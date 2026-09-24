# Vivido

**A cross-platform GPU terminal for developers and AI agents.**

Vivido is a GPU-rendered terminal emulator for macOS, Windows, and Linux. In addition to text, it
can display images, video, and audio inside the terminal window, and it provides an automation API
that lets AI agents drive, read, and observe its windows.

<img width="1915" height="1077" alt="vivida_screenshot" src="https://github.com/user-attachments/assets/752ddf5b-f8e6-46b0-b4a1-b8ce5133eece" />

(screenshot is [Vivida](https://github.com/vivido-dev/vivida), a wrapper of vivido, vivido does not have workspaces and split panes, but you can use [vvmux](https://github.com/vivido-dev/vvmux) to similar effects in Vivido)

## For developers

### Performance

Vivido renders text on the GPU through [wgpu](https://github.com/gfx-rs/wgpu), using Metal on
macOS, DirectX 12 on Windows, and Vulkan on Linux. It has the best average result across the
terminals in our [benchmarks](docs/benchmarks.md).

### Inline media

Images, encoded video, audio, and camera feeds are displayed in the terminal window alongside
text, so you can look at a plot or screenshot without copying the file somewhere else first. Over
SSH, the bundled `vvssh` forwards the same media from remote hosts and supports drag-and-drop file
transfer.

### Windows

Vivido runs natively on Windows using DirectX 12 and ConPTY, with the same rendering, inline media,
and automation features as on macOS and Linux. PowerShell and each installed WSL distribution open
as tabs in the same window, and WSL shells get `TERM=vivido` and true color automatically.
`vivido msg` and headless sessions work over an owner-only named pipe. The signed installer sets up
PowerShell 7, WSL with Ubuntu, and your PATH. See [Using Vivido on Windows](docs/windows.md).

### Accessibility

On macOS, screen readers receive the actual text, its geometry, the caret position, and scrollback
rather than a rendered bitmap.

## For AI agents

Vivido is designed to be controlled programmatically as well as typed into.

- **Automation API.** `vivido msg` gives agents structured grid snapshots, typed input,
  window and process control, screenshots, sanitized transcripts, state waits, and replayable event
  subscriptions, so agents can read the terminal as data instead of scraping the screen.
- **Headless sessions.** Run a full terminal — renderer and all — in the background with no window,
  addressable by name, discoverable, and safe to script:

  ```sh
  eval "$(vivido --headless --session build)"
  vivido msg get-text
  vivido msg screenshot
  ```

  This gives CI jobs, agents, and scripts a real terminal without a visible window.

- **Discovery and waits.** Agents can find windows, wait for a prompt, and watch for changes
  instead of polling.

Start with `vivido msg capabilities`, then read the
[Agent automation guide](docs/ipc.md) and [Headless sessions](docs/headless.md).

## Try it now

Download from releases, from [vivido.dev](https://vivido.dev/), or use cargo.

```sh
cargo install vivido
```

See [INSTALL.md](INSTALL.md). If you just want the highlight tour, see [Features](docs/features.md).

**Give your next coding agent a terminal it can actually use.** Point it at `vivido msg` and let it
work.

## Ecosystem

Vivido is the center of a family of tools built on
[Vivid Protocol 1.5](https://github.com/vivido-dev/vivid_protocol), all in the
[`vivido-dev` org](https://github.com/vivido-dev):

| Tool | Repo | Summary |
|---|---|---|
| vvmux | [vivido-dev/vvmux](https://github.com/vivido-dev/vvmux) | Detachable terminal multiplexer; panes, scrollback, and live media survive detach |
| Vivi | [vivido-dev/vivi](https://github.com/vivido-dev/vivi) | Image viewer and media player: inspect or submit images, encoded video, and audio with flow control and keyframe recovery |
| vvrd | [vivido-dev/vvrd](https://github.com/vivido-dev/vvrd) | Full-screen PDF/EPUB/Markdown/Mermaid/Office document reader with a retained viewport |
| vrowser | [vivido-dev/vrowser](https://github.com/vivido-dev/vrowser) | Browser inside terminal with video and audio support |
| vvpaint | [vivido-dev/vvpaint](https://github.com/vivido-dev/vvpaint) | MSPaint inside terminal on Vivid protocol |
| vvcam | [vivido-dev/vvcam](https://github.com/vivido-dev/vvcam) | Streams a connected camera (V4L2/DirectShow, H.264) into a full-window terminal surface |
| vvland | [vivido-dev/vvland](https://github.com/vivido-dev/vvland) | Runs and streams an isolated Weston or Sway desktop, or a single Wayland app |
| vivida | [vivido-dev/vivida](https://github.com/vivido-dev/vivida) | Workspace and tab manager built around Vivido panes |
| vvDOOM | [vivido-dev/vvdoom](https://github.com/vivido-dev/vvdoom) | Doom played through the Vivid media stack — the showcase producer |

(`vvssh` needs no separate repo — it ships with Vivido itself.)

## Acknowledgements

Vivido was initially forked from [Alacritty](https://github.com/alacritty/alacritty) — our sincere thanks to
its authors and contributors for the outstanding foundation it is built on.

## License

Vivido is released under the Apache-2.0 license. See [LICENSE](LICENSE).

## Note

> [!WARNING]
> **On Windows, do not install WezTerm alongside Vivido.** Older Vivido builds can load
> WezTerm's bundled `conpty.dll` from `PATH`. We reproduced dropped key-release events with
> that DLL, causing controls in apps such as vvdoom to stop responding after the first movement.
> Updated Vivido builds use Windows' built-in ConPTY instead. If WezTerm is already installed,
> remove its directory from `PATH` or uninstall it, then restart Vivido completely.
