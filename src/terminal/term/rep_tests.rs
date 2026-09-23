use super::test::TermSize;
use super::*;
use crate::terminal::index::Side;
use std::sync::Mutex;

#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<String>>>);

impl EventListener for Events {
    fn send_event(&self, event: Event) {
        self.0.lock().unwrap().push(format!("{event:?}"));
    }
}

fn terminal(columns: usize, lines: usize, setup: &[u8]) -> Term<Events> {
    let mut term = Term::new(Config::default(), &TermSize::new(columns, lines), Events::default());
    term.advance(&mut ansi::Processor::new(), setup);
    term.reset_damage();
    term.event_proxy.0.lock().unwrap().clear();
    term
}

fn assert_same(actual: &Term<Events>, expected: &Term<Events>) {
    for (a, b) in [(&actual.grid, &expected.grid), (&actual.inactive_grid, &expected.inactive_grid)]
    {
        // Storage's test equality requires removing spare scrollback allocation first.
        let (mut a_contents, mut b_contents) = (a.clone(), b.clone());
        a_contents.truncate();
        b_contents.truncate();
        assert_eq!(a_contents, b_contents, "grid contents/history/display offset");
        assert_eq!(a.cursor, b.cursor);
        assert_eq!(a.saved_cursor, b.saved_cursor);
        for line in a.topmost_line().0..=a.bottommost_line().0 {
            assert_eq!(a[Line(line)].occupied_len(), b[Line(line)].occupied_len(), "row {line}");
        }
    }
    assert_eq!(actual.mode, expected.mode);
    assert_eq!(actual.active_charset, expected.active_charset);
    assert_eq!(actual.scroll_region, expected.scroll_region);
    assert_eq!(actual.selection, expected.selection);
    assert_eq!(actual.damage.full, expected.damage.full);
    assert_eq!(actual.damage.lines, expected.damage.lines);
    assert_eq!(actual.damage.last_cursor, expected.damage.last_cursor);
    assert_eq!(*actual.event_proxy.0.lock().unwrap(), *expected.event_proxy.0.lock().unwrap());
}

#[test]
fn repeat_matches_scalar_cells_cursor_history_damage_and_events() {
    let setups: &[&[u8]] = &[
        b"",
        b"\x1b[?1049h",
        b"\x1b[?7l",
        b"\x1b[4h",
        b"\x1b(0",
        b"\x1b[1;3;4:3;31;44m",
        b"\x1b[58;5;44m",
        b"\x1b]8;id=rep-test;https://example.invalid\x1b\\",
        b"\x1b]8;id=rep-test;https://example.invalid\x1b\\abcdef\x1b]8;;\x1b\\\x1b[H",
        "界界界abc\x1b[H".as_bytes(),
        "e\u{301}e\u{301}abc\x1b[H".as_bytes(),
        b"1234567890\x1b[H",
        b"\x1b[2;3r\x1b[3;2H",
        b"\x1b[999;999H",
        b"\n\n\n\n\n\n\n\n\n\x1b[H",
    ];
    for (columns, lines) in [(2, 1), (5, 4), (80, 24), (120, 40)] {
        for setup in setups {
            for c in ['a', ' ', '~', 'q', '界', '\u{301}', '\u{7f}'] {
                for count in [0, 1, 2, 7, 100] {
                    let mut actual = terminal(columns, lines, setup);
                    let mut expected = terminal(columns, lines, setup);
                    for term in [&mut actual, &mut expected] {
                        term.selection = Some(Selection::new(
                            SelectionType::Simple,
                            Point::new(Line(0), Column(0)),
                            Side::Left,
                        ));
                        term.scroll_display(Scroll::Delta(2));
                    }
                    actual.repeat_char(c, count);
                    for _ in 0..count {
                        expected.input(c);
                    }
                    actual.flush_grid_scroll();
                    expected.flush_grid_scroll();
                    assert_same(&actual, &expected);
                }
            }
        }
    }
}

#[test]
fn repeat_preserves_wide_cleanup_and_resize_tracking() {
    for column in 0..5 {
        let mut actual = terminal(5, 3, "abc界界ab\x1b[H".as_bytes());
        let mut expected = terminal(5, 3, "abc界界ab\x1b[H".as_bytes());
        for term in [&mut actual, &mut expected] {
            term.grid[Line(0)][Column(column)].push_vivid_resize_tracking(7);
            term.goto(0, column);
        }
        actual.repeat_char('x', 25);
        for _ in 0..25 {
            expected.input('x');
        }
        actual.flush_grid_scroll();
        expected.flush_grid_scroll();
        assert_same(&actual, &expected);
    }
}

#[test]
fn repeated_scroll_precedes_markers_clears_swaps_and_resize() {
    for suffix in [b"\x1b[2J".as_slice(), b"\x1b[?1049h", b"\x1b[?1049l", b"\x1b[1T"] {
        let mut actual = terminal(5, 3, b"\x1b[3;5H");
        let mut expected = terminal(5, 3, b"\x1b[3;5H");
        actual.repeat_char('a', 100);
        for _ in 0..100 {
            expected.input('a');
        }
        for term in [&mut actual, &mut expected] {
            // Synthetic marker only; authentication is exercised by presenter integration tests.
            term.vivid_marker("rep-order-test".into());
            term.advance(&mut ansi::Processor::new(), suffix);
            term.resize(TermSize::new(7, 4));
            term.flush_grid_scroll();
        }
        assert_same(&actual, &expected);
    }
}

#[test]
fn rep_stream_matches_expansion_at_every_read_boundary() {
    let cases = [
        ("a", "\x1b[b\x1b[0b\x1b[2b", "aaaa"),
        ("a", "\x1b[100b", ""),
        ("界", "\x1b[3b", "界界界"),
        ("e\u{301}", "\x1b[3b", "\u{301}\u{301}\u{301}"),
        ("\x1b(0q", "\x1b[3b", "qqq"),
    ];
    for (prefix, rep, expansion) in cases {
        let expansion = if expansion.is_empty() { "a".repeat(100) } else { expansion.into() };
        for sync in [false, true] {
            let begin = if sync { "\x1b[?2026h" } else { "" };
            let end = if sync { "\x1b[?2026l" } else { "" };
            let input = format!("{begin}{prefix}{rep}{end}\x1b[6n\x1b[5n");
            let expanded = format!("{begin}{prefix}{expansion}{end}\x1b[6n\x1b[5n");
            let mut expected = terminal(5, 3, b"");
            ansi::Processor::<ansi::StdSyncHandler>::new()
                .advance(&mut expected, expanded.as_bytes());
            expected.flush_grid_scroll();
            for split in 0..=input.len() {
                let mut actual = terminal(5, 3, b"");
                let mut parser: ansi::Processor = ansi::Processor::new();
                parser.advance(&mut actual, &input.as_bytes()[..split]);
                parser.advance(&mut actual, &input.as_bytes()[split..]);
                actual.flush_grid_scroll();
                assert_same(&actual, &expected);
            }
            let mut actual = terminal(5, 3, b"");
            let mut parser: ansi::Processor = ansi::Processor::new();
            for byte in input.as_bytes() {
                parser.advance(&mut actual, &[*byte]);
            }
            actual.flush_grid_scroll();
            assert_same(&actual, &expected);
        }
    }
}

#[test]
fn maximum_rep_count_matches_scalar() {
    let mut actual = terminal(2, 1, b"a");
    let mut expected = terminal(2, 1, b"a");
    actual.repeat_char('a', usize::from(u16::MAX));
    for _ in 0..u16::MAX {
        expected.input('a');
    }
    actual.flush_grid_scroll();
    expected.flush_grid_scroll();
    assert_same(&actual, &expected);
}
