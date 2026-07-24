# Vivido agent automation IPC

Vivido exposes an owner-only automation service on Unix. It is intended for agents, test runners,
and local programs which need to control and observe both Vivido and terminal applications running
inside it. Windows IPC is not part of protocol version 1.

The CLI is the stable shell interface:

```sh
vivido msg capabilities
vivido msg list-windows
vivido msg inspect --window-id 42
vivido msg wait text 'ready>' --window-id 42
vivido msg key Enter --window-id 42
```

## Endpoint discovery and targeting

`vivido msg --socket PATH ...` uses an explicit socket. Without `--socket`, the client first tries
`VIVIDO_SOCKET`; if it is unset or stale, it scans Vivido's runtime directory for the newest live
socket on the current display. Socket files have mode `0600`. Vivido limits the server to 32 active
connections.

Window-targeted CLI commands resolve their target in this order:

1. `--window-id ID`;
2. inherited `VIVIDO_WINDOW_ID`;
3. the currently focused Vivido window.

Use `list-windows` when a caller did not inherit the per-window environment. An explicit missing ID
returns `window_not_found`; a command requiring the focused fallback returns `no_focused_window`
when no Vivido window is focused. `subscribe --all` intentionally bypasses target resolution.

## Version 1 wire protocol

The socket carries newline-delimited UTF-8 JSON. Each frame is one JSON value followed by `\n`.
Request frames are limited to 1 MiB and reply/event frames to 16 MiB.

Every connection must begin with `hello`:

```json
{"version":1,"id":1,"method":"hello","params":{}}
```

The response advertises the server version, protocol version, methods, event kinds, stable error
codes, and effective limits:

```json
{"version":1,"id":1,"ok":true,"result":{"protocol_version":1,"methods":[],"event_kinds":[],"limits":{}}}
```

A legacy raw enum frame, malformed first frame, non-`hello` first request, or unsupported version
gets a structured error and the connection closes. There is no compatibility mode for the former
unversioned protocol.

Subsequent requests use the same envelope:

```json
{"version":1,"id":17,"method":"inspect","params":{"window_id":42}}
```

Correlated success and failure envelopes are:

```json
{"version":1,"id":17,"ok":true,"result":{}}
{"version":1,"id":17,"ok":false,"error":{"code":"window_not_found","message":"..."}}
```

Request IDs are scoped to a connection. Up to 64 may be active at once. Reusing an active ID
returns `duplicate_request_id`; an ID may be reused after its response. Requests are full duplex:
responses may arrive out of order, but a connection has one serialized writer so JSON frames never
interleave. Programs must correlate responses by `id` and distinguish event frames by their
`subscription_id` field.

Stable protocol errors are `unsupported_version`, `invalid_request`, `invalid_params`,
`duplicate_request_id`, `limit_exceeded`, `window_not_found`, `no_focused_window`, `unsupported`,
`invalid_state`, `timeout`, `sequence_gap`, `pty_closed`, `resize_mismatch`, `focus_denied`,
`regex_invalid`, and `subscription_overflow`. Errors may include a `data` object with recovery
details.

## Common JSON conventions

CLI flags are converted to snake-case JSON fields. Commands using a common target encode it as
`"target":{"window_id":42}`; `get-text`, `screenshot`, `focus`, `inspect`, and subscriptions use
`window_id` directly. Omit the ID or send `null` for the focused fallback.

Input routes are `application` and `ui`. The default `application` route bypasses Vivido bindings,
search, hints, selection, clipboard actions, and local shortcuts, while honoring terminal cursor,
keypad, bracketed-paste, Kitty keyboard, and mouse modes. The `ui` route deliberately runs through
Vivido's normal input processor. Its modifier state is scoped to the request and cannot leave
physical modifiers pressed.

## Methods and CLI commands

### Basic and existing commands

- `hello {}`: required handshake. `vivido msg capabilities` prints its `result` as JSON.
- `ping {}`: wire-only liveness request; returns `{"pong":true}`.
- `create_window`: synchronously constructs a complete window and returns `{"window_id":ID}`.
  The CLI is `vivido msg create-window` with its existing window, command, directory, hold, title,
  class, and config options. `ipc_window_id` is optional and must be unique. The response does not
  wait for the first rendered frame.
- `config` and `get_config`: back the existing `config` and `get-config` commands. Configuration
  updates now always receive a correlated response. The special config ID `-1` means all/global.
- `typing {"text":"...","window_id":ID}`: writes literal UTF-8 bytes without paste handling or an
  appended Enter. Text is limited to 1 MiB. Success is sent only after every byte is written to the
  PTY master, with a five-second write timeout.
- `get_text {"rows":N,"window_id":ID}`: returns `{"text":"..."}`. With no `rows`, text is the
  visible viewport at its current scroll position. `rows` accepts 1 through 1000 and reads newest
  physical rows at the live bottom, including scrollback. The CLI writes text exactly, without an
  added newline. Styling, cursor, media, search, and message overlays are excluded.
- `screenshot {"window_id":ID}`: returns `{"path":"/absolute/private/file.png"}`. The CLI prints
  the path plus a newline. The PNG is the last successfully presented client-area frame at physical
  resolution and includes terminal rendering, cursor, selection, Vivido overlays, and Vivid media.
  It excludes OS decorations and desktop content. Straight alpha is preserved. The persistent temp
  file has mode `0600`; its caller owns cleanup. A resize invalidates the stored frame until another
  frame is presented. Only one readback per window may run at once and raw allocation is capped at
  256 MiB.

### Mode-aware input and process control

- `key {"key":"Enter","mods":["Ctrl"],"repeat":1,"route":"application","target":{...}}`
  supports one Unicode scalar, Enter, Escape, Tab, Backspace, arrows, Home/End, Insert/Delete,
  PageUp/PageDown, F1-F35, and `Keypad0` through `Keypad9`, `KeypadDecimal`, `KeypadDivide`,
  `KeypadMultiply`, `KeypadSubtract`, `KeypadAdd`, `KeypadEnter`, and `KeypadEqual`. Modifiers are
  Ctrl, Alt, Shift, and Super; repeat is 1 through 1000. Application/UI PTY bytes use tagged write
  completion before success.
- `paste {"text":"...","route":"application","target":{...}}` accepts at most 1 MiB. Application
  paste uses Vivido's bracketed-paste filtering and newline normalization without entering local UI
  state. UI paste can instead update an active search.
- `mouse {"action":{"move":POSITION}}` supports `move`, `click`, `double_click`, `down`, `up`,
  `drag`, and `scroll`. A position contains exactly one zero-based cell pair
  (`cell_column`,`cell_row`) or physical-pixel pair (`x`,`y`), plus `mods`, `route`, and `target`.
  Button actions add `button` (`left`, `middle`, or `right`). Scroll adds finite `vertical` and
  `horizontal` amounts and is capped at 1000 reports. Application routing requires active terminal
  mouse reporting and the live-bottom viewport. UI routing can select text, invoke mouse bindings,
  follow links, or report to the application as normal UI input would.
- `resize {"columns":C,"rows":R,"width":null,"height":null,"target":{...}}` requests exact grid
  dimensions; replace the grid pair with `width`/`height` for exact physical client pixels. Grid
  size is at least 2 by 1 and must fit renderer and PTY limits. Only one resize per window is active.
  Success waits for both the OS size and terminal/PTY size; failure after five seconds is
  `resize_mismatch` with requested/actual details where available.
- `focus {"window_id":ID}` requests real operating-system activation. It succeeds only after an
  actual focused event and otherwise returns `focus_denied` after two seconds. Vivido never
  synthesizes terminal focus state. On Wayland, the request uses `xdg_activation_v1` to obtain and
  apply a compositor-approved client activation token when that protocol is available.
- `signal {"signal":"INT","target":{...}}` accepts `INT`, `TERM`, `HUP`, `QUIT`, `TSTP`, `CONT`,
  `WINCH`, `KILL`, and `STOP`. It sends only the explicitly named signal to the current foreground
  process group, falling back to the PTY child group. KILL and STOP have no implicit aliases.

### Discovery and inspection

- `list_windows {}` returns `{"windows":[...]}` sorted by monotonic `creation_index`. Each entry
  contains window ID, title, focus/occlusion/hold state, grid/pixel dimensions, process state, and
  current screen/frame/output sequences.
- `inspect {"window_id":ID}` returns the list entry plus cell dimensions, scale, scrollback,
  display offset, primary/alternate screen, terminal mode names, cursor, selection, shell PID,
  foreground process group, optional executable basename/current directory, echo state, exit
  status, global event sequence, and effective automation limits. It never returns process
  arguments, environment values, Vivid tokens, tickets, authenticators, or derived capabilities.

### Structured grid

`get_grid` defaults to the current viewport:

```json
{"target":{"window_id":42},"start_line":null,"row_count":null,"since_screen":null}
```

`start_line` and `row_count` must appear together and address signed physical lines from retained
scrollback through the live screen; `row_count` is 1 through 1000. `since_screen` is mutually
exclusive with an explicit range and returns current viewport row replacements changed after that
sequence.

The result contains `window_id`, `screen_sequence`, `full`, optional `gap`, grid dimensions,
returned signed bounds, history size, display offset, cursor, selection, screen name, terminal mode
names, a deduplicated style table, and row objects. Each physical row records its signed grid line,
optional viewport row, soft-wrap flag, and every physical cell. Cells contain text (including
combining characters), width 0/1/2, `character`, `continuation`, or `leading_wide_spacer` kind, and
a style ID. Styles use resolved RGBA foreground/background/underline colors, attributes, and
optional hyperlink ID/URI. Tabs, blank styled cells, wide spacers, combining characters, and wrap
flags remain explicit.

A delta unions all changed rows and intentionally coalesces intermediate states. History older than
the retained 1,024 screen changes, resize/reflow, screen swap, scroll-position change, or another
full invalidation returns a full viewport with gap metadata. Replies larger than 16 MiB fail with
`limit_exceeded` and are never truncated.

### State sequences and waits

Each window has monotonic `screen_sequence`, `frame_sequence`, and `output_offset`; the process has
a monotonic `event_sequence`.

- Screen sequence changes represent the visible terminal model: physical rows, cursor, selection,
  dimensions, display offset, screen, and terminal input modes. Cursor blink phase, visual-bell
  animation, search/message overlays, and Vivid media do not increment it.
- Frame sequence increments only after successful surface acquisition, rendering, and presentation.
- Output offset counts retained sanitized PTY bytes before ring eviction; it never resets.
- Event sequence orders replayable automation events across all windows.

Wait methods use a 30-second CLI default. `timeout` is milliseconds on the wire and accepts 1 ms
through 24 hours. CLI duration values accept bare milliseconds or `ms`, `s`, `m`, and `h` suffixes.

- `wait_text`: params are `text`, `regex`, `after_screen`, and `common:{timeout,target}`. It searches
  current visible text immediately unless `after_screen` requires a newer screen.
- `wait_output`: params are `pattern`, mutually exclusive `regex`/`base64`, `after_offset`, and
  `common`. Without an offset it starts at the current output end and matches only future bytes.
  Matches may cross PTY read boundaries. An evicted explicit offset returns `sequence_gap`.
- `wait_screen_change`: params are `after_screen` and `common`. With no sequence it waits for the
  next change after registration.
- `wait_screen_stable`: adds `quiet` milliseconds. It completes after no semantic screen change for
  that duration; `after_screen` first requires at least one newer screen.
- `wait_frame`: params are `after_frame` and `common`; omitted means the next presented frame.
- `wait_exit`: params are `timeout` and `target`. Held windows with retained status return
  immediately; unheld windows complete from child exit before removal.

Regex patterns are limited to 8 KiB and use linear-time matching. Disconnecting cancels waits,
pending tagged input, resize/focus requests, and subscriptions immediately.

### Transcript and subscriptions

Each window retains a 1 MiB sanitized byte-exact PTY ring after Vivid marker envelopes have been
removed. `transcript` params are `after_offset`, `max_bytes` (default 65536, maximum 1048576),
`raw`, and `target`. JSON results include oldest/start/returned-end/current-end offsets, a truncation
flag, and base64 data. An explicit evicted offset returns `sequence_gap`. `transcript --raw` decodes
and writes the exact bytes without a newline.

`subscribe` params are `window_id`, `all`, `events`, and `since_event`. A targeted subscription uses
the normal focused fallback; `all:true` receives every window plus process lifecycle events. The
acknowledgement returns `subscription_id` and current `event_sequence`. Up to 32 subscriptions exist
per process and each has at most 256 queued events.

Event frames have this shape:

```json
{"version":1,"subscription_id":7,"event_sequence":123,"window_id":42,"event":{"type":"screen_changed","data":{}}}
```

Kinds are `screen_changed`, `output`, `frame_presented`, `title_changed`, `focus_changed`, `resized`,
`bell`, `child_exit`, `window_created`, `window_closed`, and `overflow`. Output data is split into
at most 64 KiB chunks with start/end offsets and base64 bytes. Screen-change data contains current
row replacements. The process replay ring is bounded by both 4 MiB and 4,096 events.

`since_event` atomically replays retained matching events before live delivery. If history is gone,
the first event is `overflow` with the gap and current window sequences so the client can recover
with `inspect` and `get-grid`. Slow clients never block the UI or PTY thread. A full subscription
queue collapses dropped detail into one overflow range before delivery resumes. Closing the socket
or pressing Ctrl-C cancels CLI subscriptions. Wire clients may send
`unsubscribe {"subscription_id":7}` on the same connection.

## CLI output contract

New structured observations, waits, transcript metadata, capabilities, and subscription events are
one compact JSON object per line. Controls are silent on success. `create-window` prints only the
new numeric ID, `get-text` prints exact text without a newline, `screenshot` prints one absolute path
with a newline, and `transcript --raw` prints exact decoded bytes without a newline. Structured IPC
errors make `vivido msg` exit nonzero and are written to standard error.
