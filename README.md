# Vivido

Vivido is a fast, cross-platform terminal emulator. It uses Vello and
wgpu for GPU rendering, targeting Metal on macOS, DirectX 12 on Windows, and Vulkan on Linux.
Headed Linux sessions use Wayland.

On Windows and Linux, every headed Vivido window includes an integrated tab strip. Use
`Ctrl+Shift+T` to create a tab, `Ctrl+Shift+W` to close the active tab, `Ctrl+Tab` and
`Ctrl+Shift+Tab` to move through tabs, and `Alt+1`…`Alt+9` to select by position. macOS retains its
native window tabs and Command-key shortcuts. `CreateNewWindow` and IPC `create_window` add a tab
to the current process on Windows/Linux; `SpawnNewInstance` explicitly starts another top-level
window.

## Vivid Protocol

Vivido accepts Vivid Protocol 1.5 only. A window presents exactly one presentation target for its
lifetime, chosen with `--vivid-target`:

- `terminal` (default) is `terminal-surface-v1`: a grid of cells with a text plane and anchors. It
  negotiates `vivid-core-control-v1`, `live-media-v1`, `timed-media-v1`, `observability-v1`, and
  `web-carrier-v1`.
- `desktop` is `desktop-surface-v1`: a virtual desktop in logical pixels, with no grid, no anchors,
  and no shell. It negotiates the same set plus `desktop-input-v1`, and rejects
  `terminal-content-v1` surfaces.

Across both targets Vivido implements core control, generic/terminal/desktop content surfaces,
immutable live and timed media tracks, observability, authenticated track channels, absolute flow
limits, marker-v3 anchors, session leases with activation-retry and suspend/resume, and the
authenticated interactive lane that carries desktop input independently of bulk media flow.
Canvas, multiplexed-carrier, and auxiliary-slot profiles are rejected. Transcript-bound root
authentication is verified before session resources are created.

Portable video includes H.264/HEVC Annex B, VP9 frames, and AV1 low-overhead temporal units.
Portable audio includes MP3, AAC, ALAC, PCM, Opus, Vorbis, and FLAC. Opus, Vorbis, and FLAC require
the canonical container-independent initialization defined by Vivid 1.5; Vivido validates it before
decoder or device allocation and applies trim/pre-skip exactly once.

`PLAY` retains all existing protocol fields, starts at the exact requested PTS after its minimum
buffer (or EOS-shortened pre-roll), and uses linked audio as the video master clock. Presentation
queues are track-scoped: slow or exhausted video cannot block audio, terminal rendering, or
control traffic. Live control handling is full duplex and immediately answers a valid inbound
`PING` with its correlated `PONG`.

## Build

Vivido requires Rust 1.88 or newer.

```sh
cargo build --release
```

On Linux, install the Wayland, Vulkan, font discovery, input, audio, and FFmpeg development
libraries; see [Installing and setting up Vivido on Linux](docs/linux.md). On Windows, install
FFmpeg with vcpkg and keep its DLL directory on `PATH`; see [Installing Vivido](INSTALL.md#windows).
The resulting executables are `target/release/vivido` and `target/release/vvssh`.

Useful verification commands:

```sh
cargo check --workspace --all-targets
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --test headless -- --ignored --test-threads=1  # test ignored
```

## Agent automation

`vivido msg` exposes the version-2 owner-only IPC service on Linux, macOS, and Windows: terminal
input, window and process control, deterministic discovery, structured grid snapshots, state waits,
screenshots, sanitized PTY transcripts, Vivid 1.5 session/surface/track inspection, and replayable
event subscriptions. Start with `vivido msg capabilities` and `vivido msg list-windows`. The
complete CLI and JSON wire contract is documented in [Agent automation IPC](docs/ipc.md).

## Accessibility

Native terminal windows on macOS expose their active buffer, caret, selection, and text geometry.
The text document contains retained scrollback plus the current screen; the on-screen viewport is
reported separately as the visible range. SGR-hidden text is reported as spaces, and all editing
accessibility actions are intentionally unsupported. Windows and Linux expose the lightweight tab
list and window controls, but omit the history-sized terminal document so scrolling cost remains
independent of retained scrollback. Desktop presentation targets expose their native
application/window roles without a terminal text area; headless targets leave accessibility
ownership to their host.

On macOS, a live window's `AXVisibleCharacterRange` can be inspected with:

```sh
cargo run --example ax_visible_character_range -- "$(pgrep -n vivido)"
```

The terminal running the probe must have permission under **System Settings → Privacy & Security →
Accessibility**.

## Headless sessions

`vivido --headless` runs the whole runtime — terminal, Vivid presenter, and GPU renderer — with no
window and no compositor, serving IPC in the background:

```sh
eval "$(vivido --headless --session build)"
vivido msg get-text
vivido msg screenshot
vivido kill-session --target build
```

Sessions are named (`--session`), discoverable (`vivido list`), addressable (`vivido msg --target`),
and safe against PID recycling. `--foreground` keeps the instance attached to the calling terminal,
and `--headless-size` fixes the geometry in cells or pixels. See
[Headless Vivido and named sessions](docs/headless.md).

## Compatibility

Vivido continues to use the `vivido` terminfo entry when it is installed, falling back to
`xterm-256color`. This is an application compatibility detail; configuration and IPC paths use
the `vivido` name. `vvssh` performs the same check on the remote host before starting its login
shell, since SSH forwards the terminal name but not Vivido's locally materialized terminfo entry.

Plain SSH does not forward the per-window Vivid media endpoint. Use the bundled `vvssh` wrapper to
display remote images or video with Vivi; see
[Running Vivi over SSH from Vivido](../docs/vivi-over-ssh.md).

By default, `vvssh user@host` keeps media on an independent lifecycle-bound SSH transport so video
backpressure cannot stall the terminal and control connection. To restore the legacy shared path
for diagnosis, use:

```sh
vvssh --shared-media-transport user@host
```

The media helper exports its private socket as both `VIVID_ENDPOINT_REALTIME` and
`VIVID_ENDPOINT_BULK`, avoids OpenSSH control-master reuse, and is cleaned up with the main
session. When password authentication is needed, `vvssh` asks once and reuses the answer through
an owner-authenticated local broker for only the SSH processes belonging to that invocation;
one-time verification-code prompts are never reused. `VIVID_ROOT_SECRET` travels only through the
protected temporary-file setup channel, never in command arguments or logs.

When the remote Vivi installation includes `vvreceive`, `vvssh` starts it quietly and waits only
until it has recorded the login-shell PID identity before executing the shell. Dropping a local
regular file copies it over Vivid's authenticated bulk connection into that shell's current
directory; one drop transfers, with the hover label naming the destination beforehand. Pasting
(`Ctrl+Shift+V`) does the same when the clipboard holds a copied file or an image, which is
re-encoded as PNG under a `pasted-image-*` name. If the helper is absent, or `--no-receive-drops`
is passed, local filename paste remains unchanged.

The 1.1 wire protocol and the version-1 automation interface are intentionally not supported. See
the [Vivido 1.1 to 1.5 migration guide](../docs/vivido-protocol-1.1-to-1.5-migration.md).

## Deliberate differences from Alacritty

Vivido is derived from Alacritty, but differs significantly from Alacritty:

- Rendering has no Xlib, XCB, or GLX surface path; wgpu's Vulkan backend owns every GPU surface.
  Winit still carries both Unix backends, Wayland preferred and X11 (`x11` feature, on by default)
  as a fallback; on X11 the text and primary-selection clipboards run through arboard.
- Vi mode, vi search, vi cursor actions, and vi-specific configuration are removed.
- In mouse selection, semantic, whole-line, double/triple-click, and right-click expansion are
  removed.
- The Vivid protocol renders raster and decoded video media between cell backgrounds and glyphs.
  Protocol-neutral placeholders remain available for future escape-sequence media decoders.

## License

Vivido is released under Apache-2.0 license. See [LICENSE](LICENSE).
