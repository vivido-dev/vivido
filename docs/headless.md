# Headless Vivido and named sessions

`vivido --headless` runs the complete Vivido runtime — terminal core, input encoders, Vivid 1.5
presenter, and the Vello/wgpu renderer — with no window and no compositor. It is the deterministic
mode for CI, agents, and remote automation: everything `vivido msg` can observe in a windowed
instance is observable headless, including real rendered screenshots.

Headless mode is supported on Linux, macOS, and Windows.

## Starting a session

```sh
vivido --headless
```

By default the process detaches, and the parent prints a shell-evaluable endpoint before exiting:

```text
VIVIDO_SOCKET=/run/user/1000/vivido/session-<hash>.sock; export VIVIDO_SOCKET
VIVIDO_SESSION=vivido-48213; export VIVIDO_SESSION
```

```sh
eval "$(vivido --headless --session build)"
vivido msg get-text
```

The parent does not exit until the daemon reports that it is actually serving, so a zero exit status
means the session is usable. Startup failure is reported as one bounded diagnostic line and a
nonzero exit — `vivido --headless` never exits 0 leaving a session that never came up. If the daemon
neither succeeds nor fails within 30 seconds, the parent reports a timeout and leaves the daemon
running rather than killing a session that may still be initializing a software renderer.

A headless session always starts with exactly one window; it then outlives that window and every
later one, and stops only on `vivido msg quit`, `vivido kill-session`, or a termination signal.

### Options

| Option | Meaning |
|---|---|
| `--headless` | Run with no window and no compositor, serving IPC. |
| `--session NAME` | Name the session. Default is `vivido-<pid>`, which is unique by construction. Requires `--headless`. |
| `--foreground` | Do not detach; block in this terminal until shutdown. Requires `--headless`. |
| `--headless-size SIZE` | Initial geometry, as `COLUMNSxLINES` or `WIDTHxHEIGHTpx`. Requires `--headless`. |
| `-s`, `--socket PATH` | Not needed headless: the session name determines the endpoint. |

`--foreground` is the shape to use under a supervisor, a container entrypoint, or a test harness
that wants the child's lifetime to be the session's lifetime. It prints the same two `export` lines
on standard output once the session is serving.

A session name is 1–64 ASCII letters, digits, `.`, `-`, or `_`, and may not start with `.`. The name
becomes part of a filename and a pipe name, so anything that could escape the runtime directory is
rejected before any file is created.

### Size

`--headless-size 120x40` goes through the ordinary `window.dimensions` configuration path, so the
pixel size is derived from real font metrics exactly as it would be for a window.
`--headless-size 1280x720px` sets the render surface directly, which is usually what a caller
driving screenshots wants. With neither, a configured `window.dimensions` still applies; the final
fallback is 1280x720 physical pixels at scale factor 1.0.

Headless windows can still be resized at runtime with `vivido msg resize`, in either grid or pixel
units.

## Managing sessions

```sh
vivido list
vivido kill-session --target build
```

`vivido list` prints one live session per line as `NAME`, `pid N`, `COLUMNSxLINES`, and the endpoint,
tab-separated. Listing also reaps rendezvous files whose owning process is gone, so a crashed daemon
never leaves a session that appears live.

`vivido kill-session --target NAME` is the forceful path. Prefer `vivido msg quit`, which asks the
daemon to shut down and lets it clean up its own socket, registry, and log file.

## Reaching a session

Every window-level command is `vivido msg`. It picks an endpoint in this order:

1. `--socket PATH` (a filesystem path on Unix, a named-pipe path on Windows);
2. `--target NAME`, else inherited `VIVIDO_SESSION` — an explicitly named session never silently
   falls through to a different instance, so a name that is not running is an error;
3. inherited `VIVIDO_SOCKET`, if it still connects;
4. the only live headless session, when there is exactly one;
5. on Unix, the newest live windowed instance on the current display.

```sh
vivido msg --target build get-text
vivido msg --target build screenshot
vivido kill-session --target build
```

`vivido msg capabilities` reports `"headless": true` and `"session": "<name>"` for a headless
instance, so a client can distinguish a windowed instance from a windowless one without inferring it
from a failed `focus`. See [Agent automation IPC](ipc.md) for the full method and JSON contract.

## What differs from a windowed instance

- There is no compositor and no operating-system focus. `focus` cannot succeed, and commands that
  fall back to "the focused window" have no focus to fall back to; pass `--window-id`, or rely on
  the single-window case.
- Rendering is offscreen. `screenshot` and `wait frame` work normally and reflect real GPU
  presentation, using the platform backend (Vulkan, Metal, or DirectX 12) with a software adapter as
  the fallback when no hardware adapter exists.
- Scale factor is 1.0, because there is no monitor to ask.
- Nested-launch environment is scrubbed: `VIVID_ENDPOINT_CONTROL`, `VIVID_ENDPOINT_BULK`,
  `VIVID_ROOT_SECRET`, `VIVID_REMOTE`, `TMUX`, `TMUX_PANE`, `STY`, `VIVIDO_SOCKET`, and
  `VIVIDO_SESSION` are removed before the daemon starts, so a headless session launched from inside
  another Vivid session or a multiplexer does not hand its child shell the wrong producer
  credentials.

## Rendezvous and ownership

Each session owns two artifacts:

| Platform | Endpoint | Registry |
|---|---|---|
| Linux, macOS | `$XDG_RUNTIME_DIR/vivido/session-<hash>.sock`, else `/tmp/vivido-<uid>/…`, mode `0600` | `session-<hash>.json`, mode `0600` |
| Windows | `\\.\pipe\vivido-session-<hash>` with an owner-and-SYSTEM-only DACL | `%LOCALAPPDATA%\vivido\sessions\session-<hash>.json` |

`<hash>` is derived from the session name, so a name maps to exactly one endpoint. The registry
records the schema, name, PID, instance nonce, Vivido version, automation protocol version, an
endpoint identity derived from the endpoint path, the owning process's **birth time**, and the
startup grid size. It is bounded to 16 KiB and written atomically through a uniquely named temporary
file.

The registry is rendezvous metadata, not an authorization boundary: authorization is the Unix socket
mode plus peer-credential check, or the named-pipe DACL plus SID verification on both peers. On
Unix, a registry that is not a regular owner-owned file with no group or world bits is refused.

Process birth time is what makes liveness and teardown safe. A registry file is not proof that its
PID still names the daemon that wrote it, so every liveness check compares birth time, every
deletion compares the complete recorded instance, and `kill-session` re-verifies birth time through
a pinned process handle (`pidfd` on Linux, `OpenProcess` on Windows) immediately before signalling.
A recycled PID can therefore never be signalled or have another session's files removed.

`VIVIDO_RUNTIME_DIR` overrides the Windows registry root, which is useful for hermetic tests and
service accounts. It does not weaken the pipe DACL.

## Platform notes

Linux forks before any thread starts, so the daemon keeps the options the parent already parsed.
macOS and Windows instead re-execute with an inherited readiness handle: macOS must initialize Metal
in a fresh process, while Windows has no `fork` and also uses detached-process creation flags. The
internal `--__headless-server-handle` and `--__resolved-session` flags exist only for that re-exec
and are not part of the public CLI.

The implementation record and security checklist for the Windows port is
[headless-windows-plan.md](headless-windows-plan.md).

## Verifying

The end-to-end suite is `tests/headless.rs`. It drives the real binary through `vivido msg` and
asserts on real rendered PNGs, so it is opt-in:

```sh
cargo test --test headless -- --ignored --test-threads=1
```
