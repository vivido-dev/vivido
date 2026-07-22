# Vivido configuration

Vivido reads a single [TOML](https://toml.io) configuration file at startup and (optionally)
reloads it live while running. Vivido began as a fork of Alacritty, so much of the syntax will be
familiar; this guide documents **only what Vivido actually supports**. Options that Alacritty ships
but Vivido has removed are listed under [Removed options](#removed-options) so an imported Alacritty
config does not surprise you.

Vivido has no vi mode, no X11/OpenGL backends, and no built-in box-drawing shim. Those knobs are
gone. In their place Vivido adds a GPU renderer (Vello/wgpu) and the Vivid Protocol 1.1 media side
channel, which is configured through the environment rather than this file — see
[Vivid media and the environment](#vivid-media-and-the-environment).

## Table of contents

- [File location and format](#file-location-and-format)
- [`general`](#general)
- [`env`](#env)
- [`window`](#window)
- [`scrolling`](#scrolling)
- [`font`](#font)
- [`colors`](#colors)
- [`bell`](#bell)
- [`selection`](#selection)
- [`cursor`](#cursor)
- [`terminal`](#terminal)
- [`mouse`](#mouse)
- [`hints`](#hints)
- [`keyboard`](#keyboard)
- [`debug`](#debug)
- [Vivid media and the environment](#vivid-media-and-the-environment)
- [Removed options](#removed-options)

## File location and format

Vivido looks for `vivido.toml` in the first of these locations that exists:

| Platform | Search order |
|---|---|
| Linux / BSD | `$XDG_CONFIG_HOME/vivido/vivido.toml`, then `$XDG_CONFIG_HOME/vivido.toml`, then `~/.config/vivido/vivido.toml`, then `~/.vivido.toml`, then `/etc/vivido/vivido.toml` |
| macOS | `$XDG_CONFIG_HOME/vivido/vivido.toml`, then the same fallbacks as Linux |
| Windows | `%APPDATA%\vivido\vivido.toml` |

A legacy `vivido.yml` (YAML) file is accepted as a transitional fallback and converted to TOML
internally, but TOML is the supported format and the only one written by tooling. Point Vivido at an
explicit file with `vivido --config-file <path>`.

Every section is optional; unset fields use the defaults documented below. A ready-to-copy,
fully-commented starter file with every default in place ships alongside this guide as
[`vivido.toml`](vivido.toml).

### Imports

Split configuration across files with `general.import`. Paths may be absolute, `~/`-relative, or
relative to the importing file. Later imports and the importing file override earlier ones. Imports
nest up to 5 levels deep.

```toml
[general]
import = [
  "~/.config/vivido/theme.toml",
  "~/.config/vivido/keys.toml",
]
```

### Live reload

With `general.live_config_reload = true` (the default), Vivido watches the loaded file(s) and
reapplies changes on save — no restart needed.

## `general`

Miscellaneous top-level options. They live under `[general]` to avoid TOML's usual root-level
key ordering pitfalls.

| Field | Type | Default | Description |
|---|---|---|---|
| `import` | array of strings | `[]` | Additional config files to merge in. See [Imports](#imports). |
| `working_directory` | string | *inherit* | Directory the shell starts in. Unset inherits Vivido's working directory. |
| `live_config_reload` | bool | `true` | Reapply the config automatically when the file changes. |
| `ipc_socket` | bool | `true` | Offer IPC over a Unix socket (`vivido msg …`). Unix only; ignored on Windows. |

```toml
[general]
live_config_reload = true
working_directory = "~/work"
```

## `env`

A table of environment variables exported to every process Vivido spawns. Useful for `TERM`,
`WINIT_*`, locale, and similar.

```toml
[env]
TERM = "vivido"
WINIT_X11_SCALE_FACTOR = "1.0"
```

Vivido uses the `vivido` terminfo entry when installed and falls back to `xterm-256color`.

## `window`

| Field | Type | Default | Description |
|---|---|---|---|
| `dimensions` | `{ columns, lines }` | `{ 0, 0 }` | Initial size in cells. **Both** must be non-zero to take effect. |
| `position` | `{ x, y }` | *auto* | Startup position in physical pixels. Unset lets the window manager decide. |
| `padding` | `{ x, y }` | `{ 0, 0 }` | Blank space around the grid, in pixels (scaled by DPI). |
| `dynamic_padding` | bool | `false` | Distribute leftover space evenly as extra padding. |
| `decorations` | enum | `Full` | `Full`, `None`, `Transparent`, or `Buttonless`. |
| `opacity` | float `0.0`–`1.0` | `1.0` | Background opacity. Requires a compositor that honors it. |
| `blur` | bool | `false` | Request background blur (macOS and KDE Wayland). |
| `startup_mode` | enum | `Windowed` | `Windowed`, `Maximized`, `Fullscreen`, or `SimpleFullscreen`. |
| `title` | string | `"Vivido"` | Initial window title. |
| `dynamic_title` | bool | `true` | Let applications change the title via escape sequences. |
| `class` | `{ general, instance }` or string | `"Vivido"` | Wayland `app_id` / window class. A bare string sets `instance`. |
| `decorations_theme_variant` | enum | *system* | Force `Light` or `Dark` server-side decorations. |
| `resize_increments` | bool | `false` | Snap resizes to whole cells. |
| `option_as_alt` | enum | `None` | **macOS only.** Treat `OnlyLeft`, `OnlyRight`, `Both`, or `None` of the Option keys as Alt. |
| `level` | enum | `Normal` | `Normal` or `AlwaysOnTop`. |

```toml
[window]
padding = { x = 6, y = 6 }
opacity = 0.95
decorations = "Full"
startup_mode = "Windowed"

[window.dimensions]
columns = 120
lines = 32

[window.class]
general = "Vivido"
instance = "Vivido"
```

> On Linux, Vivido is Wayland-only. `option_as_alt` applies to macOS; window class maps to the
> Wayland `app_id`.

## `scrolling`

| Field | Type | Default | Description |
|---|---|---|---|
| `history` | integer | `10000` | Scrollback lines retained. Maximum `100000`. |
| `multiplier` | integer | `3` | Lines scrolled per wheel/step increment. |

```toml
[scrolling]
history = 50000
multiplier = 3
```

## `font`

Font size is in points. Style names (`"Regular"`, `"Bold"`, `"Italic"`, …) select faces within a
family; omit `style` to let the system pick.

| Field | Type | Default | Description |
|---|---|---|---|
| `normal` | `{ family, style }` | platform monospace | Base face. Family default: `monospace` (Linux), `Menlo` (macOS), `Consolas` (Windows). |
| `bold` | `{ family, style }` | derived from `normal` | Bold face; falls back to `normal.family`. |
| `italic` | `{ family, style }` | derived from `normal` | Italic face. |
| `bold_italic` | `{ family, style }` | derived from `normal` | Bold-italic face. |
| `size` | float | `11.25` | Font size in points. |
| `offset` | `{ x, y }` | `{ 0, 0 }` | Extra spacing added around each cell. |
| `glyph_offset` | `{ x, y }` | `{ 0, 0 }` | Shift the glyph within its cell. |

```toml
[font]
size = 12.0

[font.normal]
family = "JetBrains Mono"
style = "Regular"

[font.bold]
style = "Bold"
```

## `colors`

Colors are hex strings such as `"#1e1e2e"`. Cursor, selection, search, and hint fields also accept
the special values `"CellForeground"` and `"CellBackground"` to mirror the cell under them.

### Primary

```toml
[colors.primary]
foreground = "#d8d8d8"
background = "#181818"
# Optional overrides:
# bright_foreground = "#ffffff"   # used with draw_bold_text_with_bright_colors
# dim_foreground   = "#828482"
```

### Normal, bright, dim palettes

Each is the standard 8-color set (`black red green yellow blue magenta cyan white`). `dim` is
auto-derived if omitted.

```toml
[colors.normal]
black = "#181818"
red = "#ac4242"
green = "#90a959"
yellow = "#f4bf75"
blue = "#6a9fb5"
magenta = "#aa759f"
cyan = "#75b5aa"
white = "#d8d8d8"

[colors.bright]
black = "#6b6b6b"
# … red green yellow blue magenta cyan white …
```

### UI colors

| Field | Shape | Notes |
|---|---|---|
| `colors.cursor` | `{ text, cursor }` (aliases of `{ foreground, background }`) | Cursor glyph and box color. |
| `colors.selection` | `{ text, background }` | Selected text and highlight. |
| `colors.search.matches` | `{ foreground, background }` | All search matches. |
| `colors.search.focused_match` | `{ foreground, background }` | Currently focused match. |
| `colors.hints.start` | `{ foreground, background }` | First character of a hint label. |
| `colors.hints.end` | `{ foreground, background }` | Remaining hint label characters. |
| `colors.line_indicator` | `{ foreground, background }` | Scrollback/line indicator; unset follows the cell. |
| `colors.footer_bar` | `{ foreground, background }` | Search/message footer bar. |
| `colors.indexed_colors` | array of `{ index, color }` | Extend the 256-color palette; `index` must be `16`–`255`. |

### Toggles

| Field | Type | Default | Description |
|---|---|---|---|
| `colors.transparent_background_colors` | bool | `false` | Apply `window.opacity` to cells with an explicit background color too. |
| `colors.draw_bold_text_with_bright_colors` | bool | `false` | Render bold text using the bright palette. |

```toml
[colors.cursor]
text = "CellBackground"
cursor = "CellForeground"

[[colors.indexed_colors]]
index = 16
color = "#ff8700"
```

## `bell`

Visual (and optional command) bell.

| Field | Type | Default | Description |
|---|---|---|---|
| `animation` | enum | `Linear` | Easing: `Ease`, `EaseOut`, `EaseOutSine`, `EaseOutQuad`, `EaseOutCubic`, `EaseOutQuart`, `EaseOutQuint`, `EaseOutExpo`, `EaseOutCirc`, `Linear`. |
| `duration` | integer (ms) | `0` | Flash length. `0` disables the visual bell. |
| `color` | hex string | `"#ffffff"` | Flash color. |
| `command` | string or `{ program, args }` | *none* | Command run when the bell rings. |

```toml
[bell]
animation = "EaseOutQuad"
duration = 100
color = "#ffffff"
command = { program = "paplay", args = ["/usr/share/sounds/freedesktop/stereo/bell.oga"] }
```

## `selection`

| Field | Type | Default | Description |
|---|---|---|---|
| `save_to_clipboard` | bool | `false` | Copy selections to the system clipboard automatically. |

> Vivido has a deliberately minimal selection model: drag for a character selection, hold `Control`
> while dragging for a block selection. Semantic word/line expansion and
> `semantic_escape_chars` are not implemented, so there is no such option to configure.

## `cursor`

| Field | Type | Default | Description |
|---|---|---|---|
| `style` | `{ shape, blinking }` or `shape` | `Block` / `Off` | `shape`: `Block`, `Underline`, `Beam`. `blinking`: `Never`, `Off`, `On`, `Always`. |
| `unfocused_hollow` | bool | `true` | Draw a hollow cursor when the window loses focus. |
| `thickness` | float `0.0`–`1.0` | `0.15` | Beam/underline thickness relative to the cell. |
| `blink_interval` | integer (ms) | `750` | Time between blinks (floor of 10 ms). |
| `blink_timeout` | integer (s) | `5` | Stop blinking after this idle time. `0` blinks forever. |

```toml
[cursor]
unfocused_hollow = true
thickness = 0.15

[cursor.style]
shape = "Block"
blinking = "Off"
```

## `terminal`

| Field | Type | Default | Description |
|---|---|---|---|
| `osc52` | enum | `onlycopy` | Clipboard access via OSC 52: `disabled`, `onlycopy`, `onlypaste`, `copypaste` (case-insensitive). |
| `shell` | string or `{ program, args }` | *system* | Shell to launch instead of the login shell. |

```toml
[terminal]
osc52 = "onlycopy"
shell = { program = "/usr/bin/fish", args = ["--login"] }
```

## `mouse`

| Field | Type | Default | Description |
|---|---|---|---|
| `hide_when_typing` | bool | `false` | Hide the pointer while typing. |
| `bindings` | array | see below | Custom mouse bindings; merged over the defaults. |

Each binding takes a `mouse` button (`Left`, `Middle`, `Right`, `Back`, `Forward`, `WheelUp`,
`WheelDown`, or a number), optional `mods` and `mode`, and exactly one of `action`, `command`, or
`chars`. Actions are the same set as [keyboard actions](#keyboard).

```toml
[mouse]
hide_when_typing = true

[[mouse.bindings]]
mouse = "Middle"
action = "PasteSelection"
```

On Windows, right-click pastes the clipboard by default when the application is not consuming mouse
events.

## `hints`

Regex hints match on-screen text (and OSC 8 hyperlinks) and let you open or act on it via keyboard
or mouse. This replaces Alacritty's vi-cursor-driven hint flow; Vivido hints work without a vi mode.

| Field | Type | Default | Description |
|---|---|---|---|
| `alphabet` | string | `"jfkdls;ahgurieowpq"` | Label keys. Must be at least 2 width-1 characters. |
| `enabled` | array of hint objects | one URL hint | The configured hints. |

Each hint object:

| Field | Type | Description |
|---|---|---|
| `regex` | string | Pattern to match. Optional if `hyperlinks = true`. |
| `hyperlinks` | bool | Also match OSC 8 hyperlinks. |
| `post_processing` | bool | Trim trailing punctuation and similar heuristics off matches. |
| `persist` | bool | Keep hint mode open after a selection. |
| `action` | enum | Built-in action: `Copy`, `Paste`, or `Select`. |
| `command` | string or `{ program, args }` | Pipe the match to a command instead of an `action`. |
| `mouse` | `{ enabled, mods }` | Enable pointer highlighting and its required modifiers. |
| `binding` | `{ key, mods, mode }` | Keyboard trigger for the hint. |

Provide exactly one of `action` or `command`. The default configuration is a single URL-opening hint
bound to `Control+Shift+O` (and mouse), opening with `xdg-open` (Linux), `open` (macOS), or the
shell on Windows.

```toml
[hints]
alphabet = "jfkdls;ahgurieowpq"

[[hints.enabled]]
regex = "[^ ]+\\.rs"
hyperlinks = false
post_processing = true
action = "Copy"
binding = { key = "R", mods = "Control|Shift" }
mouse = { enabled = true, mods = "Control" }
```

## `keyboard`

Custom key bindings are merged over Vivido's defaults; a binding whose trigger matches a default
replaces it, and `action = "ReceiveChar"` (or `"None"`) neutralizes a default.

Each binding needs a `key`, optional `mods` and `mode`, and one of `action`, `chars`, or `command`.

- **`key`** — a character (`"a"`), a named key (`"F5"`, `"Home"`, `"PageUp"`, `"Enter"`), or a raw
  scancode.
- **`mods`** — `|`-joined subset of `Control`, `Shift`, `Alt`/`Option`, `Super`/`Command`. Case
  insensitive; `None` for no modifiers.
- **`mode`** — terminal modes such as `AppCursor`, `AppKeypad`, `Alt`, `Search`; prefix with `~` to
  require the mode be *off* (e.g. `~Search`).
- **`chars`** — literal bytes/escape to send.
- **`command`** — `{ program, args }` to spawn.

### Actions

Window / app: `Quit`, `Hide`, `HideOtherApplications` (macOS), `Minimize`, `ToggleFullscreen`,
`ToggleMaximized`, `ToggleSimpleFullscreen` (macOS), `SpawnNewInstance`, `CreateNewWindow`,
`CreateNewTab` (macOS), `SelectNextTab`, `SelectPreviousTab`, `SelectTab1`…`SelectTab9`,
`SelectLastTab`.

Clipboard / selection: `Copy`, `Paste`, `CopySelection`, `PasteSelection`, `ClearSelection`.

Font: `IncreaseFontSize`, `DecreaseFontSize`, `ResetFontSize`.

Scrolling: `ScrollLineUp`, `ScrollLineDown`, `ScrollHalfPageUp`, `ScrollHalfPageDown`,
`ScrollPageUp`, `ScrollPageDown`, `ScrollToTop`, `ScrollToBottom`.

Search: `SearchForward`, `SearchBackward`. Inside search mode: `SearchFocusNext`,
`SearchFocusPrevious`, `SearchConfirm`, `SearchCancel`, `SearchClear`, `SearchDeleteWord`,
`SearchHistoryPrevious`, `SearchHistoryNext`.

Misc: `ClearHistory`, `ClearLogNotice`, `ReceiveChar`, `None`.

> There is no vi mode, so vi-motion, vi-cursor, and vi-selection actions do not exist in Vivido.

### Default search bindings

| Keys | Action |
|---|---|
| `Control+Shift+F` (`Command+F` on macOS) | Start a forward search |
| `Control+Shift+B` (`Command+B` on macOS) | Start a backward search |
| `F3` / `Shift+F3` | Next / previous match |
| `Enter` | Confirm the focused match as a selection |
| `Escape` | Cancel search |

```toml
[[keyboard.bindings]]
key = "N"
mods = "Control|Shift"
action = "SpawnNewInstance"

[[keyboard.bindings]]
key = "Return"
mods = "Alt"
action = "ToggleFullscreen"

# Disable a default:
[[keyboard.bindings]]
key = "V"
mods = "Control|Shift"
action = "ReceiveChar"
```

## `debug`

Diagnostics. Most users never touch these.

| Field | Type | Default | Description |
|---|---|---|---|
| `log_level` | enum | `Warn` | `Off`, `Error`, `Warn`, `Info`, `Debug`, `Trace`. |
| `print_events` | bool | `false` | Log window and input events. |
| `persistent_logging` | bool | `false` | Keep the log file after exit. |
| `render_timer` | bool | `false` | Overlay per-frame render time. |
| `highlight_damage` | bool | `false` | Tint redrawn (damaged) regions. |

```toml
[debug]
log_level = "Info"
render_timer = false
```

## Vivid media and the environment

Vivido is the reference **Vivid Protocol 1.1** presenter: it decodes and composites images, video,
and audio delivered over an authenticated per-window side channel (see the project `README.md` and
`docs/vivid_protocol_spec.md`). This pathway is **not** configured through `vivido.toml`. There are
no TOML knobs for codecs, buffering, credits, or endpoints — the presenter negotiates all of that at
runtime and keeps media bytes off the PTY.

What you interact with instead is the environment Vivido sets for programs it launches:

- `VIVID_ENDPOINT` — the private per-window control endpoint. Vivido exports this to its child
  shell automatically; producers such as `vivi` read it.
- `VIVID_TOKEN` — the per-window capability token. **Never** print, log, copy into shell history,
  pass as a command argument, or commit it. Treat it like a password.
- `VIVID_ENDPOINT_BULK` — an optional separate media transport advertised by `vvssh` for remote
  sessions. It is transport discovery only; control always stays on `VIVID_ENDPOINT`.

For remote display, use the bundled `vvssh` wrapper rather than plain `ssh`, which does not forward
the media endpoint. See `docs/vivi-over-ssh.md`.

Rendering backend is fixed per platform and not configurable: Metal on macOS, DirectX 12 on
Windows, Vulkan on Linux (Wayland only).

