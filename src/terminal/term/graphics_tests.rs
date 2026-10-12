//! Kitty graphics driven through the terminal the way an application drives it: as bytes.
//!
//! The graphics module tests its own rules in isolation. These tests cover what only the terminal
//! can get right: where the cursor ends up, and that images follow the grid through scrolling,
//! erasing, resizing, the alternate screen and reset.

// Rust guideline compliant 2026-10-07

use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::terminal::event::{Event, EventListener, WindowSize};
use crate::terminal::grid::Dimensions;
use crate::terminal::index::{Column, Line, Point};
use crate::terminal::term::test::TermSize;
use crate::terminal::term::{Config, Term, TermMode};
use crate::terminal::vvte::ansi;

/// Records what the terminal writes back to the application.
#[derive(Clone, Default)]
struct Replies(Arc<Mutex<Vec<String>>>);

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        let window = WindowSize { num_lines: 24, num_cols: 80, cell_width: 9, cell_height: 19 };
        match event {
            Event::PtyWrite(text) => self.0.lock().unwrap().push(text),
            Event::TextAreaSizeRequest(format) => self.0.lock().unwrap().push(format(window)),
            _ => {},
        }
    }
}

/// A terminal of 10x20-pixel cells, which is what a terminal without a display assumes.
struct Screen {
    term: Term<Replies>,
    parser: ansi::Processor,
    replies: Replies,
}

impl Screen {
    fn new(columns: usize, lines: usize) -> Self {
        let replies = Replies::default();
        let term = Term::new(Config::default(), &TermSize::new(columns, lines), replies.clone());
        Self { term, parser: ansi::Processor::new(), replies }
    }

    fn feed(&mut self, bytes: impl AsRef<[u8]>) -> &mut Self {
        self.term.advance(&mut self.parser, bytes.as_ref());
        self
    }

    fn cursor(&self) -> (i32, usize) {
        let point = self.term.grid().cursor.point;
        (point.line.0, point.column.0)
    }

    /// Line, column, columns and rows of every placement on the displayed screen.
    fn placed(&self) -> Vec<(i32, i32, u32, u32)> {
        let store = self.term.graphics().store();
        store.placements().iter().map(|p| (p.row, p.col, p.eff_cols, p.eff_rows)).collect()
    }

    fn replies(&self) -> Vec<String> {
        std::mem::take(&mut *self.replies.0.lock().unwrap())
    }

    /// Text of one grid line, without trailing blanks.
    fn text(&self, line: i32) -> String {
        let row = &self.term.grid()[Line(line)];
        let text: String = (0..self.term.columns()).map(|column| row[Column(column)].c).collect();
        text.trim_end().to_owned()
    }
}

/// The exact bytes kitim writes for one frame when run directly in a terminal.
///
/// Raw RGBA, base64 in 4096-character chunks, no image id, `q=2`, and `C=1` when it wants the
/// cursor left alone.
fn kitim_frame(width: u32, height: u32, keep_cursor: bool) -> Vec<u8> {
    let data = STANDARD.encode([40, 80, 120, 255].repeat((width * height) as usize));
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
    let mut out = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        if index == 0 {
            let policy = if keep_cursor { ",C=1" } else { "" };
            out.extend(
                format!("\x1b_Ga=T,f=32,s={width},v={height}{policy},q=2,m={more};").bytes(),
            );
        } else {
            out.extend(format!("\x1b_Gq=2,m={more};").bytes());
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// A small image with an id, for tests that need to place or name it afterwards.
fn image(id: u32, width: u32, height: u32, keys: &str) -> String {
    let data = STANDARD.encode([1, 2, 3, 128].repeat((width * height) as usize));
    format!("\x1b_Ga=T,i={id},s={width},v={height},q=2{keys};{data}\x1b\\")
}

#[test]
fn kitim_showing_an_image_leaves_the_prompt_below_it() {
    let mut screen = Screen::new(80, 24);
    // kitim writes the image and then a newline.
    screen.feed(kitim_frame(640, 360, false));
    assert_eq!(screen.placed(), [(0, 0, 64, 18)]);
    assert_eq!(screen.cursor(), (17, 64), "on the image's last row, just right of it");
    screen.feed("\r\n$ ");
    assert_eq!(screen.cursor(), (18, 2));
    assert!(screen.replies().is_empty(), "q=2 asks for silence");
}

#[test]
fn the_stream_may_be_cut_anywhere_between_reads() {
    let frame = kitim_frame(64, 40, false);
    for size in [1, 2, 3, 5, 4096, 4097, 5000] {
        let mut screen = Screen::new(80, 24);
        for piece in frame.chunks(size) {
            screen.feed(piece);
        }
        assert_eq!(screen.placed(), [(0, 0, 7, 2)], "reads of {size} bytes");
        assert_eq!(screen.cursor(), (1, 7), "reads of {size} bytes");
    }
}

#[test]
fn an_image_near_the_bottom_scrolls_the_screen_and_itself() {
    let mut screen = Screen::new(80, 24);
    screen.feed("\x1b[21;1H").feed(kitim_frame(100, 200, false));
    // Ten rows from line 20 would end on line 29; the screen scrolls six lines to make room.
    assert_eq!(screen.cursor(), (23, 10));
    assert_eq!(screen.placed(), [(14, 0, 10, 10)]);
    assert_eq!(screen.term.grid().history_size(), 6);
}

#[test]
fn an_image_reaching_the_right_margin_wraps_the_cursor_to_the_next_line() {
    let mut screen = Screen::new(80, 24);
    screen.feed("\x1b[1;71H").feed(kitim_frame(100, 40, false));
    assert_eq!(screen.placed(), [(0, 70, 10, 2)]);
    assert_eq!(screen.cursor(), (2, 0));
}

#[test]
fn an_image_taller_than_the_screen_scrolls_by_one_screen_at_most() {
    let mut screen = Screen::new(80, 5);
    screen.feed(image(1, 10, 4000, ""));
    // 200 rows of image; after five line feeds everything has scrolled as far as it matters.
    assert_eq!(screen.cursor(), (4, 1));
    assert_eq!(screen.placed(), [(-1, 0, 1, 200)]);
}

#[test]
fn frames_drawn_in_place_leave_the_cursor_and_one_image() {
    let mut screen = Screen::new(80, 24);
    screen.feed("\x1b[5;9H");
    for _ in 0..30 {
        screen.feed(kitim_frame(320, 180, true));
    }
    assert_eq!(screen.cursor(), (4, 8));
    assert_eq!(screen.placed(), [(4, 8, 32, 9)]);
    assert!(screen.replies().is_empty());
}

/// What kitim's video player does from start to exit.
#[test]
fn kitim_video_plays_on_the_alternate_screen_and_leaves_nothing_behind() {
    let mut screen = Screen::new(80, 24);
    screen.feed(image(9, 20, 40, "")).feed("\r\nshell prompt");
    let primary = screen.placed();

    screen.feed("\x1b[?1049h\x1b[?25l\x1b[2J\x1b[1;1H");
    assert!(screen.placed().is_empty(), "the alternate screen starts with no images");
    for frame in 0..10 {
        screen.feed("\x1b[3;9H").feed(kitim_frame(640, 360, true));
        screen.feed(format!("\x1b[24;1H\x1b[2K00:{frame:02} / 00:10"));
    }
    assert_eq!(screen.placed(), [(2, 8, 64, 18)]);

    // A resize makes kitim clear and draw at a new origin.
    screen.feed("\x1b[2J\x1b[1;1H\x1b[2;5H").feed(kitim_frame(320, 180, true));
    assert_eq!(screen.placed(), [(1, 4, 32, 9)]);

    screen.feed("\x1b[?25h\x1b[2J\x1b[?1049l\x1b[1;1H");
    assert!(!screen.term.mode().contains(TermMode::ALT_SCREEN));
    assert_eq!(screen.placed(), primary, "the image shown before the video is still there");
    assert!(screen.replies().is_empty());
}

/// Two owners with the same id: the screens. Whatever one does must not reach the other.
#[test]
fn the_same_image_id_on_both_screens_stays_separate() {
    let mut screen = Screen::new(80, 24);
    screen.feed(image(1, 20, 40, ""));
    screen.feed("\x1b[?1049h");
    screen.feed("\x1b_Ga=p,i=1\x1b\\");
    assert!(screen.placed().is_empty(), "id 1 does not exist on the alternate screen");
    assert_eq!(
        screen.replies(),
        ["\x1b_Gi=1;ENOENT:Put command refers to non-existent image\x1b\\"]
    );

    screen.feed(image(1, 50, 20, "")).feed("\x1b_Ga=d,d=I,i=1\x1b\\\x1b[2J");
    screen.feed("\x1b[?1049l");
    assert_eq!(screen.placed(), [(0, 0, 2, 2)], "the primary screen's image 1 is untouched");
    let store = screen.term.graphics().store();
    assert_eq!(store.image(store.key_by_id(1).unwrap()).unwrap().width, 20);
}

/// Two owners with the same ids: two terminals. They share nothing.
#[test]
fn two_terminals_using_the_same_ids_do_not_affect_each_other() {
    let mut first = Screen::new(80, 24);
    let mut second = Screen::new(80, 24);
    first.feed(image(1, 20, 40, ""));
    second.feed(image(1, 60, 20, ""));

    second.feed("\x1b_Ga=d,d=I,i=1\x1b\\");
    assert!(second.placed().is_empty());
    assert_eq!(first.placed(), [(0, 0, 2, 2)]);

    second.feed("\x1bc");
    first.feed("\x1b_Ga=p,i=1,C=1\x1b\\");
    assert_eq!(first.replies(), ["\x1b_Gi=1;OK\x1b\\"]);
    assert!(second.replies().is_empty());

    let uid = |screen: &Screen| {
        let store = screen.term.graphics().store();
        store.image(store.key_by_id(1).unwrap()).unwrap().uid
    };
    second.feed(image(1, 20, 40, ""));
    assert_ne!(uid(&first), uid(&second), "even identical images have distinct identities");
}

#[test]
fn text_scrolling_carries_images_into_scrollback_until_it_runs_out() {
    let config = Config { scrolling_history: 3, ..Config::default() };
    let replies = Replies::default();
    let term = Term::new(config, &TermSize::new(80, 5), replies.clone());
    let mut screen = Screen { term, parser: ansi::Processor::new(), replies };

    screen.feed(image(1, 10, 40, "")).feed("\x1b[5;1H");
    for expected in [-1, -2, -3] {
        screen.feed("\n");
        assert_eq!(screen.placed(), [(expected, 0, 1, 2)]);
    }
    // History holds three lines, so the image's second row is still at line -3.
    screen.feed("\n");
    assert_eq!(screen.placed(), [(-4, 0, 1, 2)]);
    screen.feed("\n");
    assert!(screen.placed().is_empty(), "both rows have left the top of scrollback");
    assert!(screen.term.graphics().store().key_by_id(1).is_some(), "the image can be placed again");
}

#[test]
fn a_scroll_region_moves_and_clips_only_the_images_inside_it() {
    let mut screen = Screen::new(80, 12);
    screen.feed(image(1, 10, 60, ",C=1"));
    screen.feed("\x1b[5;1H\x1b_Ga=p,i=1,C=1,p=2,q=2\x1b\\");
    // Region is lines 4..=9; the first image is above it, the second inside.
    screen.feed("\x1b[5;10r\x1b[10;1H\n");
    assert_eq!(screen.placed(), [(0, 0, 1, 3), (4, 0, 1, 2)], "one row was clipped at the margin");
    screen.feed("\n\n");
    assert_eq!(screen.placed(), [(0, 0, 1, 3)], "the second scrolled out of the region");

    // A reverse index at the top of the region pushes an image down and clips it at the bottom.
    screen.feed("\x1b[8;1H\x1b_Ga=p,i=1,C=1,p=3,q=2\x1b\\\x1b[5;1H\x1bM");
    assert_eq!(screen.placed(), [(0, 0, 1, 3), (8, 0, 1, 2)]);
}

#[test]
fn inserting_and_deleting_lines_moves_the_images_below_the_cursor() {
    let mut screen = Screen::new(80, 12);
    screen.feed("\x1b[6;1H").feed(image(1, 10, 20, ",C=1"));
    screen.feed("\x1b[3;1H\x1b[2L");
    assert_eq!(screen.placed(), [(7, 0, 1, 1)]);
    screen.feed("\x1b[3M");
    assert_eq!(screen.placed(), [(4, 0, 1, 1)]);
}

#[test]
fn erasing_the_display_removes_what_is_on_screen_and_erasing_scrollback_the_rest() {
    let mut screen = Screen::new(80, 5);
    screen.feed(image(1, 10, 20, ",C=1")).feed("\x1b[5;1H\n\n");
    screen.feed("\x1b[3;1H").feed(image(2, 10, 20, ",C=1"));
    assert_eq!(screen.placed(), [(-2, 0, 1, 1), (2, 0, 1, 1)]);

    // Erasing below or above the cursor is a text operation and leaves images alone.
    screen.feed("\x1b[0J\x1b[1J");
    assert_eq!(screen.placed().len(), 2);

    screen.feed("\x1b[2J");
    assert_eq!(screen.placed(), [(-2, 0, 1, 1)]);
    let store = screen.term.graphics().store();
    assert!(store.key_by_id(2).is_none(), "an erased image with no placement left is freed");

    screen.feed("\x1b[3J");
    assert!(screen.term.graphics().store().is_empty());
}

/// Erasing the primary screen keeps its text in scrollback, so an image beside that text is kept
/// with it. One that would still be on screen afterwards is erased, as the application asked.
#[test]
fn erasing_the_primary_screen_sends_images_into_scrollback_with_their_text() {
    let mut screen = Screen::new(80, 6);
    screen.feed("old output\r\n").feed("\n\n\n\n\n\n");
    screen.feed("\x1b[1;1H").feed(image(1, 10, 20, ",C=1,p=1"));
    assert_eq!(screen.placed(), [(0, 0, 1, 1)]);
    screen.feed("\x1b[2J\x1b[1;1H").feed("$ kitim cat.png\r\n").feed(image(2, 10, 20, ""));
    screen.feed("\r\n$ ").feed("\x1b[6;1H").feed(image(3, 10, 20, ",C=1"));
    // Text on lines 0..=2, image 2 on line 1 among it, image 3 down on line 5 below it all.
    assert_eq!(screen.placed(), [(1, 0, 1, 1), (5, 0, 1, 1)]);
    let history = screen.term.grid().history_size();

    screen.feed("\x1b[2J");
    assert_eq!(screen.term.grid().history_size(), history + 3, "three lines of text were kept");
    assert_eq!(screen.placed(), [(-2, 0, 1, 1)], "image 2 is still beside `kitim cat.png`");
    let store = screen.term.graphics().store();
    assert!(store.key_by_id(3).is_none(), "image 3 had nowhere to go and was erased");
}

#[test]
fn a_full_reset_forgets_every_image_on_both_screens() {
    let mut screen = Screen::new(80, 24);
    screen.feed(image(1, 10, 20, "")).feed("\x1b[?1049h").feed(image(2, 10, 20, ""));
    screen.feed("\x1bc");
    assert!(!screen.term.mode().contains(TermMode::ALT_SCREEN));
    assert!(screen.term.graphics().store().is_empty());
    screen.feed("\x1b[?1049h");
    assert!(screen.term.graphics().store().is_empty());
}

#[test]
fn host_recovery_drops_images_and_an_interrupted_transmission() {
    let mut screen = Screen::new(80, 24);
    screen.feed(image(1, 10, 20, ""));
    let frame = kitim_frame(64, 64, false);
    screen.feed(&frame[..frame.len() / 2]);
    screen.term.reset_client_state();
    screen.parser = ansi::Processor::new();
    assert!(screen.term.graphics().store().is_empty());
    // The rest of the interrupted frame arrives as text the parser no longer recognizes; what
    // matters is that a fresh image works afterwards.
    screen.feed("\x1b[2J\x1b[1;1H").feed(image(2, 10, 20, ""));
    assert_eq!(screen.placed(), [(0, 0, 1, 1)]);
}

#[test]
fn a_reflowing_resize_keeps_an_image_beside_its_text() {
    let mut screen = Screen::new(40, 24);
    // Sixty characters wrap onto two lines at forty columns; the image is on the line after.
    screen.feed("x".repeat(60)).feed("\r\n").feed(image(1, 10, 20, "")).feed("\r\nafter");
    assert_eq!(screen.placed(), [(2, 0, 1, 1)]);

    // The image has no text of its own, so the lines around it say where it belongs.
    let beside_its_text = |screen: &Screen| {
        let row = screen.placed()[0].0;
        assert!(screen.text(row - 1).ends_with('x'), "below the wrapped line, at row {row}");
        assert_eq!(screen.text(row), "");
        assert_eq!(screen.text(row + 1), "after");
        row
    };
    beside_its_text(&screen);

    // At eighty columns the two lines join into one and everything below moves up.
    screen.term.resize_with_tracking(TermSize::new(80, 24), &mut []);
    assert_eq!(beside_its_text(&screen), 1);

    // Back at forty the line wraps again; wherever the grid puts it, the image follows.
    screen.term.resize_with_tracking(TermSize::new(40, 24), &mut []);
    beside_its_text(&screen);
    screen.term.resize_with_tracking(TermSize::new(7, 24), &mut []);
    beside_its_text(&screen);
}

#[test]
fn a_resize_that_discards_an_images_line_discards_the_image() {
    let mut screen = Screen::new(80, 10);
    screen.feed("\x1b[?1049h\x1b[9;1H").feed(image(1, 10, 20, ",C=1"));
    screen.feed("\x1b[1;1H");
    assert_eq!(screen.placed(), [(8, 0, 1, 1)]);
    // The alternate screen has no scrollback: shrinking it cuts lines off the bottom.
    screen.term.resize_with_tracking(TermSize::new(80, 4), &mut []);
    assert!(screen.placed().is_empty());
}

#[test]
fn a_resize_tracks_caller_positions_and_images_together() {
    use crate::terminal::term::ResizePoint;

    let mut screen = Screen::new(40, 24);
    screen.feed("x".repeat(60)).feed("\r\n").feed(image(1, 10, 20, ""));
    // One caller position on the image's own cell and one on the last `x`.
    let mut positions = [
        Some(ResizePoint { point: Point::new(Line(2), Column(0)), alternate: false }),
        Some(ResizePoint { point: Point::new(Line(1), Column(19)), alternate: false }),
        Some(ResizePoint { point: Point::new(Line(99), Column(0)), alternate: false }),
    ];
    screen.term.resize_with_tracking(TermSize::new(80, 24), &mut positions);
    let at = positions.map(|p| p.map(|p| (p.point.line.0, p.point.column.0)));
    assert_eq!(at, [Some((1, 0)), Some((0, 59)), None]);
    assert_eq!(
        screen.placed(),
        [(1, 0, 1, 1)],
        "the image moved exactly as the position on it did"
    );
}

#[test]
fn a_graphics_command_inside_a_synchronized_update_uses_the_cursor_of_its_own_moment() {
    let mut screen = Screen::new(80, 24);
    // The cursor move is still buffered when the image command arrives; both run in order when
    // the update ends, so the image lands at 5,5 and not at the origin.
    screen.feed("\x1b[?2026h\x1b[6;6H").feed(image(1, 10, 20, ",C=1"));
    assert!(screen.placed().is_empty(), "nothing is applied while the update is held");
    screen.feed("\x1b[?2026l");
    assert_eq!(screen.placed(), [(5, 5, 1, 1)]);
}

#[test]
fn a_query_is_answered_and_a_cell_size_report_is_available() {
    let mut screen = Screen::new(80, 24);
    // The probe kitim sends under vvmux, followed by a device-attributes request.
    screen.feed("\x1b_Ga=q,t=d,f=24,s=1,v=1,i=31;AAAA\x1b\\");
    screen.feed("\x1b_Ga=q,t=f,i=32;L3RtcC94\x1b\\");
    screen.feed("\x1b[16t");
    assert_eq!(
        screen.replies(),
        [
            "\x1b_Gi=31;OK\x1b\\",
            "\x1b_Gi=32;EINVAL:Unsupported transmission medium\x1b\\",
            "\x1b[6;19;9t",
        ]
    );
}

#[test]
fn a_command_cut_off_by_other_output_does_not_run() {
    let mut screen = Screen::new(80, 24);
    let command = image(1, 10, 20, "");
    // Everything but the terminator, then an unrelated sequence.
    screen.feed(&command[..command.len() - 2]).feed("\x1b[31mred");
    assert!(screen.placed().is_empty());
    assert_eq!(screen.cursor(), (0, 3));
    screen.feed(image(1, 10, 20, ""));
    assert_eq!(screen.placed(), [(0, 3, 1, 1)]);
}

#[test]
fn placeholder_text_is_stored_with_its_marks_for_the_renderer_to_decode() {
    use crate::terminal::graphics::PLACEHOLDER;

    let mut screen = Screen::new(80, 24);
    // Every protocol diacritic must survive the terminal as a zero-width mark on its cell, or
    // the rows and columns it encodes would be lost.
    let marks: Vec<char> = (0x0300..=0x1D244_u32)
        .filter_map(char::from_u32)
        .filter(|mark| {
            let mut cell =
                crate::terminal::term::cell::Cell { c: PLACEHOLDER, ..Default::default() };
            cell.push_zerowidth(*mark);
            crate::terminal::graphics::decode_cell(&cell).is_some_and(|code| code.row != 0)
        })
        .collect();
    assert_eq!(marks.len(), 297);
    for (index, mark) in marks.iter().enumerate() {
        screen.feed(format!("\x1b[1;1H\x1b[2K{PLACEHOLDER}{mark}"));
        let cell = &screen.term.grid()[Line(0)][Column(0)];
        assert_eq!(cell.c, PLACEHOLDER);
        assert_eq!(cell.zerowidth(), Some(&[*mark][..]), "mark {index} U+{:04X}", *mark as u32);
        assert_eq!(screen.cursor(), (0, 1), "the placeholder is one cell wide");
    }
}

#[test]
fn ordinary_output_never_touches_graphics_state() {
    let mut screen = Screen::new(80, 24);
    screen.feed("plain text\r\n\x1b[31mcolor\x1b[m \x1b]0;title\x07 \x1bP+q544e\x1b\\ \x1b^pm\x1b\\ \x1bXsos\x1b\\");
    screen.feed("\x1b_not graphics\x1b\\ \x1b_\x1b\\ more");
    assert_eq!(screen.term.graphics().revision(), 0);
    assert!(screen.term.graphics().store().is_empty());
    assert!(!screen.term.take_graphics_activity().1);
}
