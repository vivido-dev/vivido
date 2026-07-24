# Installing Vivido

Vivido requires Rust 1.88 or newer, FFmpeg (`avcodec`, `avutil`, and `swresample`), and a Vulkan,
Metal, or DirectX 12 adapter supported by wgpu. Audio output uses CPAL's system-default CoreAudio,
ALSA, or WASAPI device.

## Build from source

```sh
cargo build --release
```

The binaries are written to `target/release/vivido` and `target/release/vvssh` (with an `.exe`
suffix on Windows).

To install from the repository root with Cargo:

```sh
cargo install --path .
```

This installs both `vivido` and its `vvssh` companion command. Do not select only the `vivido`
binary with `--bin` if remote Vivid forwarding is needed.

### Linux

Linux builds are Wayland-only. Install the development packages for Wayland, xkbcommon,
fontconfig, FreeType, CMake, and pkg-config. For example, on Debian or Ubuntu:

```sh
sudo apt install cmake g++ pkg-config libfontconfig1-dev libfreetype6-dev \
  libwayland-dev libxkbcommon-dev
cargo build --release
```

There is intentionally no X11 feature or fallback. Run Vivido inside a Wayland session.

### macOS

```sh
make app
```

This creates `target/release/osx/Vivido.app`. Use `make app-universal` when both Apple Silicon
and Intel Rust targets are installed.

### Windows

The signed Windows suite installer writes the user configuration to
`%USERPROFILE%\vivido\vivido.toml`. Vivido still reads the former
`%APPDATA%\vivido\vivido.toml` location when the new path does not exist.
The consumer EXE also installs PowerShell 7 LTS when absent and provisions WSL with Ubuntu; the
standalone MSI is intended for managed systems where those prerequisites already exist. See
[`windows/README.md`](windows/README.md) for signing, packaging, silent-install, and release steps.
See [`windows/LOCAL-TESTING.md`](windows/LOCAL-TESTING.md) for a complete local Windows build and
installation-verification procedure.

Build from a Visual Studio Developer Command Prompt with the Rust MSVC toolchain:

```powershell
$env:VCPKG_ROOT = "C:\path\to\vcpkg"
$env:VCPKG_DEFAULT_TRIPLET = "x64-windows"
vcpkg install --triplet x64-windows --x-manifest-root="$PWD\windows"
$env:PATH = "$env:VCPKG_ROOT\installed\x64-windows\bin;$env:PATH"
cargo build --release
```

Vivido discovers the FFmpeg import libraries, including `swresample.lib`, from `VCPKG_ROOT` and the
selected vcpkg triplet. The `PATH` entry is also required when running Vivido because the
`x64-windows` triplet supplies FFmpeg as DLLs. WASAPI support is provided by CPAL and needs no
PulseAudio installation.

## Tests

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

The CI matrix performs these checks on Linux, macOS, and Windows.
