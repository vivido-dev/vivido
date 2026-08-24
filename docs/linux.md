# Installing and setting up Vivido on Linux

Vivido does not currently provide a Linux package or installer. Install the executables with
Cargo, then install the system integration files from the source checkout. Keep that checkout until
the post-installation steps are complete.

Vivido requires Rust 1.88 or newer, a Wayland session, FFmpeg development libraries, ALSA
development libraries, and a Vulkan-capable driver. It has no supported X11 fallback.

## 1. Install build and runtime dependencies

On Debian or Ubuntu:

```sh
sudo apt update
sudo apt install build-essential cmake git pkg-config ncurses-bin scdoc \
  libasound2-dev libfontconfig1-dev libfreetype6-dev \
  libwayland-dev libxkbcommon-dev \
  libavcodec-dev libavutil-dev libswscale-dev libswresample-dev \
  libvulkan1 mesa-vulkan-drivers
```

Use the equivalent packages on other distributions. A vendor Vulkan driver can replace Mesa's
Vulkan driver where appropriate. `scdoc` is needed only to build the optional manual pages.

Install Rust with [rustup](https://rustup.rs/) if the distribution's Rust compiler is older than
1.88, then confirm that Cargo is available:

```sh
rustc --version
cargo --version
```

Vivido opens headed windows through Wayland. Before launching it, confirm that the current desktop
session exports a Wayland display:

```sh
test -n "$WAYLAND_DISPLAY" && printf 'Wayland display: %s\n' "$WAYLAND_DISPLAY"
```

## 2. Install the executables with Cargo

Clone the repository and install both `vivido` and its `vvssh` companion:

```sh
git clone https://github.com/vivido-dev/vivido.git
cd vivido
cargo install --locked --path .
```

Cargo installs the executables in `~/.cargo/bin` by default. Rustup normally adds that directory to
`PATH`; if your shell cannot find Vivido, add it explicitly in the shell's startup file:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

Verify the installation:

```sh
vivido --version
vvssh --help
```

To replace an existing Cargo installation with a newer checkout, update the checkout and reinstall:

```sh
git pull --ff-only
cargo install --locked --force --path .
```

## 3. Install the terminfo entries system-wide

This step is required for complete terminal compatibility. Vivido can provide a private temporary
terminfo entry to ordinary child processes, but environment boundaries such as `sudo` discard the
private `TERMINFO` path while preserving `TERM=vivido`. Programs using ncurses or a pager can then
report that the terminal is not fully functional.

From the repository checkout, compile both Vivido entries into the system terminfo database:

```sh
sudo tic -x -e vivido,vivido-direct -o /usr/share/terminfo extra/vivido.info
```

Verify that the entry is available both as the current user and through `sudo`:

```sh
infocmp vivido >/dev/null
sudo infocmp vivido >/dev/null
```

Both commands should exit successfully. New shells opened by Vivido use `TERM=vivido` by default.

## 4. Add Vivido to the desktop application menu

Cargo installs executables only. Install the supplied desktop entry and icon for the current user:

```sh
install -Dm644 extra/linux/Vivido.desktop \
  "$HOME/.local/share/applications/Vivido.desktop"
install -Dm644 extra/logo/vivido-term.svg \
  "$HOME/.local/share/icons/hicolor/scalable/apps/Vivido.svg"
```

If `update-desktop-database` and `gtk-update-icon-cache` are installed, refresh their caches:

```sh
update-desktop-database "$HOME/.local/share/applications"
gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor"
```

The desktop entry runs `vivido`, so the graphical session must also have `~/.cargo/bin` in its
`PATH`. Log out and back in after changing a login environment.

## 5. Create a configuration file

Vivido works without a configuration file. To start from the documented defaults:

```sh
install -Dm644 docs/vivido.toml "$HOME/.config/vivido/vivido.toml"
```

Edit that file as needed. See the [configuration guide](configuration.md) for the supported fields
and complete configuration search order.

## 6. Optional shell completions and manual pages

Install the completion matching the current shell.

For Bash:

```sh
install -Dm644 extra/completions/vivido.bash \
  "$HOME/.local/share/bash-completion/completions/vivido"
```

For Fish:

```sh
install -Dm644 extra/completions/vivido.fish \
  "$HOME/.config/fish/completions/vivido.fish"
```

For Zsh, install `_vivido` into a directory already present in `fpath`:

```sh
install -Dm644 extra/completions/_vivido \
  "$HOME/.local/share/zsh/site-functions/_vivido"
```

To build and install the supplied manual pages system-wide:

```sh
manual_dir=$(mktemp -d)
for page in extra/man/*.scd; do
  section=${page%.scd}
  section=${section##*.}
  name=${page##*/}
  name=${name%.scd}
  scdoc < "$page" | gzip -9 > "$manual_dir/$name.gz"
  sudo install -Dm644 "$manual_dir/$name.gz" "/usr/local/share/man/man$section/$name.gz"
done
rm -r "$manual_dir"
```

Refresh the manual-page index when the distribution provides `mandb`:

```sh
sudo mandb
```

## Troubleshooting

### `WARNING: terminal is not fully functional` after `sudo`

Install the system terminfo entries from step 3. As a temporary workaround for one command, use a
widely installed terminal definition:

```sh
sudo TERM=xterm-256color apt search sshd
```

### Vivido cannot open a window

Confirm that `WAYLAND_DISPLAY` is set and that the session has a working Vulkan driver. Vivido does
not support an X11-only session.

### The linker cannot find FFmpeg or ALSA

Install the development packages, not only the runtime libraries. `pkg-config` must be able to find
`libavcodec`, `libavutil`, `libswscale`, `libswresample`, and `alsa` before Cargo builds Vivido.
