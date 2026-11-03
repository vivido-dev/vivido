use super::test::TermSize;
use super::*;
use crate::terminal::grid::Cursor;
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
    // Cells are compared resolved: equal content may be interned under different ids when the
    // two paths add combining characters in different steps.
    let resolve = |term: &Term<Events>, grid: &Grid<Cell>| {
        // Storage's test equality requires removing spare scrollback allocation first.
        let mut grid = grid.clone();
        grid.truncate();
        grid.map(|row, cell| term.serialized_cell(row, cell))
    };
    let cursor = |term: &Term<Events>, cursor: &Cursor<Cell>| {
        let (template, erase) = (&cursor.template, &cursor.erase);
        // Cursor cells never hold extra content, so an empty row resolves them.
        let row = Row::default();
        let cells = (term.serialized_cell(&row, template), term.serialized_cell(&row, erase));
        (cursor.point, cursor.charsets, cursor.input_needs_wrap, cells)
    };
    for (a, b) in [(&actual.grid, &expected.grid), (&actual.inactive_grid, &expected.inactive_grid)]
    {
        assert_eq!(
            resolve(actual, a),
            resolve(expected, b),
            "grid contents/history/display offset"
        );
        assert_eq!(cursor(actual, &a.cursor), cursor(expected, &b.cursor));
        assert_eq!(cursor(actual, &a.saved_cursor), cursor(expected, &b.saved_cursor));
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
            term.grid[Line(0)]
                .update_extra(Column(column), |extra| extra.vivid_resize_tracking.push(7));
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
        // The escape-heavy benchmark repeatedly changes SGR, positions the cursor,
        // prints one byte, then expands it with REP.
        ("\x1b[38:5:24;48:2:125:136:147m\x1b[39m\x1b[10`a", "\x1b[100b", ""),
        ("\x1b[58;5;44;2m\x1b[2Ka", "\x1b[100b", ""),
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

#[test]
fn printable_runs_match_scalar_at_every_character_boundary() {
    let setups: &[&[u8]] = &[
        b"",
        b"\x1b[?1049h",
        b"\x1b[?7l",
        b"\x1b[4h",
        b"\x1b(0",
        b"\x1b[1;31;44m",
        b"\x1b[58;5;44m",
        b"\x1b]8;id=batch;https://example.invalid\x1b\\",
        "界e\u{301}界abc\x1b[H".as_bytes(),
        b"\x1b[2;3r\x1b[3;5H",
        // Rows that scroll back in at the bottom still hold wide characters, combining marks and
        // another background when a wrapping run recycles them, on both screens.
        "\x1b[?1049h界界界界\r\n界e\u{301}界\r\na\u{301}界\r\n界\x1b[44m\x1b[K\x1b[99;1H"
            .as_bytes(),
        "界界界界\r\n界e\u{301}界\r\na\u{301}界\r\n\x1b[41m\x1b[2J\x1b[m界\x1b[99;1H".as_bytes(),
    ];
    for (columns, lines) in [(2, 1), (5, 4), (7, 3), (80, 24)] {
        for setup in setups {
            for text in [
                "hello world abcdefghijklmnopqrstuvwxyz",
                "界你好界abc界",
                "a\u{301}\u{302}\u{303}界\u{301}e",
                "😀\u{200d}😀x\u{fe0f}🇺🇸",
                "abc\u{7f}xyz",
                // Kitty's "Unicode chars" mix: ideographs, full-width punctuation, quotes,
                // emoji, Latin-1 and combining marks.
                "旦海司：扒裝，位「才波」‘’😀—§ÀÁ ū̀n̂o᷵H̨a̠界",
            ] {
                for split in text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len()))
                {
                    let mut actual = terminal(columns, lines, setup);
                    let mut expected = terminal(columns, lines, setup);
                    for part in [&text[..split], &text[split..]] {
                        actual.input_str(part);
                        for c in part.chars() {
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
}

#[test]
#[ignore = "release-mode timing; run alone with --nocapture"]
fn benchmark_printable_runs() {
    use std::hint::black_box;
    use std::time::Instant;
    for (name, unit) in [
        ("ascii", "abcdefghijklmnopqrstuvwxyz "),
        ("unicode", "你好世界αΩ😀abc"),
        ("combining", "a\u{301}\u{302}\u{303}"),
    ] {
        let text = unit.repeat(16_384);
        for sample in 0..5 {
            for batch in if sample % 2 == 0 { [false, true] } else { [true, false] } {
                let mut term = terminal(120, 40, b"\x1b[?1049h");
                let start = Instant::now();
                for _ in 0..20 {
                    if batch {
                        term.input_str(black_box(&text));
                    } else {
                        for c in black_box(&text).chars() {
                            term.input(c);
                        }
                    }
                    term.flush_grid_scroll();
                }
                let elapsed = start.elapsed().as_secs_f64();
                black_box(&term);
                println!(
                    "{name},{sample},{batch},{:.3}",
                    text.len() as f64 * 20. / elapsed / 1048576.
                );
            }
        }
    }
}
