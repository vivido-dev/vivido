# Vivido

**The fast, cross-platform, GPU terminal for developers and their AI agents.**

Vivido is a blazing-fast, GPU-native terminal emulator for macOS, Windows, and Linux. It renders
text like a champion — and then goes where no terminal has gone before: images, video, and audio
play *inside* your shell, and AI agents get a first-class API to drive, read, and observe every
window. One terminal, built for the two users who share it: you and your agent.

## Why developers love Vivido

### Speed you can feel

Text is rendered on the GPU through [wgpu](https://github.com/gfx-rs/wgpu),
riding Metal on macOS, DirectX 12 on Windows, and Vulkan on Linux.
Scrolling through a build log feels effortless — that's the whole point.

### Your media lives where you work

Charts, screenshots, webcam feeds, encoded video, audio — streamed straight into the terminal
window, right between the text. Stop `scp`-ing files around just to glance at a plot. And over SSH,
the bundled `vvssh` brings the same rich media to your remote boxes, with drag-and-drop file
transfer into the bargain.

### Accessible by design

Screen readers get real text geometry, caret position, and scrollback on macOS — not a rasterized
bitmap. Accessibility is a feature, not an afterthought.

## Why AI agents love Vivido

Vivido is the first terminal designed from day one to be *driven*, not just typed into.

- **A real automation API.** `vivido msg` gives agents structured grid snapshots, typed input,
  window and process control, screenshots, sanitized transcripts, state waits, and replayable event
  subscriptions. No fragile screen-scraping — agents read the terminal as data.
- **Headless sessions.** Run a full terminal — renderer and all — in the background with no window,
  addressable by name, discoverable, and safe to script:

  ```sh
  eval "$(vivido --headless --session build)"
  vivido msg get-text
  vivido msg screenshot
  ```

  CI, agents, and bots get a real terminal surface on demand.

- **Deterministic discovery.** Find windows, wait for the prompt, watch for changes. Agents stop
  guessing and start knowing.

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
