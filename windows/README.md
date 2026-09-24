# Vivido Windows resources

This directory holds the Windows-specific inputs that Vivido itself and the suite installer consume.
The suite installer is built from the parent repository's `installer/` directory; see
`installer/README.md` there for build, signing, and release steps.

- `vivido.rc`, `vivido.manifest`, `vivido.ico`: resources embedded into `vivido.exe` by
  `build/windows.rs`. The installer also uses `vivido.ico` for its shortcuts and bundle.
- `vcpkg.json`: native dependencies for a source build (see `../INSTALL.md`). It selects the DirectX
  Shader Compiler plus only FFmpeg `avcodec`, `avformat`, `dav1d`, `swresample`, and `swscale`
  with default features disabled. Do not add `gpl`, `all-gpl`, `nonfree`, or `fdk-aac` features.
- `setup-helper/`: the signed helper the installer runs to seed the default configuration and to
  refuse upgrade or uninstall while a vvmux session is live.
- `default-vivido.toml`: the configuration the setup helper writes on first install.
- `wix/license.rtf`: the license shown by the installer UI.

## Configuration ownership and upgrades

Vivido looks first for `%USERPROFILE%\.config\vivido\vivido.toml`, then for the installer-managed
`%USERPROFILE%\vivido\vivido.toml`. On a first install the setup helper:

1. leaves an existing dot-config or installer-managed TOML untouched; or
2. writes `default-vivido.toml` to the installer-managed path when none exists.

The config is user-owned rather than an MSI file component, so repair, major upgrade, and uninstall
do not remove or overwrite it. Uninstall and upgrade also call `vvmux list` and refuse to continue
while a live session exists. The user must run `vvmux kill-session -t NAME`; setup never kills a
session silently.
