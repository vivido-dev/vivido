# Playing media from WSL in Vivido or Vivida on Windows

If a Linux build of `vivi` reports `Connection refused (os error 111)` inside a WSL pane,
check whether WSL 2 is using NAT. Vivido and Vivida listen for Vivid connections on Windows
loopback and pass the address to the pane through `VIVID_ENDPOINT_CONTROL`. Under NAT,
`127.0.0.1` inside WSL reaches the Linux VM. Mirrored networking lets the same address reach
the Windows listener, so you can use the Linux binary you already built.

## Confirm the cause

Run these commands in the WSL pane where playback failed:

```sh
wslinfo --networking-mode
printenv VIVID_ENDPOINT_CONTROL
```

If the mode is `nat` and the endpoint looks like `tcp:127.0.0.1:51055`, check the listener
in **Windows PowerShell**, using the port printed in your pane. Each pane can have a different
port.

```powershell
Get-NetTCPConnection -State Listen -LocalPort 51055 |
    Select-Object LocalAddress, LocalPort, OwningProcess
```

A listener on `127.0.0.1` at that port confirms the Windows side is listening. If
`printenv` produced no output, open a new WSL pane from Vivido or Vivida so the shell
inherits its endpoint.

## Enable mirrored networking

The Windows installer creates `%USERPROFILE%\.wslconfig` with mirrored networking if the
file is missing. If you already have a configuration or installed an earlier release, you can
enable the setting manually.

1. On Windows 11 22H2 or later, open your user-level WSL configuration from **Windows
   PowerShell**. Create the file if prompted.

   ```powershell
   notepad.exe "$env:USERPROFILE\.wslconfig"
   ```

2. Set `networkingMode` under `[wsl2]` and save the file. If the section already exists,
   edit it in place and preserve your other settings.

   ```ini
   [wsl2]
   networkingMode=mirrored
   ```

3. Save your work in **all running WSL distributions**. Run the following in **Windows
   PowerShell** to stop them and apply the setting on the next launch.

   ```powershell
   wsl --shutdown
   ```

4. Open a **new WSL pane in Vivido or Vivida** and run `wslinfo --networking-mode`.
   Once it prints `mirrored`, retry playback. For the sample video, run this from the
   `vivi/` checkout:

   ```sh
   ./target/release/vivi ../medias/test_pattern.mp4
   ```

For platform requirements and configuration details, see Microsoft's guides to
[mirrored networking](https://learn.microsoft.com/en-us/windows/wsl/networking#mirrored-mode-networking)
and [WSL configuration](https://learn.microsoft.com/en-us/windows/wsl/wsl-config).

## Install Vivido terminfo with `tic`

If Vim, less, or tmux reports `'vivido': unknown terminal type`, install the terminal
description once in each WSL distribution you use. A Vivido or Vivida pane sets
`TERM=vivido`, and Linux applications look up that name in the distribution's terminfo database.

From the `vivido/` checkout **inside WSL**, compile both entries for all users. On Ubuntu
or Debian, install `ncurses-bin` with `sudo apt install ncurses-bin` if `tic` is missing.

```sh
sudo tic -x -e vivido,vivido-direct -o /usr/share/terminfo extra/vivido.info
```

Verify that both names resolve, then restart the affected application:

```sh
infocmp vivido >/dev/null && infocmp vivido-direct >/dev/null && echo "Vivido terminfo installed"
```

If you installed Vivido without a source checkout, the
[Windows guide](windows.md#install-the-entry-once-per-wsl-distribution) includes the download command.
