# Vivido agent automation IPC

Vivido exposes an owner-only automation service on Linux, macOS, and Windows. It is intended for
agents, test runners, and local programs which need to control and observe both Vivido and terminal
applications running inside it. The service is available from windowed instances and from
[headless sessions](headless.md) alike; a headless session is the deterministic target for CI.

The CLI is the stable shell interface:

```sh
vivido msg capabilities
vivido msg list-windows
vivido msg inspect --window-id 42
vivido msg wait text 'ready>' --window-id 42
vivido msg key Enter --window-id 42
```

The canonical agent loop is discover → inspect → capture a sequence → act with an explicit wait →
follow trace → diagnose or bundle:

```sh
vivido list --all --json
vivido doctor --target vivido-1234 --json
vivido msg --target vivido-1234 diagnose --trace-limit 128
vivido msg --target vivido-1234 vivid trace --after 40 --follow --recovery-only
vivido debug-bundle --target vivido-1234 --output vivido-debug.zip
```

For a multi-step task, `run-plan` keeps one owner-verified IPC connection open for the complete
workflow. It reads JSON from standard input by default or from `--file PATH`, and emits compact
NDJSON events for the plan, each step, and final status:

```json
{
  "version": 1,
  "steps": [
    {
      "id": "inspect",
      "method": "inspect",
      "params": {"window_id": 42},
      "bind": {"target": "/window/window_id"}
    },
    {
      "id": "click",
      "method": "mouse",
      "params": {"action": {"click": {"button": "left", "position": {
        "relative_x": 0.5, "relative_y": 0.5, "mods": [], "route": "ui",
        "target": {"window_id": {"$ref": "target"}}
      }}}},
      "verify": {"window_id": {"$ref": "target"}, "frame_changed": true,
                 "screenshot": true, "timeout": 30000}
    }
  ]
}
```

Plans contain 1 through 256 bounded linear steps. `bind` maps a plan-local alias to a JSON Pointer
in that step's result; a later JSON value containing only `{"$ref":"alias"}` substitutes it. An
optional `when` compares one alias with an exact JSON `equals` value. `on_error` is `abort` by
default or `continue`. There are no loops, scripts, persistent aliases, or forward references.
Request parameters are not repeated in trace events. `--dry-run` validates without executing;
`--preflight` executes observation methods only and reports mutations or unavailable dependencies
as skipped.

## Endpoint discovery and targeting

The endpoint is an owner-only Unix socket with mode `0600` on Unix, and a named pipe with an
owner-and-SYSTEM-only DACL on Windows. Both ends verify the peer's process owner in addition to the
endpoint's mode or ACL, and the Windows pipe rejects remote clients. `--socket PATH` therefore takes
a filesystem path on Unix and a pipe path on Windows. Vivido limits the server to 32 active
connections. The service is offered when `general.ipc_socket` is enabled, which is the default;
`--socket` and `--headless` enable it regardless of configuration. Every headed process also
enables and registers this endpoint: `--automation-name NAME` chooses its name, otherwise it is
`vivido-PID`. `vivido list` retains its legacy headless-only text output; `vivido list --all
--json` discovers both headed and headless instances on every platform.

`vivido msg` resolves its endpoint in this order:

1. `--socket PATH`;
2. `--target NAME`, else inherited `VIVIDO_SESSION` — an explicitly named registered instance never
   silently falls through to a different instance, so a name that is not running is an error;
3. inherited `VIVIDO_SOCKET`, if it still connects;
4. the only live headless session, when there is exactly one;
5. on Unix, the newest live windowed instance on the current display.

Window-targeted CLI commands then resolve their window in this order:

1. `--window-id ID`;
2. inherited `VIVIDO_WINDOW_ID`;
3. the currently focused Vivido window.

Use `list-windows` when a caller did not inherit the per-window environment. An explicit missing ID
returns `window_not_found`; a command requiring the focused fallback returns `no_focused_window`
when no Vivido window is focused. A headless session has no operating-system focus, so pass an
explicit `--window-id` there whenever more than one window exists. `subscribe --all` intentionally
bypasses target resolution.

## Version 2 wire protocol

The endpoint carries newline-delimited UTF-8 JSON. Each frame is one JSON value followed by `\n`.
Request frames are limited to 1 MiB and reply/event frames to 16 MiB.

Every connection must begin with `hello`:

```json
{"version":2,"id":1,"method":"hello","params":{}}
```

The response advertises the server version, protocol version, whether this instance is headless,
its optional session and `automation_name`, methods, event kinds, stable error codes, and limits:

```json
{"version":2,"id":1,"ok":true,"result":{"server_version":"0.0.0","protocol_version":2,"headless":true,"session":"build","methods":[],"event_kinds":[],"limits":{}}}
```

The additive `method_capabilities` array classifies every method as `observe`, `input`, `window`,
`config`, `process`, `lifecycle`, or `extension`, and records `mutating` and `host_claimed` flags.
The original `methods` array remains the compatibility authority. An unclassified host claim is a
mutating `extension`.

`headless` is `false` and `session` is `null` for a windowed instance. `automation_name` addresses
either form uniformly. All are fixed at startup.

A legacy raw enum frame, malformed first frame, non-`hello` first request, or unsupported version
gets a structured error and the connection closes. There is no compatibility mode for the former
unversioned protocol and no version-1 compatibility mode.

Subsequent requests use the same envelope:

```json
{"version":2,"id":17,"method":"inspect","params":{"window_id":42}}
```

Correlated success and failure envelopes are:

```json
{"version":2,"id":17,"ok":true,"result":{}}
{"version":2,"id":17,"ok":false,"error":{"code":"window_not_found","message":"..."}}
```

Request IDs are scoped to a connection. Up to 64 may be active at once. Reusing an active ID
returns `duplicate_request_id`; an ID may be reused after its response. Requests are full duplex:
responses may arrive out of order, but a connection has one serialized writer so JSON frames never
interleave. Programs must correlate responses by `id` and distinguish event frames by their
`subscription_id` field.

Stable protocol errors are `unsupported_version`, `invalid_request`, `invalid_params`,
`duplicate_request_id`, `limit_exceeded`, `window_not_found`, `no_focused_window`, `unsupported`,
`invalid_state`, `timeout`, `sequence_gap`, `pty_closed`, `resize_mismatch`, `focus_denied`,
`regex_invalid`, `subscription_overflow`, and `client_fault`. Errors may include a `data` object with recovery
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
- `ping {}` / `vivido msg ping`: liveness request; returns `{"pong":true}`.
- `reset_terminal {"window_id":ID}` / `vivido msg reset-terminal`: discards partial parser state,
  returns to the primary screen, clears client-controlled input modes and the Vivid scene, and
  resumes a quarantined PTY. Primary scrollback and the stable window ID are preserved.
- `restart_terminal {"window_id":ID}` / `vivido msg restart-terminal`: transactionally creates a
  replacement PTY and Vivid service from the pane's retained launch options. The window ID and any
  embedding Vivida workspace/tab/split position remain stable; a failed replacement leaves the
  existing quarantined pane available for another recovery attempt.
- `quit {}`: shuts the whole instance down, closing every window. For a headless session this is
  the graceful stop, and it lets the daemon remove its own endpoint and registry; `vivido
  kill-session` is the forceful alternative.
- `unsubscribe {"subscription_id":ID}`: wire-only cancellation for a subscription on the same
  connection.
- `create_window`: synchronously constructs a complete window and returns `{"window_id":ID}`.
  The CLI is `vivido msg create-window` with its existing window, command, directory, hold, title,
  class, and config options. `ipc_window_id` is optional and must be unique. Assigned IDs are small
  and monotonic within a process, starting at 1, and are never reused; a caller that names its own
  ID keeps it, and automatic assignment then steps past it. The value is opaque — discover it, never
  predict it — but it is deliberately small enough to be an agent-mesh address segment, which is a
  one-based `u32`. A window whose ID is outside that range works normally here and simply inherits
  no `AGENT_MESH_ADDRESS`. The response does not
  wait for the first rendered frame. In a headed Windows/Linux process this method creates and
  activates a tab in the existing top-level window; each tab's returned window ID remains its
  stable public identity. macOS and headless sessions retain their existing window semantics.
  `--vivid-target desktop` creates a `desktop-surface-v1` window
  instead of the default `terminal-surface-v1` one; a desktop window has no grid, no anchors, and no
  shell, so terminal-shaped methods do not apply to it.
- `config` and `get_config`: back the existing `config` and `get-config` commands. Configuration
  updates now always receive a correlated response. The special config ID `-1` means all/global.
- `typing {"text":"...","window_id":ID}`: writes literal UTF-8 bytes without paste handling or an
  appended Enter. Text is limited to 1 MiB. Success is sent only after every byte is written to the
  PTY master, with a five-second write timeout.
  `typing`, `key`, and `paste` accept CLI `--report`, which prints the resolved window, encoded byte
  count, input sequence, and successful PTY-write completion. It explicitly reports that
  application consumption was not observed.
- `get_text {"rows":N,"window_id":ID}`: returns `{"text":"..."}`. With no `rows`, text is the
  visible viewport at its current scroll position. `rows` accepts 1 through 1000 and reads newest
  physical rows at the live bottom, including scrollback. The CLI writes text exactly, without an
  added newline. Styling, cursor, media, search, and message overlays are excluded.
- `screenshot {"window_id":ID}`: returns the private PNG `path` together with `window_id`, captured
  `frame_sequence`, physical `width`/`height`, `scale_factor`, and cell metrics. The CLI prints only
  the path by default or the complete result with `--json`. The PNG is the last successfully
  presented client-area frame at physical
  resolution and includes terminal rendering, cursor, selection, Vivido overlays, and Vivid media.
  It excludes OS decorations and desktop content. Straight alpha is preserved. The persistent temp
  file has mode `0600` on Unix and lives in the per-user temporary directory on Windows; its caller
  owns cleanup. Headless sessions render offscreen and support this identically. A resize
  invalidates the stored frame until another
  frame is presented. Only one readback per window may run at once and raw allocation is capped at
  256 MiB.
- `vivido msg capture` is a client-side composite over existing methods. `--activate` first calls
  an advertised `vivida_activate_pane`, `--after-frame N` requires a newer frame, `--stable[=TIME]`
  waits for semantic screen stability (250 ms by default), and the final result is screenshot JSON.

### Host-claimed methods

An in-process host that embeds Vivido as a library — a shell that arranges terminals into its own
tabs, splits, or panels — serves this endpoint on Vivido's behalf and may claim methods. A claimed
request is handed to the host verbatim instead of being dispatched, so the host can answer from
state Vivido does not have, and can take over a built-in method: a host that claims `create_window`
places the new window in its own layout rather than letting Vivido build a top-level one.

Standalone Vivido is itself such a host on Windows and Linux. Its `list_windows` response reports
only the active tab as visible; focusing an inactive window ID selects and reveals that tab first,
and a resize preserves the requested terminal content size while resizing every tab consistently.

Claimed names appear in the `hello` handshake's `methods` array alongside Vivido's own, so
`capabilities` remains the single description of what an endpoint answers. Claiming a name Vivido
already advertises replaces its handler without duplicating the entry. Everything the host does not
claim behaves exactly as documented here.

Errors, limits, framing, request IDs, and subscriptions are unchanged: a claimed method is an
ordinary request that a different component answers. A host that never replies leaves the client
waiting until it disconnects, the same as any unanswered request.

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
  `drag`, `path`, and `scroll`. A position contains exactly one zero-based cell pair
  (`cell_column`,`cell_row`) or physical-pixel pair (`x`,`y`), plus `mods`, `route`, and `target`.
  Button actions add `button` (`left`, `middle`, or `right`). A path contains 2 through 1,000
  physical-pixel `{x,y}` points plus one button, modifier set, route, and target; it performs one
  press/move/release gesture in one request. Scroll adds finite `vertical` and `horizontal` amounts
  and is capped at 1000 reports. Application routing requires active terminal mouse reporting and
  the live-bottom viewport. SGR pixel mouse mode preserves exact physical coordinates; other mouse
  modes resolve them to terminal cells. UI routing can select text, invoke mouse bindings, follow
  links, or report to the application as normal UI input would without requiring OS focus.
  A position may instead contain `relative_x` and `relative_y`, both finite fractions from 0
  through 1; they are atomically mapped to the current physical client area. `mouse path
  --duration TIME` paces one gesture for 1 ms through 30 seconds, with at most one paced gesture per
  window. Vivido always releases the held button on completion, failure, disconnect, cancellation,
  or window loss. `--wait-frame` delays success until a frame newer than the pre-gesture frame and
  accepts the normal bounded `--timeout`. Omitting duration preserves the original one-write path.
- `resize {"columns":C,"rows":R,"width":null,"height":null,"target":{...}}` requests exact grid
  dimensions; replace the grid pair with `width`/`height` for exact physical client pixels. Grid
  size is at least 2 by 1 and must fit renderer and PTY limits. Only one resize per window is active.
  Success waits for both the OS size and terminal/PTY size; failure after five seconds is
  `resize_mismatch` with requested/actual details where available.
- `set_geometry {"x":X,"y":Y,"width":W,"height":H,"target":{...}}` moves the outer frame to a
  physical screen position, resizes the client area to exact physical pixels, or both. Each pair is
  all-or-nothing and at least one must be present. Unlike `resize`, it returns as soon as the
  requests are issued rather than waiting for the windowing system, because a caller driving a
  layout sends these continuously while dragging; subscribe to `moved` and `resized` for
  confirmation. The result reports the resulting `x`, `y`, `width`, and `height`, with a null
  position when the windowing system refuses to report one. Positioning a headless window returns
  `unsupported`, since it is on no screen.
- `set_visible {"visible":BOOL,"target":{...}}` maps or unmaps a window without destroying it.
  Mapping deliberately does not take the keyboard, so an external layout owner can reveal a window
  while its own stays focused.
- `set_level {"level":"normal"|"always_on_top"|"always_on_bottom","target":{...}}` sets stacking
  relative to other windows, including other applications'.
- `focus {"window_id":ID}` requests real operating-system activation. It succeeds only after an
  actual focused event and otherwise returns `focus_denied` after two seconds. Vivido never
  synthesizes terminal focus state. On Windows, the CLI makes a best-effort
  `AllowSetForegroundWindow` grant to the owner-verified server process before requesting focus;
  Windows foreground-lock rules may still deny activation. On Wayland, the request uses
  `xdg_activation_v1` to obtain and
  apply a compositor-approved client activation token when that protocol is available. A headless
  session has no compositor, so `focus` cannot succeed there.
- `signal {"signal":"INT","target":{...}}` accepts `INT`, `TERM`, `HUP`, `QUIT`, `TSTP`, `CONT`,
  `WINCH`, `KILL`, and `STOP`. It sends only the explicitly named signal to the current foreground
  process group, falling back to the PTY child group. KILL and STOP have no implicit aliases.

### Discovery and inspection

- `list_windows {}` returns `{"windows":[...]}` sorted by monotonic `creation_index`. Each entry
  contains window ID, title, focus/occlusion/visibility/hold state, grid/pixel dimensions, outer
  frame position, process state, and current screen/frame/output sequences.
- `inspect {"window_id":ID}` returns the list entry plus cell dimensions, scale, scrollback,
  display offset, primary/alternate screen, terminal mode names, cursor, selection, shell PID,
  foreground process group, optional executable basename/current directory, echo state, exit
  status, global event sequence, and effective automation limits. It never returns process
  arguments, environment values, Vivid root/resume secrets, channel authenticators, or derived
  capabilities. `current_directory` prefers the shell's OSC 7 report when its host is this
  machine and falls back to the foreground-process probe; over Windows OSC 7 is the only
  source.
- `list_windows`, `inspect`, and `diagnose` include `client_health` (`healthy`, `quarantined`, or
  `recovering`) and an optional bounded `last_client_fault` containing only an opaque fault ID,
  fault class, and fixed diagnostic text.
- `diagnose` captures window, renderer, presenter, track, flow, connection-health, and bounded
  recent-trace metadata in one event-loop turn. It does not wait for rendering or transport;
  asynchronous metrics carry an age.
- `vivido doctor --target NAME --json` combines registry identity, IPC responsiveness, renderer
  progress, and presenter health. `vivido debug-bundle` creates an owner-only, atomic versioned ZIP.
  Its default is metadata only; screenshot, grid, transcript, and bounded log content each require
  their corresponding explicit `--include-*` flag.

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

### Vivid Protocol 1.5 inspection

IPC v2 exposes the presenter without projecting 1.5 objects back into the removed source model:

- `vivid_sessions` and `vivid_surfaces` enumerate complete owners and stable surfaces.
- `vivid_surface_status` requires `session_id`, `context_id`, and `surface_id`.
- `vivid_tracks` enumerates immutable tracks.
- `vivid_track_status` adds `track_id` and returns track revision, channel generation, immutable
  kind/slot/mode/lane, lifecycle, generation-local milestones, media progress, cumulative and
  maximum flow counters, and playback state.
- `vivid_scene_status` requires `session_id` and returns surface-referencing nodes plus independent
  scene revision and target generation.
- `wait_vivid_track` requires the complete track identity, current `channel_generation`, condition,
  optional value, and millisecond `timeout`.

The CLI names those conditions `revision-after`, `milestones`, `presentation-after`, `pts-after`,
`clock-started`, `buffered-ended`, `channel-accepted`, `channel-detached`, and `track-lost`.
`vivid trace` reads or follows the bounded metadata journal (4,096 events or 2 MiB), with complete
owner filters, monotonic sequences, eviction gaps, recovery filtering, process/start anchors, and
no credentials, media bytes, or frame hashes.

A query without a selector retains its original oldest-first behavior. `--after SEQUENCE` reads
forward, `--tail --limit N` returns the newest matching events in chronological order, and
`--before SEQUENCE --limit N` returns the newest matches strictly before the cursor. An around
query uses independent bounded sides:

```sh
vivido msg vivid trace --around 420 --preceding 64 --following 16
```

The selectors are mutually exclusive, and follow mode accepts only the forward form. Every batch
contains a `selection` object describing how it was captured. `diagnose` uses tail selection, so
its trace is the newest `trace_limit` events rather than the oldest retained events.

Track control transitions emit one terminal applied or rejected event with the request ID, control
record sequence, record type, object ID, complete authenticated track identity when available, and
operation-specific before/after state. Covered operations are create, destroy, channel advance,
audio gain, play, pause, flush, and drain. Channel acceptance records the open request and record
sequence. Channel detachment records its clean/failed outcome, generation disposition, last record
metadata, and a typed bounded failure when present; a `track_lost` event repeats the complete
failure so it is independently diagnosable. Pre-authentication channel rejection never publishes
the claimed identity, authenticator, nonce, or other capability material.

There are no IPC v1 aliases for `vivid_sources`, `vivid_source_status`, `vivid_milestones`, or
`wait_vivid_source`. The wire service never returns root secrets, channel keys, authenticators, or
resume material through automation.

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
{"version":2,"subscription_id":7,"event_sequence":123,"window_id":42,"event":{"type":"screen_changed","data":{}}}
```

An event frame carries the same protocol version as requests and responses. Distinguish it by the
presence of `subscription_id`, not by `version`.

Kinds are `screen_changed`, `output`, `frame_presented`, `title_changed`, `directory_changed`,
`focus_changed`, `resized`, `moved`, `bell`, `child_exit`, `window_created`, `window_closed`, and
`overflow`, plus `client_fault` and `client_recovered`. The handshake's `event_kinds` is the
authority and is also the `--events` allowlist: a kind it does not list cannot be subscribed to by
name. A replayable `client_fault` contains the
window ID in the envelope and only `fault_id`, `class`, and `quarantined`; client bytes, panic
payloads, paths, and capability material are never included. `client_recovered` follows a completed
reset. Output data is split into
at most 64 KiB chunks with start/end offsets and base64 bytes. Screen-change data contains current
row replacements. `directory_changed` fires when the local shell reports a new working directory
through OSC 7 and carries `{"directory":"/path"}`. The process replay ring is bounded by both 4 MiB and 4,096 events.

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
with a newline unless `--json` requests its capture metadata, and `transcript --raw` prints exact
decoded bytes without a newline. Structured IPC errors make `vivido msg` exit nonzero and are
written to standard error.
