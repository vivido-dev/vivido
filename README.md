# Vivido

Vivido is a fast, cross-platform terminal emulator. It uses Vello and
wgpu for GPU rendering, targeting Metal on macOS, DirectX 12 on Windows, and Vulkan on Linux.
Linux uses Wayland exclusively.


## Vivid Protocol

Vivido accepts Vivid Protocol 1.1 only while retaining the framing-1.0 connection preface. Its private
per-window service supports authenticated marker-v2 anchors, raw/zstd raster with straight or
premultiplied alpha, retained PNG/JPEG images, portable video access units, visibility events,
pause/flush, keyframe recovery, and source-scoped failure. Local peer origin and the per-window token
are verified before session resources are created.

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
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Compatibility

Vivido continues to use the `vivido` terminfo entry when it is installed, falling back to
`xterm-256color`. This is an application compatibility detail; configuration and IPC paths use
the `vivido` name.

Plain SSH does not forward the per-window Vivid media endpoint. Use the bundled `vvssh` wrapper to
display remote images or video with Vivi; see
[Running Vivi over SSH from Vivido](../docs/vivi-over-ssh.md).

## Deliberate differences from Alacritty

Vivido is derived from Alacritty, but differ significantly from Alacritty:
- Linux has no X11, Xlib, XCB, or GLX backend. The `wayland` feature is the only Unix desktop
  backend and is enabled by default.
- Vi mode, vi search, vi cursor actions, and vi-specific configuration are removed.
- In mouse selection, semantic, whole-line, double/triple-click, and right-click expansion are removed.
- The Vivid protocol renders raster and decoded video media between cell backgrounds and glyphs.
  Protocol-neutral placeholders remain available for future escape-sequence media decoders.

## License

Vivido is released under Apache-2.0 license. See [LICENSE](LICENSE).
