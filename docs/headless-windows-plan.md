# Headless mode on Windows — remaining work

Status as of commit `13c7f75` (`feat(vivido): add a true headless mode`).

`vivido --headless` is implemented and verified on Linux. macOS is *structured* for but not built
or run. **Windows is not implemented at all**, and this document is the handoff for that work.

It was deferred for one reason: the development host had no Windows toolchain, so the code could
not be compiled or even type-checked (`cargo check --target x86_64-pc-windows-gnu` fails in the C
build with `failed to find tool "x86_64-w64-mingw32-gcc"`). Writing an owner-only named-pipe IPC
transport plus ~250 `cfg` changes with no way to build them would have produced a large, unverified
diff over a working Linux path. Nothing about the design is blocked — only the verification.

**Before starting, confirm you can actually build for Windows.** Either work on a Windows machine,
or install `mingw-w64` (plus a Windows FFmpeg, see "Build prerequisites"). If `cargo check
--target …-windows-…` does not succeed on the *unmodified* tree first, stop and fix that before
writing any code.

---

## 1. What already exists and does not need redoing

The headless architecture itself is platform-neutral. These landed in `13c7f75` and are already
correct for Windows:

| Piece | Where | Windows status |
|---|---|---|
| `RenderSource::{Surface, Offscreen}` and the surfaceless `SceneRenderer` | `src/display/renderer.rs` | Portable. `offscreen_device()` uses plain wgpu; DX12 is already the Windows backend (`Cargo.toml`), and `force_fallback_adapter` gives WARP as the software fallback. |
| `Window` `Backend::{Winit, Headless}` | `src/display/window.rs` | Portable. No `cfg` in it. |
| `EventSink`, `LoopHandle`, `HeadlessLoop`, `Processor::{new_headless, start_headless, run_headless}` | `src/event.rs` | Portable. No `cfg` in these items. |
| NDJSON protocol, framing, limits, `hello`/`quit` | `src/polling/ipc.rs` | Portable **except** the transport type (see §3). The file has zero internal `cfg(unix)`. |
| Session registry logic, `RegistryGuard`, name validation, `same_instance` teardown rule | `src/session.rs` | Logic portable; three concrete spots need Windows equivalents (§4). |
| Readiness protocol (`OK\n<socket>\n<session>\n` / `ERR\n<diagnostic>\n`), 30 s timeout, daemon left running on timeout | `src/headless.rs` | Protocol portable; the fork and fd mechanics are not (§5). |

Read `~/.claude/plans/brainstorm-to-add-a-kind-stearns.md` for the original design rationale, and
the `13c7f75` commit message for what changed and why.

---

## 2. Scope of the `cfg` un-gating

Today the entire automation surface is Unix-only, gated at module level in `src/main.rs`:

```rust
#[cfg(unix)] mod automation;
#[cfg(unix)] mod headless;
#[cfg(unix)] mod polling;
#[cfg(unix)] mod screenshot;
#[cfg(unix)] mod session;
#[cfg(unix)] use crate::cli::{MessageOptions, Subcommands};
#[cfg(unix)] use crate::polling::{IoListener, ipc};
```

`cfg(unix)`-family occurrences per file, as a sizing estimate:

| File | Count | Character of the work |
|---|---|---|
| `src/window_context.rs` | 107 | Mostly automation methods (`ui_paste`, `ui_key`, `ui_mouse`, waiters, screenshots). Mechanical. |
| `src/event.rs` | 77 | IPC handlers and automation emission. Mechanical. |
| `src/cli.rs` | 60 | `msg` subcommand, `--headless`, `--socket`, `HeadlessSize`, session verbs. Mechanical. |
| `src/display/renderer.rs` | 16 | `ScreenshotError` variants, `ScreenshotReadback`, `begin_screenshot`, `poll_screenshot`, `has_rendered_frame`. Pure wgpu, no OS calls — just delete the gates. |
| `src/main.rs` | 14 | Module declarations above. |
| `src/display/mod.rs` | 8 | Screenshot pass-throughs. |

**Do this incrementally and keep Linux green at every step.** The largest risk in this task is not
Windows — it is silently regressing the working Unix path while touching 250 gates. Run
`cargo test --workspace --all-targets` on Linux after each file.

`src/screenshot.rs` is nearly portable; it uses `PermissionsExt::mode(0o600)` at `src/screenshot.rs:24`
to create the PNG owner-only. On Windows, files in the per-user temp directory are already
user-scoped; either `cfg` that line out or set an explicit DACL. Do not silently drop the
restriction without a comment saying why it is safe.

---

## 3. The IPC transport (the real work)

### 3.1 Decision: named pipes, not AF_UNIX and not loopback TCP

The plan originally floated AF_UNIX or a loopback socket. **Use named pipes.** Windows AF_UNIX
ignores file-mode ACLs, and loopback TCP is reachable by every local user — either would make an
agent-controllable terminal locally hijackable by any other account on the machine. This is not a
theoretical concern for a socket whose whole purpose is "type keystrokes into a shell and read the
screen back".

### 3.2 Reference implementation to model on

`vvmux/src/platform/windows.rs` already has a shipping, tested owner-only named-pipe transport.
Model on it rather than writing from scratch:

- `SecurityDescriptor::for_current_user()` — `vvmux/src/platform/windows.rs:1090-1112`. Builds an
  SDDL string `O:{sid}G:{sid}D:P(A;;GA;;;SY)(A;;GA;;;{sid})` — owner and SYSTEM only, with
  `D:P` protecting the DACL from inherited ACEs — and converts it via
  `ConvertStringSecurityDescriptorToSecurityDescriptorW`. `LocalFree` on drop.
- `CreateNamedPipeW` with `PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS`
  — `vvmux/src/platform/windows.rs:864-885`. `PIPE_REJECT_REMOTE_CLIENTS` is required.
- `require_pipe_client_owner` / `require_pipe_server_owner` / `require_process_owner` —
  `vvmux/src/platform/windows.rs:1194-1215`. Uses `GetNamedPipeClientProcessId` /
  `GetNamedPipeServerProcessId` then `EqualSid` against the current process token. This is the
  Windows analogue of the `SO_PEERCRED` check now in `src/polling/ipc.rs::require_peer_owner`, and
  it must be applied on **both** ends, exactly as the Unix version is.
- `windows-sys` feature list — `vvmux/Cargo.toml`. `vivido/Cargo.toml` currently enables far fewer
  features; you will need at least `Win32_Security`, `Win32_Security_Authorization`,
  `Win32_System_Pipes`, `Win32_Storage_FileSystem`, `Win32_System_IO`.

Note vvmux is a **separate workspace** with its own `Cargo.lock`. You cannot `use` its code. Copy
with attribution in a comment, or extract a shared crate — extraction is cleaner but touches two
workspaces, so it is probably a follow-up rather than part of this task.

### 3.3 Shape of the change

Add `src/polling/transport.rs` exposing a small abstraction that both platforms implement:

```rust
pub struct LocalListener { /* UnixListener | named-pipe server */ }
pub struct LocalStream   { /* UnixStream   | named-pipe handle */ }

impl LocalListener {
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self>;
    pub fn accept(&self) -> io::Result<LocalStream>;   // performs require_peer_owner
    pub fn set_nonblocking(&self, on: bool) -> io::Result<()>;
}
impl LocalStream {
    pub fn connect(endpoint: &Endpoint) -> io::Result<Self>;  // performs require_peer_owner
    pub fn try_clone(&self) -> io::Result<Self>;
    pub fn shutdown(&self) -> io::Result<()>;
}
// Read + Write on LocalStream.
```

Then replace `UnixListener`/`UnixStream` in `src/polling/ipc.rs`. The call sites are few:
`bind_socket`, `IpcListener::process_message` (the `accept`), `spawn_connection`, `find_socket`,
`connect_checked`, and `ConnectionInner::shutdown`.

**Endpoint naming differs and this leaks into the CLI.** A Unix endpoint is a filesystem path; a
Windows named pipe is `\\.\pipe\<name>` and is *not* a filesystem object. Consequences:

- `-s/--socket <PATH>` takes a pipe name on Windows. Document this in `--help` text.
- `session.rs` currently stores `socket: PathBuf` in the registry and derives the endpoint identity
  by hashing the socket path (`src/session.rs:66-71`). On Windows the registry `.json` still lives
  on disk (in `%LOCALAPPDATA%`), but the `socket` field becomes a pipe name. Keep the field, keep
  hashing it for `endpoint_id` — just do not assume it is a path that can be `stat`ed or removed.
- `SessionPaths::prepare_endpoint` (`src/session.rs:107-146`) probes a stale socket by
  `UnixStream::connect` and unlinks it. On Windows there is nothing to unlink: a pipe disappears
  when its last handle closes. That branch simplifies — a failed connect means "gone".
- `SessionPaths::remove_instance` (`src/session.rs:159-186`) removes the socket file then the
  registry. On Windows only the registry file exists to remove. Keep the revalidate-before-each-
  mutation structure; it is the whole point of that function.

### 3.4 `IoListener` is Unix-shaped

`src/polling/mod.rs` is titled "Unix I/O event polling" and multiplexes two things with the
`polling` crate: the IPC listener (`IPC_READ_KEY`) and a `SignalListener` (`SIGNAL_READ_KEY`).

`src/polling/signal.rs` is entirely Unix: `signal_hook` + `UnixStream::pair()` turning `SIGINT`/
`SIGTERM` into an `EventType::Shutdown` on the event sink.

For Windows:

- The IPC half is what you need. A named-pipe server is usually driven with overlapped I/O or a
  blocking accept thread per instance rather than the `polling` crate; the simplest correct
  approach is a dedicated accept thread, since `IpcListener::process_message` already runs on a
  background thread and hands work to the loop via `EventSink`.
- Replace the signal half with a console control handler (`SetConsoleCtrlHandler`) and/or
  `CTRL_CLOSE_EVENT`, emitting the same `EventType::Shutdown`. A headless daemon has no console,
  so the practical shutdown path on Windows is `vivido msg quit` and `vivido kill-session`, which
  is why §4 needs a working `terminate_session`.

Keep `IoListenerHandle { ipc_socket_path }` shaped the same so `src/headless.rs` does not care.

---

## 4. `src/session.rs` on Windows

Three concrete gaps.

**`ProcessBirth`.** The enum (`src/session.rs:36-39`) has `Linux { start_ticks }` and
`Macos { start_micros }`. Add `Windows { creation_time: u64 }` from `GetProcessTimes`'s
`lpCreationTime` (`FILETIME` → `u64`). This is the anti-PID-recycling mechanism and is not
optional — without it a stale registry can name an unrelated live process and `kill-session` would
signal it. `process_birth()` at `src/session.rs:465` and `src/session.rs:485` shows both existing
implementations.

**`terminate_session` / `signal_session`.** `src/session.rs:296-330` uses `pidfd_open` +
`pidfd_send_signal` on Linux specifically so a PID recycled mid-call cannot be signalled. Windows
has an equivalent guarantee available: `OpenProcess` returns a handle that pins the process object,
so open the handle, **re-verify creation time through that handle**, then `TerminateProcess`.
Prefer asking the daemon to exit over `msg quit` where possible and treat `kill-session` as the
forceful path; note in the code that `TerminateProcess` gives the daemon no chance to run
`RegistryGuard`, so `list_registries` must reap the leftover (it already does, via
`registry_process_matches`).

**Runtime directory and file-mode checks.** `runtime_root()` (`src/session.rs:519`) uses
`XDG_RUNTIME_DIR` else `/tmp/vivido-<uid>`; use `%LOCALAPPDATA%\vivido\sessions` on Windows.
`ensure_private_directory` / `safe_registry_metadata` / `read_registry_bytes`
(`src/session.rs:390-411`, `:532-570`) check `uid`, `mode & 0o077`, and `O_NOFOLLOW`. None of these map to
Windows. Replace with an ACL check (owner is the current user, no other non-SYSTEM ACEs) — or, if
you decide the per-user `%LOCALAPPDATA%` root is sufficient, say so explicitly in a comment. Do
not just delete the checks silently; the Unix version treats them as security-relevant and a reader
will assume Windows does too.

---

## 5. `src/headless.rs`: fork must become re-exec

The Linux daemon **forks** (`src/headless.rs:73`). That was a deliberate choice over vvland's
re-exec, because vivido can fork before any thread starts and thereby keep the options the parent
already parsed, with no flag serialize/parse round trip. **Windows has no `fork`**, so it must
re-exec — which means this is the one place the Linux design does not carry over.

Two options:

1. **Re-exec on Windows only.** Add a hidden `__headless-server` subcommand (vvland's
   `vvland/src/cli.rs` `#[command(name = "__server", hide = true)]` is the model) and re-serialize
   the config as flags. `vvland/src/linux/serve.rs::append_server_config` is the reference for
   doing that exhaustively. Risk: a flag that fails to round-trip silently changes daemon
   behaviour. Mitigate with a test that round-trips a fully-populated `Options`.
2. **Re-exec on all platforms**, dropping the fork. More uniform and easier to reason about, at the
   cost of reintroducing the round-trip risk on Linux where it currently does not exist. Only do
   this if you also add the round-trip test.

Recommendation: option 1, keeping the Linux fork, with the re-exec path `cfg`-gated to Windows.

The rest of `src/headless.rs` needs Windows equivalents:

| Unix mechanism | Line | Windows equivalent |
|---|---|---|
| `libc::fork` | `:73` | re-exec, per above |
| `libc::setsid` | `:99` | `DETACHED_PROCESS \| CREATE_NEW_PROCESS_GROUP` in `CreationFlags` |
| `pipe2(O_CLOEXEC)` | `:317` | `CreatePipe` with a non-inheritable read end; pass the write handle explicitly |
| `dup2` to `/dev/null` | `:346` | `NUL` device, or just `Stdio::null()` on the child `Command` |
| `libc::poll` for the readiness timeout | `:324` | `WaitForSingleObject` on the pipe handle, or an overlapped read with a timeout |

**Trap, already hit and fixed once on Linux — do not reintroduce it.** The readiness pipe *must*
be non-inheritable. The daemon goes on to spawn a shell; if that shell inherits the write end, the
parent never sees EOF and blocks until its 30 s timeout even though the session came up perfectly.
On Linux this was `pipe2(O_CLOEXEC)`; on Windows it is the `bInheritHandle` flag on
`SECURITY_ATTRIBUTES` plus `SetHandleInformation`. See `src/headless.rs:310-315` for the comment
explaining why.

---

## 6. Build prerequisites

- `build.rs:31-64` requires FFmpeg (`libavcodec`, `libavutil`, `libswscale`, `libswresample`) via
  pkg-config and **panics on non-Windows** if absent. Windows has its own path — check what it
  expects before assuming this is free.
- wgpu on Windows uses the `dx12` backend (`Cargo.toml`, `cfg(windows)` block). The software
  fallback in `offscreen_device()` resolves to WARP, which should work headless, but confirm
  `force_fallback_adapter: true` actually yields an adapter on a bare Windows Server image — that
  is the environment this feature exists for.
- `#![windows_subsystem = "windows"]` (`src/main.rs:10`) means no console is attached by default;
  `main` already calls `AttachConsole(ATTACH_PARENT_PROCESS)`. A headless parent must print
  `VIVIDO_SOCKET=…` to that attached console, so verify the startup output actually reaches the
  caller's terminal — this is easy to get silently wrong.

---

## 7. Verification

Nothing here is proven until it runs. Minimum bar, matching what Linux already passes:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo test --test headless -- --ignored --test-threads=1
```

`tests/headless.rs` is the end-to-end suite and is currently Unix-shaped in three helpers:
`set_private` (`#[cfg(unix)]`, mode 0700), the `/tmp/...` runtime paths, and the `SUN_LEN` comment
about short socket paths (irrelevant for pipes). Port those and the five tests should apply
unchanged — they drive the binary through `vivido msg` and assert on real rendered PNGs.

Keep the isolation regression (`tearing_down_one_session_leaves_the_other_untouched`). It starts
two sessions that deliberately reuse the same numeric window id — headless ids start from `1 << 63`
in every process — kills one, and asserts the survivor's grid, window, rendering and rendezvous are
intact. Root `AGENTS.md` requires this class of test for any lifecycle or teardown change, and the
Windows transport is exactly such a change.

Also add, for Windows specifically:

- A test that a second process owned by the same user can connect, and that the owner check is
  actually exercised (the Unix analogue is `the_owner_is_accepted_on_both_ends_of_a_socket` in
  `src/polling/ipc.rs`).
- The `Options` re-exec round-trip test, if you take option 1 in §5.
- A CI job. There is currently **no Linux CI for this repo at all** (`.github/workflows/` is
  Windows-only per `docs/vvland/headless-plan-audit.md`), so a Windows job would ironically be the
  better-covered platform. Do not rely on "it built locally".

---

## 8. Suggested order

1. Get an unmodified `cargo check --target <windows>` to pass. Do not skip this.
2. `src/polling/transport.rs` with both backends and the owner checks, plus its unit tests.
3. Swap `ipc.rs` onto it; keep everything still `cfg(unix)`-gated so Linux stays green.
4. Un-gate the renderer/display screenshot items (§2) — pure wgpu, no OS calls, cheapest win.
5. `src/session.rs` Windows: `ProcessBirth`, runtime root, ACL checks, `signal_session`.
6. `src/headless.rs` Windows: re-exec, detach flags, readiness pipe.
7. Un-gate `cli.rs`, `event.rs`, `window_context.rs`, `main.rs` — the bulk, mechanical, Linux green
   after each file.
8. Port `tests/headless.rs`, add the Windows-specific tests, add CI.
