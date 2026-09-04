# Vivido automation commands

Every flag here comes from the shipped CLI. When something is unfamiliar, run
`vivido msg <command> --help` rather than reconstructing a wire request by hand — and
`vivido msg capabilities` rather than assuming a method exists.

## Output contract

Structured observations, waits, transcript metadata, capabilities, and subscription events print as
one compact JSON object per line. Controls are silent on success. The exceptions are worth knowing:

| Command | Prints |
|---|---|
| `create-window` | the new numeric window ID, alone |
| `get-text` | exact text, with no added newline |
| `screenshot` | one absolute path; the full metadata only with `--json` |
| `capture` | screenshot JSON, always |
| `transcript --raw` | exact decoded bytes, no newline |

Structured errors go to standard error and exit nonzero. Stable codes: `unsupported_version`,
`invalid_request`, `invalid_params`, `duplicate_request_id`, `limit_exceeded`, `window_not_found`,
`no_focused_window`, `unsupported`, `invalid_state`, `timeout`, `sequence_gap`, `pty_closed`,
`resize_mismatch`, `focus_denied`, `regex_invalid`, `subscription_overflow`, `client_fault`. An
error may carry a `data` object with recovery detail.

## Discovery

```sh
vivido list --all --json                    # every instance, headed and headless, all platforms
vivido msg capabilities                     # the hello result: methods, event kinds, limits
vivido msg ping                             # {"pong":true}
vivido msg list-windows
vivido msg inspect --window-id 42
```

`list-windows` returns entries sorted by monotonic `creation_index`: window ID, title,
focus/occlusion/visibility/hold, grid and pixel dimensions, outer frame position, process state,
`client_health`, and current `sequences`.

`inspect` adds cell metrics, scale factor, scrollback size, display offset, primary/alternate
screen, terminal mode names, cursor, selection, shell PID, foreground process group, executable
basename, current directory, echo state, exit status, global event sequence, and effective limits.
It never returns process arguments, environment values, or any capability material.

Useful fields:

```
.window.sequences.screen   .window.sequences.frame   .window.sequences.output
.window.grid.columns       .window.pixels.width      .window.padding.x
.cell.width                .scale_factor             .current_directory
```

`current_directory` prefers the shell's OSC 7 report when its host is this machine; on Windows OSC 7
is the only source.

## Input

```sh
vivido msg typing 'cargo test' --window-id 42 --report
vivido msg key Enter --window-id 42
vivido msg key c --mods Ctrl --window-id 42
vivido msg key ArrowDown --repeat 4 --window-id 42
vivido msg paste "$(cat notes.txt)" --window-id 42
vivido msg signal INT --window-id 42
```

- `typing` writes literal UTF-8, up to 1 MiB, with no paste handling and no appended Enter. Success
  arrives only after every byte reached the PTY master, with a five-second write timeout.
- `key` accepts one Unicode scalar or a named key: `Enter`, `Escape`, `Tab`, `Backspace`, arrows,
  `Home`/`End`, `Insert`/`Delete`, `PageUp`/`PageDown`, `F1`–`F35`, `Keypad0`–`Keypad9`,
  `KeypadDecimal`, `KeypadDivide`, `KeypadMultiply`, `KeypadSubtract`, `KeypadAdd`, `KeypadEnter`,
  `KeypadEqual`. Modifiers are `Ctrl`, `Alt`, `Shift`, `Super`, comma-separated. `--repeat` is 1
  through 1000.
- `paste` accepts at most 1 MiB and applies bracketed-paste filtering and newline normalisation.
- `signal` sends exactly the named signal — `INT`, `TERM`, `HUP`, `QUIT`, `TSTP`, `CONT`, `WINCH`,
  `KILL`, `STOP` — to the foreground process group, falling back to the PTY child group. `KILL` and
  `STOP` have no implicit aliases.

`--report` on `typing`, `key`, and `paste` prints the resolved window, encoded byte count, input
sequence, and PTY-write completion. It says explicitly that application consumption was **not**
observed; nothing on a PTY can prove that.

### Routes

`--route application` (the default) bypasses Vivido's bindings, search, hints, selection, and
clipboard actions while honouring the terminal's cursor, keypad, bracketed-paste, Kitty keyboard,
and mouse modes. `--route ui` runs through Vivido's normal input processor — use it for Vivido's own
bindings and local UI behaviour. Modifier state on the UI route is scoped to the request, so it can
never leave a physical modifier stuck down. An explicit target accepts either route while in the
background.

## Mouse

```sh
vivido msg mouse move --x 320 --y 180 --route ui --window-id 42
vivido msg mouse click --cell-column 12 --cell-row 4 --button left --window-id 42
vivido msg mouse click --relative-x 0.5 --relative-y 0.5 --button left --window-id 42
vivido msg mouse scroll --x 320 --y 180 --vertical -3 --route ui --window-id 42
vivido msg mouse path --point 100,100 110,105 120,115 \
  --button left --route application --duration 250ms --wait-frame --window-id 42
```

Actions are `move`, `click`, `double-click`, `down`, `up`, `drag`, `path`, `scroll`. A position
carries exactly one of: a zero-based cell pair (`--cell-column`, `--cell-row`), a physical-pixel
pair (`--x`, `--y`), or a relative pair (`--relative-x`, `--relative-y`, each 0 through 1, mapped
atomically to the current client area).

`mouse path` is one bounded press/move/release gesture of 2 through 1,000 points in a single
request. It takes **physical pixels only** — no cell or relative form. Prefer it over one CLI
invocation per point. `--duration` (1 ms to 30 s) paces it, with at most one paced gesture per
window; a paced gesture is bounded by its own deadline and fails with `timeout` rather than
blocking. Vivido always releases the held button on completion, failure, disconnect, cancellation,
or window loss. `--wait-frame` delays success until a newer frame is presented.

Application routing requires active terminal mouse reporting and the live-bottom viewport. SGR pixel
mouse mode preserves exact physical coordinates; other modes resolve them to cells. Coordinates do
not survive a resize, layout change, scale-factor change, font change, or content change.

## Waits

CLI default timeout is 30 s; values accept bare milliseconds or `ms`, `s`, `m`, `h` suffixes, from
1 ms to 24 hours.

```sh
vivido msg wait text 'ready>' --window-id 42 --after-screen "$screen"
vivido msg wait text 'completed in [0-9.]+s' --regex --window-id 42
vivido msg wait output 'panicked at' --after-offset "$offset" --window-id 42
vivido msg wait screen-change --after-screen "$screen" --window-id 42
vivido msg wait screen-stable --quiet 250ms --window-id 42
vivido msg wait frame --after-frame "$frame" --window-id 42
vivido msg wait exit --window-id 42 --timeout 10m
```

- `wait text` searches current visible text immediately unless `--after-screen` demands a newer
  screen. `--regex` patterns are capped at 8 KiB and matched in linear time.
- `wait output` without an offset starts at the current end and matches only future bytes; matches
  may cross PTY read boundaries. `--base64` matches raw bytes. An evicted explicit offset returns
  `sequence_gap`.
- `wait screen-stable` completes after no semantic screen change for `--quiet`; with
  `--after-screen` it first requires at least one newer screen.
- `wait exit` returns immediately for a held window with a retained status.

What increments what, so a wait is chosen for the right reason:

| Counter | Advances on |
|---|---|
| `screen_sequence` | physical rows, cursor, selection, dimensions, display offset, screen swap, terminal input modes — **not** cursor blink, visual bell, overlays, or Vivid media |
| `frame_sequence` | only after successful surface acquisition, rendering, and presentation |
| `output_offset` | retained sanitized PTY bytes; never resets |
| `event_sequence` | process-wide ordering of replayable automation events |

Disconnecting cancels waits, pending tagged input, resize and focus requests, and subscriptions
immediately.

## Reading the screen

```sh
vivido msg get-text --window-id 42                  # visible viewport at current scroll
vivido msg get-text --rows 200 --window-id 42       # newest physical rows, scrollback included
vivido msg get-grid --window-id 42
vivido msg get-grid --since-screen "$screen" --window-id 42
vivido msg get-grid --start-line -40 --row-count 40 --window-id 42
vivido msg transcript --after-offset "$offset" --max-bytes 65536 --window-id 42
```

`get-text` excludes styling, cursor, media, search, and message overlays; `--rows` is 1 through 1000.

`get-grid` returns `window_id`, `screen_sequence`, `full`, an optional `gap`, grid dimensions,
returned signed bounds, history size, display offset, cursor, selection, screen name, terminal mode
names, a deduplicated style table, and row objects. Each row records its signed grid line, optional
viewport row, soft-wrap flag, and every physical cell; a cell carries its text (combining characters
included), width 0/1/2, a kind of `character`, `continuation`, or `leading_wide_spacer`, and a style
ID. Styles resolve to RGBA foreground, background, and underline colours, attributes, and an
optional hyperlink. `--start-line` and `--row-count` (1–1000) go together and address signed
physical lines from retained scrollback through the live screen; `--since-screen` is mutually
exclusive with them and returns changed viewport rows.

A delta unions all changed rows and deliberately coalesces intermediate states. Scrollback older
than the retained 1,024 screen changes, a resize or reflow, a screen swap, or a scroll-position
change returns a full viewport with gap metadata instead. A reply over 16 MiB fails with
`limit_exceeded` and is never truncated.

`transcript` reads the 1 MiB sanitized byte-exact PTY ring, after Vivid marker envelopes have been
removed. JSON gives oldest/start/returned-end/current-end offsets, a truncation flag, and base64
data; `--raw` writes the exact bytes. An evicted explicit offset returns `sequence_gap`. Use it for
transient output that a text snapshot would miss.

## Screenshots

```sh
vivido msg screenshot --json --window-id 42
vivido msg capture --window-id 42 --stable --after-frame "$frame" --timeout 30s
```

Both return `window_id`, `frame_sequence`, physical `width`/`height`, `scale_factor`,
`cell:{width,height}`, `padding:{x,y}`, and the PNG `path`. `capture` additionally accepts
`--activate` (requires `--window-id` and a host advertising pane activation — standalone Vivido does
not), `--after-frame N`, and `--stable[=DURATION]`, defaulting to 250 ms.

The PNG is the last successfully presented client-area frame at physical resolution: terminal
rendering, cursor, selection, Vivido overlays, and Vivid media, with straight alpha preserved. It
excludes OS decorations and desktop content. The file is mode `0600` on Unix and lives in the
per-user temporary directory on Windows; **the caller owns cleanup.** One readback per window at a
time, raw allocation capped at 256 MiB. A resize invalidates the stored frame until another is
presented. Headless sessions render offscreen and support this identically.

`padding` is the origin of the grid inside the capture and is not derivable from the other fields —
see the note in SKILL.md, and `scripts/geometry.py`.

## Windows and layout

```sh
vivido msg create-window --command ./my-cli --working-directory /project --title Build
vivido msg create-window --vivid-target desktop
vivido msg resize --columns 120 --rows 40 --window-id 42
vivido msg resize --width 1280 --height 720 --window-id 42
vivido msg set-geometry --x 100 --y 100 --width 1280 --height 720 --window-id 42
vivido msg set-visible --visible false --window-id 42
vivido msg set-level always-on-top --window-id 42
vivido msg focus --window-id 42
vivido msg reset-terminal --window-id 42
vivido msg restart-terminal --window-id 42
vivido msg quit
```

- `create-window` returns only after the window and PTY are ready, and prints the assigned ID. In a
  headed Linux or Windows process it creates and activates a **tab** in the existing top-level
  window; each tab's ID stays its stable public identity. macOS and headless sessions keep their
  own window semantics. `--vivid-target desktop` makes a `desktop-surface-v1` window with no grid,
  no anchors, and no shell, so terminal-shaped methods do not apply to it.
- `resize` requests exact grid dimensions (at least 2×1) or exact physical client pixels, never
  both. It waits for the OS size *and* the terminal/PTY size; after five seconds it fails with
  `resize_mismatch` and the requested/actual detail. One resize per window at a time.
- `set-geometry` returns as soon as the requests are issued, because a layout owner sends these
  continuously while dragging — subscribe to `moved` and `resized` for confirmation. Each of the
  position pair and the size pair is all-or-nothing, and at least one must be present. Headless
  returns `unsupported`.
- `set-visible` maps or unmaps without destroying, and deliberately does not take the keyboard.
- `focus` requests real OS activation and succeeds only after an actual focused event, otherwise
  `focus_denied` after two seconds. Vivido never synthesizes terminal focus state. On Windows the
  CLI makes a best-effort `AllowSetForegroundWindow` grant; foreground-lock rules may still deny it.
  On Wayland it uses `xdg_activation_v1` when available. Headless has no compositor, so it cannot
  succeed there.
- `reset-terminal` discards partial parser state, returns to the primary screen, clears
  client-controlled input modes and the Vivid scene, and resumes a quarantined PTY. Scrollback and
  the window ID survive.
- `restart-terminal` transactionally replaces the PTY and Vivid service from the pane's retained
  launch options. The window ID and any embedding Vivida position stay stable; a failed replacement
  leaves the quarantined pane available for another attempt.

## Plans

`run-plan` reads JSON from stdin or `--file PATH` and emits compact NDJSON for the plan, each step,
and the final status. It holds one owner-verified connection for the whole workflow.

```json
{
  "version": 1,
  "steps": [
    {"id": "inspect", "method": "inspect", "params": {"window_id": 42},
     "bind": {"target": "/window/window_id"}},
    {"id": "click", "method": "mouse",
     "params": {"action": {"click": {"button": "left", "position": {
       "relative_x": 0.5, "relative_y": 0.5, "mods": [], "route": "ui",
       "target": {"window_id": {"$ref": "target"}}}}}},
     "verify": {"window_id": {"$ref": "target"}, "frame_changed": true,
                "screenshot": true, "timeout": 30000}}
  ]
}
```

1 through 256 bounded linear steps. `bind` maps a plan-local alias to a JSON Pointer into that
step's result; a later JSON value consisting only of `{"$ref":"alias"}` substitutes it. `when`
compares one alias against an exact JSON `equals` value. `on_error` is `abort` by default, or
`continue`. There are no loops, scripts, persistent aliases, or forward references. `--dry-run`
validates without executing; `--preflight` runs observation methods only and reports mutations and
unavailable dependencies as skipped.

## Events

```sh
vivido msg subscribe --window-id 42 --events screen_changed,output
vivido msg subscribe --all --since-event "$sequence"
```

The handshake's `event_kinds` is also the `--events` allowlist, so take the list from
`capabilities` rather than from prose: `screen_changed`, `output`, `frame_presented`,
`title_changed`, `directory_changed`, `focus_changed`, `resized`, `moved`, `bell`, `child_exit`,
`window_created`, `window_closed`, `client_fault`, `client_recovered`, `overflow`.

`directory_changed` carries `{"directory":"/path"}` when the shell reports a new working directory
through OSC 7, which needs shell integration; `inspect`'s `current_directory` answers the same
question by polling and needs none.

Frames look like:

```json
{"version":2,"subscription_id":7,"event_sequence":123,"window_id":42,
 "event":{"type":"screen_changed","data":{}}}
```

Output data arrives in chunks of at most 64 KiB with start and end offsets and base64 bytes.
A replayable `client_fault` carries
only `fault_id`, `class`, and `quarantined` — never client bytes, panic payloads, paths, or
capability material.

Up to 32 subscriptions per process, 256 queued events each; the process replay ring is bounded by
4 MiB and 4,096 events. `--since-event` atomically replays retained matching events before live
delivery; if history is gone the first event is `overflow` with the gap and current sequences, and
the recovery is `inspect` plus `get-grid`. A slow client never blocks the UI or PTY thread — a full
queue collapses the dropped detail into one overflow range. `--all` bypasses target resolution.

## Vivid presenter inspection

```sh
vivido msg vivid sessions
vivido msg vivid surfaces
vivido msg vivid surface-status --session-id S --context-id C --surface-id U
vivido msg vivid tracks
vivido msg vivid track-status --session-id S --context-id C --surface-id U --track-id T
vivido msg vivid scene-status --session-id S
vivido msg wait vivid-track presentation-after --value 900 \
  --session-id S --context-id C --surface-id U --track-id T --channel-generation G
vivido msg vivid trace --tail --limit 64
vivido msg vivid trace --after 40 --follow --recovery-only
vivido msg vivid trace --around 420 --preceding 64 --following 16
```

The `wait vivid-track` condition is positional: `revision-after`, `milestones`,
`presentation-after`, `pts-after`, `clock-started`, `buffered-ended`, `channel-accepted`,
`channel-detached`, or `track-lost`. The first four also require `--value`. All of them need the
complete track identity and the current `--channel-generation`, because a wait is scoped to one
generation and must not be satisfied by a later one.

`trace` reads the bounded metadata journal (4,096 events or 2 MiB). Selectors are mutually
exclusive, and `--follow` accepts only the forward `--after` form. Every batch carries a `selection`
object describing how it was captured. No credentials, media bytes, or frame hashes ever appear
there — and the automation service never returns root secrets, channel keys, authenticators, or
resume material by any route.

## Diagnostics

```sh
vivido doctor --target NAME --json
vivido msg diagnose --trace-limit 128
vivido debug-bundle --target NAME --output vivido-debug.zip
```

`diagnose` captures window, renderer, presenter, track, flow, connection-health, and bounded recent
trace metadata in one event-loop turn; it does not wait for rendering or transport, so asynchronous
metrics carry an age. Its trace is the *newest* `trace_limit` events.

`debug-bundle` writes an owner-only, atomic, versioned ZIP. It is metadata only by default —
screenshot, grid, transcript, and bounded log content each need their own explicit `--include-*`
flag. Ask before collecting content that was not requested.

## Headless sessions

```sh
eval "$(vivido --headless --session build)"
vivido --headless --session build --foreground --headless-size 1280x720px
vivido list
vivido msg --target build quit
vivido kill-session --target build
```

The parent does not exit until the daemon is actually serving, so exit status 0 means the session is
usable; a startup failure is one bounded diagnostic line and a nonzero exit. If the daemon neither
succeeds nor fails within 30 seconds the parent reports a timeout and leaves it running rather than
killing a session that may still be initialising a software renderer.

A session name is 1–64 ASCII letters, digits, `.`, `-`, or `_`, and may not start with `.`.
`--headless-size` takes `COLUMNSxLINES` (through the real font metrics) or `WIDTHxHEIGHTpx`
(the render surface directly, usually what a screenshot-driven caller wants); the fallback is
1280x720 at scale factor 1.0. A session starts with exactly one window, then outlives it and every
later one. Prefer `vivido msg quit`, which lets the daemon remove its own socket, registry, and log
file; `kill-session` is the forceful path. `vivido list` also reaps rendezvous files whose owning
process is gone.

## Protocol notes

The endpoint carries newline-delimited UTF-8 JSON, one value per frame. Requests are capped at
1 MiB, replies and events at 16 MiB. Every connection begins with `hello`; a legacy raw enum frame,
a malformed first frame, a non-`hello` first request, or an unsupported version gets a structured
error and the connection closes. There is no compatibility mode for the former unversioned protocol
and none for version 1.

Request IDs are per-connection, up to 64 active; reusing an active one returns
`duplicate_request_id`. Requests are full duplex — responses may arrive out of order, so correlate
by `id` and distinguish events by `subscription_id`. A connection has one serialized writer, so
frames never interleave. Vivido allows 32 active connections.

The endpoint is an owner-only Unix socket with mode `0600`, or a named pipe with an owner-and-SYSTEM
DACL on Windows that rejects remote clients. Both ends verify the peer's process owner in addition
to the mode or ACL.
