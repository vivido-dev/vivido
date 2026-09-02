# OSC 7 and OSC 22 support

Design notes for the two VT additions described in [`vt-sequences.md`](vt-sequences.md):
**OSC 7** (shell working directory) and **OSC 22** (pointer shape).

## Context

The VT-sequence audit found both unimplemented: `vte` 0.15 does not dispatch OSC 7 at all,
and OSC 22 is parsed by `vte` but landed in Vivido's unimplemented
`Handler::set_mouse_cursor_icon` no-op. With them, shells can report their current directory
— which works on Windows and reflects shell-tracked state where the
`foreground_process_path` OS probe fails or returns `None` — and applications can request a
pointer shape such as `progress` during long operations.

## Design

- **Parser reuse.** OSC 7 is captured by widening `OscNotificationParser`
  (`src/osc_notification.rs`) to a general `OscMessage` output. The existing scanner already
  handles split reads, OSC-inside-DCS nesting, BEL/ST terminators, 8 KiB bounding with
  resynchronization, and a nested-`vte` top-level confirmation; duplicating it would be
  strictly worse. The scanner is observe-only — bytes still reach `vte`, which swallows
  OSC 7 harmlessly.
- **Host validation.** A report is accepted only when its host is empty, `localhost`
  (ASCII case-insensitive), or this machine's hostname (case-insensitive; `gethostname` on
  Unix, `COMPUTERNAME` on Windows, cached for the process lifetime). Reports naming another
  host — a shell behind `vvssh` or plain `ssh` — are ignored so a remote path never becomes a
  local spawn directory.
- **URL shape.** `file://` scheme only (exact lowercase), host split at the first `/` after
  the prefix, path percent-decoded with `percent-encoding` (already in the dependency graph
  through winit) and rejected unless it decodes to control-free UTF-8.
- **Cursor precedence.** One shared resolver decides the visible pointer:
  message bar → highlighted hint → application OSC 22 shape → mouse-report arrow (`Default`)
  → `Text`. The application shape outranks the mouse-report arrow because the primary OSC 22
  users are TUI applications that also enable mouse reporting. `vte::ansi::CursorIcon` and
  `winit::window::CursorIcon` are the same `cursor-icon` crate type, so `src/terminal/`
  stores the re-exported form and stays winit-free.
- **Reset.** `RIS` clears both states; it also emits `MouseCursorDirty` now, fixing a
  pre-existing staleness where resetting mouse-report modes never refreshed the pointer.
- **Events.** An accepted, changed OSC 7 report emits `Event::WorkingDirectory`, which the
  event processor forwards to IPC subscribers as `directory_changed`. The working directory
  itself is polled from `Term` by the consumers below, so the event carries no window state.

## Consumers

- `WindowContext::current_directory` (IPC `inspect`'s `current_directory`) prefers the OSC 7
  path and falls back to the foreground-process probe. `automation_inspect` reads the value
  through its already-held terminal lock — the terminal mutex is not reentrant.
- New windows inherit the OSC 7 directory before falling back to the probe; this is the
  first working-directory source on Windows.
- Hint- and bell-launched commands (`spawn_daemon`) keep the probe: that pre-`exec` path has
  no terminal access, and locally the two sources agree.

Out of scope: resolving relative paths for hints or hyperlinks against the reported
directory.

## Verification

From `vivido/`:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Manual smoke: `printf '\e]22;progress\a'` / `printf '\e]22;text\a'`;
`printf '\e]7;file://%s%s\a' "$HOST" "$PWD"` then IPC `inspect` → `current_directory` (a
subscribed client sees `directory_changed`); a `vvssh` session's reports are ignored.
