# Vivido VT sequence support

This document maps the VT/ANSI escape sequences Vivido understands, using the
[Ghostty VT reference](https://ghostty.org/docs/vt/reference) as the comparison baseline. It
covers everything on that page, plus the sequences Vivido supports beyond it.

Status legend:

| Marker | Meaning |
|---|---|
| ✅ | Fully supported |
| ⚠️ | Parsed but partial — accepted on the wire, state or rendering is incomplete |
| ❌ | Not supported — sequence is consumed silently (never passed through) |

## How to read this

Vivido does not hand-roll a parser. It feeds PTY bytes through `vte` 0.15 with the `ansi`
adapter, then implements the `vte::ansi::Handler` trait in `src/terminal/term/mod.rs`. Support
is therefore decided in three places:

1. **`vte`'s state machine** (`vte::ansi`) — decides whether a sequence is dispatched at all.
   Sequences it does not know are consumed and dropped with a debug log; they never error.
2. **`Term`'s `Handler` impl** (`src/terminal/term/mod.rs`) — decides what a dispatched
   sequence does. `vte`'s `Handler` trait gives every method an empty default, so a method
   Vivido does not override is a parse-only no-op.
3. **Pre-parser scanners** (`src/terminal/event_loop.rs`, `src/osc_notification.rs`) — intercept
   OSC 7 working-directory reports, OSC 9/99 desktop notifications, DCS queries, and Vivid media
   anchors before `vte` sees them.

## Control characters (C0)

| Name | Code | Status | Notes |
|---|---|---|---|
| BEL | `0x07` | ✅ | Emits `Event::Bell`. |
| BS | `0x08` | ✅ | Cursor left, stops at column 0. |
| HT | `0x09` | ✅ | Next tab stop; honors configured charsets. |
| LF | `0x0A` | ✅ | Scrolls at bottom margin. |
| VT | `0x0B` | ✅ | Treated as LF. |
| FF | `0x0C` | ✅ | Treated as LF. |
| CR | `0x0D` | ✅ | Column 0. |
| SO | `0x0E` | ✅ | Invokes G1 charset. |
| SI | `0x0F` | ✅ | Invokes G0 charset. |
| SUB | `0x1A` | ⚠️ | Cancels an in-progress escape sequence; deliberately displays nothing (Alacritty/Kitty parity; xterm shows a replacement glyph). |
| NUL, ENQ, others | — | ❌ | Ignored. ENQ triggers no answer-back. |
| DEL | `0x7F` | ❌ | Ignored. |

## ESC sequences

| Name | Sequence | Status | Notes |
|---|---|---|---|
| DECSC | `ESC 7` | ✅ | Saves full cursor (position, attrs, charsets, origin mode). |
| DECRC | `ESC 8` | ✅ | |
| IND | `ESC D` | ✅ | |
| RI | `ESC M` | ✅ | Scrolls down at top margin. |
| NEL | `ESC E` | ✅ | CR + LF. |
| HTS | `ESC H` | ✅ | Sets tab stop at cursor column. |
| RIS | `ESC c` | ✅ | Full reset: grids, modes, tabs, title stack, selection, keyboard-mode stack. |
| DECKPAM | `ESC =` | ✅ | |
| DECKPNM | `ESC >` | ✅ | |
| DECALN | `ESC # 8` | ✅ | Fills screen with `E`; does not move the cursor or reset margins. |
| DECID | `ESC Z` | ✅ | Replies like primary DA. |
| Charset designation | `ESC ( 0`, `ESC ) B`, … | ✅ | G0–G3 via `( ) * +`. Two charsets only: ASCII (`B`) and DEC Special Graphics / line drawing (`0`). |
| DECDHL/DECDWL/DECSWL | `ESC # 3/4/5/6` | ❌ | Double-height/width lines unsupported. |

## CSI — cursor movement

| Name | Sequence | Status | Notes |
|---|---|---|---|
| CUU / CUD | `CSI Pn A / B` | ✅ | |
| CUF / CUB | `CSI Pn C / D` | ✅ | |
| CNL / CPL | `CSI Pn E / F` | ✅ | |
| CUP / HVP | `CSI y ; x H / f` | ✅ | Origin-mode aware (relative to margins). |
| CHT / CBT | `CSI Pn I / Z` | ✅ | |
| VPA | `CSI Py d` | ✅ | |
| VPR | `CSI Pn e` | ✅ | |
| HPA | `CSI Px \` | ✅ | |
| HPR | `CSI Pn a` | ✅ | |
| SCOSC / SCORC | `CSI s / CSI u` | ✅ | ANSI.SYS-style save/restore. Note `CSI s` therefore cannot be DECSLRM. |

## CSI — editing, erasing, scrolling

| Name | Sequence | Status | Notes |
|---|---|---|---|
| ICH | `CSI Pn @` | ✅ | |
| DCH | `CSI Pn P` | ✅ | |
| ECH | `CSI Pn X` | ✅ | |
| IL | `CSI Pn L` | ✅ | Only inside the scroll region. |
| DL | `CSI Pn M` | ✅ | Only inside the scroll region. |
| ED | `CSI Ps J` | ✅ | 0, 1, 2, 3 (3 clears scrollback). On Windows, ED 3 also invalidates the Vivid scene (ConPTY `cls` translation). |
| EL | `CSI Ps K` | ✅ | 0, 1, 2. |
| SU / SD | `CSI Pn S / T` | ✅ | |
| REP | `CSI Pn b` | ✅ | Repeats the preceding character. |
| TBC | `CSI Ps g` | ✅ | 0 (current), 3 (all). |
| Restore default tabs | `CSI ? 5 W` | ⚠️ | Dispatched by `vte`; Vivido does not implement `set_tabs`, so it is a no-op. |
| DECSED / DECSEL | `CSI ? Ps J / K` | ❌ | No selective erase (DECSCA is not tracked). |

## CSI — modes

ANSI modes (`CSI Ps h/l`):

| Name | Mode | Status | Notes |
|---|---|---|---|
| IRM | 4 | ✅ | Insert mode shifts cells right. |
| LNM | 20 | ✅ | LF also does CR. |
| KAM, others | — | ❌ | Unknown ANSI modes are ignored. |

DEC private modes (`CSI ? Ps h/l`):

| Name | Mode | Status | Notes |
|---|---|---|---|
| DECCKM | 1 | ✅ | Application cursor keys. |
| DECCOLM | 3 | ⚠️ | Side effects only: resets margins, clears screen. No 80/132-column switch (grid size is unchanged). DECRPM reports *not supported*. |
| DECOM | 6 | ✅ | Homes the cursor when set. |
| DECAWM | 7 | ✅ | On by default. |
| Cursor blink | 12 | ✅ | |
| DECTCEM | 25 | ✅ | Show/hide cursor; shown by default. |
| X11 mouse click | 1000 | ✅ | Mouse protocols are mutually exclusive. |
| Cell-motion mouse | 1002 | ✅ | |
| All-motion mouse | 1003 | ✅ | |
| Focus reporting | 1004 | ✅ | Sends `CSI I` / `CSI O`. |
| UTF-8 ext mouse | 1005 | ✅ | |
| SGR ext mouse | 1006 | ✅ | |
| Alternate scroll | 1007 | ✅ | On by default. |
| SGR **pixel** mouse | 1016 | ✅ | Vivido extension point beyond stock `vte`: coordinates reported in pixels. |
| Urgency hints | 1042 | ✅ | On by default. |
| Alt screen + cursor save | 1049 | ✅ | Swaps grids, resets alt contents on entry, restores cursor and keyboard-mode stack on exit. Emits `VividScreenSwap`. |
| Alt screen | 47 / 1047 | ❌ | Not mapped by `vte` 0.15; ignored. |
| Save/restore cursor | 1048 | ❌ | Ignored. Use `ESC 7/8` or `CSI s/u`. |
| urxvt ext mouse | 1015 | ❌ | Ignored. |
| Bracketed paste | 2004 | ✅ | Wraps pastes in `CSI 200~` … `CSI 201~` on input. |
| Synchronized output | 2026 | ✅ | Handled inside `vte`'s processor: output is batched until `CSI ? 2026 l` or a timeout, then rendered as one frame. DECRPM reports *reset*. |

## CSI — identification and reports

| Name | Sequence | Status | Notes / reply |
|---|---|---|---|
| DA1 | `CSI c` | ✅ | `ESC[?6c` (VT102 w/ video). |
| DA2 | `CSI > c` | ✅ | `ESC[>0;{version};1c`; version derived from the crate version. |
| DA3 | `CSI = c` | ✅ | `DCS !|00000000 ST`. |
| DSR | `CSI 5 n` | ✅ | `ESC[0n`. |
| CPR | `CSI 6 n` | ✅ | `ESC[{row};{col}R`. |
| DECRQM (private) | `CSI ? Ps $ p` | ✅ | Replies `CSI ? Ps ; Pm $ y` (0 unknown, 1 set, 2 reset) for every named mode plus 1016. |
| DECRQM (ANSI) | `CSI Ps $ p` | ✅ | IRM, LNM only; others reply 0. |
| DECSCUSR | `CSI Ps SP q` | ✅ | 0–6 → block/underline/beam × blinking. |
| XTSHIFTESCAPE | `CSI > Ps s` | ❌ | Unhandled. |
| Window ops | `CSI Ps t` | ⚠️ | 14 (pixel size, replies `CSI 4 ; h ; w t`), 18 (char size, replies `CSI 8 ; rows ; cols t`), 22/23 (title stack push/pop) are supported. All other XTWINOPS (resize, iconify, raise, title query) are unhandled. |
| Bidirectional SCP | `CSI Ps ; Ps SP k` | ⚠️ | Dispatched by `vte`; `set_scp` is not implemented — no-op, no BiDi state. |

## SGR — character attributes (`CSI … m`)

| Attribute | Codes | Status | Notes |
|---|---|---|---|
| Reset | 0 | ✅ | |
| Bold / dim | 1 / 2 | ✅ | Cancel via 21 (bold) and 22 (both). |
| Italic | 3 / 23 | ✅ | |
| Underline styles | 4, 4:2, 4:3, 4:4, 4:5 / 24 | ✅ | Single, double, undercurl, dotted, dashed; mutually exclusive. Underline color via 58/59 (incl. `58:2:r:g:b` colon form and `38`-style sub-params). |
| Slow/fast blink | 5 / 6 / 25 | ⚠️ | Parsed by `vte` but no cell flag exists — silently unhandled. |
| Reverse | 7 / 27 | ✅ | |
| Hidden | 8 / 28 | ✅ | |
| Strikethrough | 9 / 29 | ✅ | |
| Foreground | 30–37, 39, 90–97 | ✅ | |
| Background | 40–47, 49, 100–107 | ✅ | |
| 256-color / truecolor | `38;5;n`, `38;2;r;g;b` (and `48…`) | ✅ | Colon-separated variants accepted. |
| Overline | 53 | ❌ | Not parsed by `vte`. |

Full-width and zero-width (combining) characters are handled in the grid; wide glyphs that do
not fit at end-of-line get a leading spacer cell when DECAWM is on.

## OSC sequences

| Name | Number | Status | Notes |
|---|---|---|---|
| Title | 0, 2 | ✅ | `Event::Title`; OSC 0 behaves like 2 (no separate icon title). |
| Icon title | 1 | ❌ | |
| Palette set/query | 4 | ✅ | Set `rgb:`, `#rrggbb`, or `?` query (reply `OSC 4 ; idx ; rgb:…`). |
| Special colors | 5 | ❌ | |
| Current working directory | 7 | ✅ | Bounded (8 KiB) pre-parser. `file://` URLs only, percent-decoded; reports whose host is empty, `localhost`, or this machine update the shell working directory (new-window cwd, IPC `inspect`, `directory_changed` events); foreign hosts (vvssh/ssh) are ignored. |
| Hyperlinks | 8 | ✅ | `id=` parameter honored; stored per cell; opened through hints. |
| Desktop notification | 9 | ✅ | Bounded (8 KiB) pre-parser; macOS/Windows notification workers; rate-limited. Gated by `terminal.osc_notifications` (default on). Numeric subfamilies are rejected. |
| Progress state | 9;4 | ❌ | Explicitly ignored. |
| Kitty notifications | 99 | ✅ | Payload assembly, `a=`, `f=`, `o=`, `u=`, `s=`, `i=`/`d=` (close, focus, query) subsets, bounded and rate-limited. |
| FG / BG / cursor color | 10, 11, 12 | ✅ | Set and `?` query; multiple colors per sequence accepted up to index 12. |
| Pointer / Tek / highlight colors | 13–19 | ❌ | |
| Kitty color protocol | 21 | ❌ | |
| Pointer shape | 22 | ✅ | `vte` validates the shape; Term stores it and emits `MouseCursorDirty`. Precedence: message bar / hint highlight, then the app-requested shape, then the mouse-report arrow, then the text caret. Cleared by RIS. |
| Cursor shape | 50 | ✅ | `CursorShape=0/1/2` only. |
| Clipboard | 52 | ✅ | Copy and paste, `c`/`s`/`p` selections, base64. Gated by `terminal.osc52` (default `OnlyCopy`). |
| Reset palette | 104 | ✅ | All indexes or a list. |
| Reset special colors | 105 | ❌ | |
| Reset FG / BG / cursor | 110, 111, 112 | ✅ | |
| Reset pointer/Tek/highlight | 113–119 | ❌ | |
| Shell integration | 133 | ❌ | |
| Session color scheme (ConPTY) | 9;1 / 9;… variants | ❌ | |

## DCS, APC, and media protocols

| Name | Sequence | Status | Notes |
|---|---|---|---|
| DECRQSS | `DCS $ q … ST` | ✅ | Intercepts before `vte`. Reports `m` (full SGR status), `r` (DECSTBM), `"p` (`61;1`), `"q` (`0`); anything else gets the error reply `DCS 0 $ r ST`. Bodies bounded to 4 KiB, then resynchronized. |
| XTGETTCAP | `DCS + q … ST` | ✅ | Reports `TN=xterm-vivido`, `Co=256`, `RGB=8`, `Tc`. |
| XTSETTCAP | `DCS + p … ST` | ⚠️ | Parsed and hex-validated, then ignored — applications cannot change the identity. |
| Sixel | `DCS … q … ST` | ⚠️ | Consumed cleanly (never rendered as garbage); no decoder yet. `terminal/graphics.rs` defines protocol-neutral `GraphicsCommand` extension points (Sixel/Kitty/custom) that nothing currently emits. |
| Kitty graphics | `APC G … ST` | ⚠️ | Silently consumed by the parser; not rendered. |
| **Vivid 1.5 anchors** | `ESC _ VIVID ; 3 ; … ST` (ConPTY path: bare `VIVID;3;…;VIVID-END`) | ✅ | The authenticated media anchor is the only escape Vivido adds to the PTY stream. The event loop accepts either bounded envelope so a remote Windows ConPTY hop can reach a macOS/Linux presenter, attaches the anchor to the cursor's grid position (`Event::VividMarker`), and drives the side-channel media pipeline (images/video/audio). Media bytes themselves never touch the PTY. |

## Input-side protocols (what Vivido sends)

| Protocol | Status | Notes |
|---|---|---|
| DECCKM application cursor keys | ✅ | |
| DECKPAM/DECKPNM keypad modes | ✅ | |
| Bracketed paste | ✅ | Wraps on input when mode 2004 is set. |
| Focus in/out | ✅ | `CSI I` / `CSI O` under mode 1004. |
| Mouse encodings | ✅ | X10/legacy, UTF-8 extended, SGR extended; SGR reports switch to pixel coordinates under mode 1016. |
| Kitty keyboard protocol | ✅ | All five progressive-enhancement flags (disambiguate, event types, alternate keys, report-all-keys, associated text); `CSI = f;m u` set, `CSI > u` push, `CSI < u` pop, `CSI ? u` query; per-screen mode stacks on the alternate screen. Always compiled in (`kitty_keyboard: true`). |
| modifyOtherKeys | ⚠️ | `CSI > 4 ; Pm m` and the `CSI ? 4 m` query are parsed by `vte` but Vivido keeps no state and sends no reply; key encoding is driven by the kitty protocol flags instead. |

## Deliberate behavioral notes

- Unknown CSI actions, OSC codes, and modes are always **consumed silently** (debug-logged),
  never echoed or passed through — a broken sequence cannot desynchronize the parser.
- OSC notification and DCS parsing is bounded and rate-limited; oversized payloads are
  discarded and the scanner resynchronizes at the next terminator.
- ED 2/3 and screen swaps flush pending grid scrolls and emit Vivid scene events so anchored
  media stays aligned with the text model.
- Alternate-screen swap preserves the primary screen's keyboard-mode stack and restores it on
  exit, matching the kitty keyboard protocol's expectations.
