# Vivido

Vivido is a fast, cross-platform terminal emulator. It uses Vello and
wgpu for GPU rendering, targeting Metal on macOS, DirectX 12 on Windows, and Vulkan on Linux.
Linux uses Wayland exclusively.

## Vivid Protocol

Vivido accepts Vivid Protocol 1.1 only while retaining the framing-1.0 connection preface. Its
private per-window service supports authenticated marker-v2 anchors, raw/zstd raster with straight
or premultiplied alpha, retained PNG/JPEG images, portable video and audio access units, visibility
events, complete buffered `PLAY`, pause/flush/EOS, keyframe recovery, and source-scoped failure.
Local peer origin and the per-window token are verified before session resources are created.

Portable video includes H.264/HEVC Annex B, VP9 frames, and AV1 low-overhead temporal units.
Portable audio includes MP3, AAC, ALAC, PCM, Opus, Vorbis, and FLAC. Opus, Vorbis, and FLAC require
the canonical container-independent initialization defined by Vivid 1.1; Vivido validates it before
decoder or device allocation and applies trim/pre-skip exactly once.

`PLAY` retains all existing protocol fields, starts at the exact requested PTS after its minimum
buffer (or EOS-shortened pre-roll), and uses linked audio as the video master clock. Presentation
queues are source-scoped: slow or exhausted video cannot block audio, terminal rendering, or
control traffic. Live control handling is full duplex and immediately answers a valid inbound
`PING` with its correlated `PONG`.

## Build

Vivido requires Rust 1.88 or newer.

```sh
cargo build --release
```

On Linux, install the Wayland, Vulkan, font discovery, input, and FFmpeg development libraries. On
Windows, install FFmpeg with vcpkg and keep its DLL directory on `PATH`; see
[Installing Vivido](INSTALL.md#windows).
The resulting executables are `target/release/vivido` and `target/release/vvssh`.

Useful verification commands:

```sh
cargo check --workspace --all-targets
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

## Compatibility

Vivido continues to use the `vivido` terminfo entry when it is installed, falling back to
`xterm-256color`. This is an application compatibility detail; configuration and IPC paths use
the `vivido` name.

Plain SSH does not forward the per-window Vivid media endpoint. Use the bundled `vvssh` wrapper to
display remote images or video with Vivi; see
[Running Vivi over SSH from Vivido](../docs/vivi-over-ssh.md).

By default, `vvssh user@host` carries control and media over one private SSH transport. For an
independent media path, use:

```sh
vvssh --separate-media-transport user@host
```

The opt-in helper creates a distinct lifecycle-bound SSH TCP connection and private remote socket,
exports it as `VIVID_ENDPOINT_BULK`, avoids OpenSSH control-master reuse for that helper, and cleans
up the process and socket with the main session. The Vivid token still travels only through the
protected setup channel, never in command arguments.

## Deliberate differences from Alacritty

Vivido is derived from Alacritty, but differs significantly from Alacritty:

- Linux has no X11, Xlib, XCB, or GLX backend. The `wayland` feature is the only Unix desktop
  backend and is enabled by default.
- Vi mode, vi search, vi cursor actions, and vi-specific configuration are removed.
- In mouse selection, semantic, whole-line, double/triple-click, and right-click expansion are
  removed.
- The Vivid protocol renders raster and decoded video media between cell backgrounds and glyphs.
  Protocol-neutral placeholders remain available for future escape-sequence media decoders.

## License

Vivido is released under Apache-2.0 license. See [LICENSE](LICENSE).
