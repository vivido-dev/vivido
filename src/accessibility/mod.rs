//! Read-only native accessibility support for terminal windows.

use std::cmp;
use std::ops::Range;

use crate::display::SizeInfo;
use crate::terminal::grid::Dimensions;
use crate::terminal::index::{Column, Line, Point};
use crate::terminal::selection::SelectionRange;
use crate::terminal::term::Term;
use crate::terminal::term::cell::{Flags, LineLength};

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod accesskit_backend;
#[cfg(all(test, target_os = "macos"))]
#[allow(dead_code)]
#[path = "accesskit_backend.rs"]
mod accesskit_backend_compile;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) use accesskit_backend::AccessibilityState;
#[cfg(target_os = "macos")]
pub(crate) use macos::AccessibilityState;

/// Whether the native adapter should expose retained terminal scrollback.
///
/// AccessKit represents every retained row and its character geometry as nodes. Rebuilding that
/// tree on Windows and Linux makes wheel scrolling proportional to the configured history size,
/// so those platforms expose only the lightweight window and tab controls.
pub(crate) const fn terminal_document_enabled() -> bool {
    cfg!(target_os = "macos")
}

/// A text range expressed in both platform offset systems.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AccessibleRange {
    pub scalar: Range<usize>,
    pub utf16: Range<usize>,
}

/// One selectable character in a physical terminal row.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AccessibleCharacter {
    pub bytes: Range<usize>,
    pub scalar: usize,
    pub utf16: Range<usize>,
    pub column: usize,
    pub x: f32,
    pub width: f32,
}

/// Accessibility data for one physical grid row.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AccessibleLine {
    pub grid_line: i32,
    pub bytes: Range<usize>,
    pub range: AccessibleRange,
    pub characters: Vec<AccessibleCharacter>,
    pub column_offsets: Vec<usize>,
    pub y: f32,
    pub hard_break: bool,
}

/// Immutable terminal state consumed by the native accessibility adapters.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AccessibilitySnapshot {
    pub title: String,
    pub text: String,
    pub lines: Vec<AccessibleLine>,
    pub cursor: AccessibleRange,
    pub selection: Option<AccessibleRange>,
    pub block_selection: Vec<AccessibleRange>,
    pub visible: AccessibleRange,
    pub focused: bool,
    pub width: f32,
    pub height: f32,
    pub cell_width: f32,
    pub cell_height: f32,
    pub padding_x: f32,
    pub padding_y: f32,
}

impl AccessibilitySnapshot {
    pub(crate) fn new<T>(term: &Term<T>, size: SizeInfo, title: &str) -> Self {
        let grid = term.grid();
        let top = grid.topmost_line();
        let bottom = grid.bottommost_line();
        let display_offset = i32::try_from(grid.display_offset()).unwrap_or(i32::MAX);
        let visible_top = Line(-display_offset);
        let visible_bottom = cmp::min(
            visible_top + i32::try_from(grid.screen_lines()).unwrap_or(i32::MAX) - 1,
            bottom,
        );
        let mut cursor = grid.cursor.point;
        if grid[cursor].flags.contains(Flags::WIDE_CHAR_SPACER) && cursor.column.0 > 0 {
            cursor.column -= 1;
        }
        let selection = term.selection.as_ref().and_then(|selection| selection.to_range(term));

        let mut text = String::new();
        let mut lines = Vec::with_capacity(grid.total_lines());
        let mut scalar_offset = 0usize;
        let mut utf16_offset = 0usize;

        for raw_line in top.0..=bottom.0 {
            let line = Line(raw_line);
            let row = &grid[line];
            let mut end_column = row.line_length().0;
            if cursor.line == line {
                end_column = cmp::max(end_column, cursor.column.0.saturating_add(1));
            }
            if let Some(selection) = selection {
                end_column =
                    cmp::max(end_column, selection_row_end(selection, line, grid.columns()));
            }
            end_column = cmp::min(end_column, grid.columns());

            let line_byte_start = text.len();
            let line_scalar_start = scalar_offset;
            let line_utf16_start = utf16_offset;
            let mut characters = Vec::new();
            let mut column_offsets = vec![scalar_offset; grid.columns().saturating_add(1)];
            let mut tab_mode = false;

            for column_index in 0..end_column {
                let column = Column(column_index);
                let cell = &row[column];
                column_offsets[column_index] = scalar_offset;

                if tab_mode {
                    if term.is_tab_stop(column) || cell.c != ' ' {
                        tab_mode = false;
                    } else {
                        column_offsets[column_index + 1] = scalar_offset;
                        continue;
                    }
                }

                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    column_offsets[column_index + 1] = scalar_offset;
                    continue;
                }

                if cell.c == '\t' {
                    tab_mode = true;
                }

                let cell_width = if cell.flags.contains(Flags::WIDE_CHAR) {
                    size.cell_width() * 2.0
                } else if cell.c == '\t' {
                    let next_stop = ((column_index / 8) + 1) * 8;
                    size.cell_width() * cmp::max(1, next_stop.saturating_sub(column_index)) as f32
                } else {
                    size.cell_width()
                };
                let hidden = cell.flags.contains(Flags::HIDDEN);
                push_character(
                    &mut text,
                    &mut characters,
                    if hidden { ' ' } else { cell.c },
                    column_index,
                    size.padding_x() + column_index as f32 * size.cell_width(),
                    cell_width,
                    &mut scalar_offset,
                    &mut utf16_offset,
                );
                for mark in cell.zerowidth().into_iter().flatten() {
                    push_character(
                        &mut text,
                        &mut characters,
                        if hidden { ' ' } else { *mark },
                        column_index,
                        size.padding_x() + column_index as f32 * size.cell_width(),
                        0.0,
                        &mut scalar_offset,
                        &mut utf16_offset,
                    );
                }
                column_offsets[column_index + 1] = scalar_offset;
            }
            for offset in column_offsets.iter_mut().skip(end_column.saturating_add(1)) {
                *offset = scalar_offset;
            }

            let hard_break = !row.last().is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
            if hard_break {
                push_character(
                    &mut text,
                    &mut characters,
                    '\n',
                    end_column,
                    size.padding_x() + end_column as f32 * size.cell_width(),
                    0.0,
                    &mut scalar_offset,
                    &mut utf16_offset,
                );
            }

            lines.push(AccessibleLine {
                grid_line: raw_line,
                bytes: line_byte_start..text.len(),
                range: AccessibleRange {
                    scalar: line_scalar_start..scalar_offset,
                    utf16: line_utf16_start..utf16_offset,
                },
                characters,
                column_offsets,
                y: size.padding_y() + (raw_line + display_offset) as f32 * size.cell_height(),
                hard_break,
            });
        }

        let cursor_scalar = point_offset(&lines, cursor, false).unwrap_or(scalar_offset);
        let cursor_utf16 = scalar_to_utf16(&lines, cursor_scalar).unwrap_or(utf16_offset);
        let cursor = AccessibleRange {
            scalar: cursor_scalar..cursor_scalar,
            utf16: cursor_utf16..cursor_utf16,
        };
        let (selection, block_selection) = selection_ranges(&lines, selection);
        let visible = visible_range(&lines, visible_top, visible_bottom);

        Self {
            title: title.to_owned(),
            text,
            lines,
            cursor,
            selection,
            block_selection,
            visible,
            focused: term.is_focused,
            width: size.width(),
            height: size.height(),
            cell_width: size.cell_width(),
            cell_height: size.cell_height(),
            padding_x: size.padding_x(),
            padding_y: size.padding_y(),
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn utf16_text(&self, range: Range<usize>) -> String {
        let start = utf16_to_byte(&self.text, range.start);
        let end = utf16_to_byte(&self.text, range.end);
        self.text.get(start..end).unwrap_or_default().to_owned()
    }
}

fn selection_row_end(selection: SelectionRange, line: Line, columns: usize) -> usize {
    if line < selection.start.line || line > selection.end.line {
        return 0;
    }
    if selection.is_block || line == selection.end.line {
        selection.end.column.0.saturating_add(1)
    } else {
        columns
    }
}

#[allow(clippy::too_many_arguments)]
fn push_character(
    text: &mut String,
    characters: &mut Vec<AccessibleCharacter>,
    character: char,
    column: usize,
    x: f32,
    width: f32,
    scalar_offset: &mut usize,
    utf16_offset: &mut usize,
) {
    let byte_start = text.len();
    text.push(character);
    let byte_end = text.len();
    let utf16_len = character.len_utf16();
    characters.push(AccessibleCharacter {
        bytes: byte_start..byte_end,
        scalar: *scalar_offset,
        utf16: *utf16_offset..utf16_offset.saturating_add(utf16_len),
        column,
        x,
        width,
    });
    *scalar_offset = scalar_offset.saturating_add(1);
    *utf16_offset = utf16_offset.saturating_add(utf16_len);
}

fn point_offset(lines: &[AccessibleLine], point: Point, after: bool) -> Option<usize> {
    let line = lines.iter().find(|line| line.grid_line == point.line.0)?;
    let column = point.column.0.saturating_add(usize::from(after));
    line.column_offsets.get(column).copied().or(Some(line.range.scalar.end))
}

fn scalar_to_utf16(lines: &[AccessibleLine], scalar: usize) -> Option<usize> {
    let line = lines.iter().find(|line| line.range.scalar.contains(&scalar))?;
    line.characters
        .iter()
        .find(|character| character.scalar == scalar)
        .map(|character| character.utf16.start)
        .or(Some(line.range.utf16.end))
}

fn selection_ranges(
    lines: &[AccessibleLine],
    selection: Option<SelectionRange>,
) -> (Option<AccessibleRange>, Vec<AccessibleRange>) {
    let Some(selection) = selection else { return (None, Vec::new()) };
    if selection.is_block {
        let mut ranges = Vec::new();
        for raw_line in selection.start.line.0..=selection.end.line.0 {
            let start = Point::new(Line(raw_line), selection.start.column);
            let end = Point::new(Line(raw_line), selection.end.column);
            if let Some(range) = grid_range(lines, start, end) {
                ranges.push(range);
            }
        }
        return (None, ranges);
    }
    (grid_range(lines, selection.start, selection.end), Vec::new())
}

fn grid_range(lines: &[AccessibleLine], start: Point, end: Point) -> Option<AccessibleRange> {
    let scalar_start = point_offset(lines, start, false)?;
    let scalar_end = point_offset(lines, end, true)?;
    let utf16_start = scalar_to_utf16(lines, scalar_start).or_else(|| {
        lines
            .iter()
            .find(|line| line.range.scalar.end == scalar_start)
            .map(|line| line.range.utf16.end)
    })?;
    let utf16_end = scalar_to_utf16(lines, scalar_end).or_else(|| {
        lines
            .iter()
            .find(|line| line.range.scalar.end == scalar_end)
            .map(|line| line.range.utf16.end)
    })?;
    Some(AccessibleRange { scalar: scalar_start..scalar_end, utf16: utf16_start..utf16_end })
}

fn visible_range(lines: &[AccessibleLine], top: Line, bottom: Line) -> AccessibleRange {
    let first = lines.iter().find(|line| line.grid_line == top.0);
    let last = lines.iter().find(|line| line.grid_line == bottom.0);
    match (first, last) {
        (Some(first), Some(last)) => AccessibleRange {
            scalar: first.range.scalar.start..last.range.scalar.end,
            utf16: first.range.utf16.start..last.range.utf16.end,
        },
        _ => AccessibleRange::default(),
    }
}

#[cfg(target_os = "macos")]
fn utf16_to_byte(text: &str, target: usize) -> usize {
    let mut utf16 = 0usize;
    for (byte, character) in text.char_indices() {
        if utf16 >= target {
            return byte;
        }
        utf16 = utf16.saturating_add(character.len_utf16());
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::terminal::event::VoidListener;
    use crate::terminal::grid::Scroll;
    use crate::terminal::index::Side;
    use crate::terminal::selection::{Selection, SelectionType};
    use crate::terminal::term::test::TermSize;

    fn term(columns: usize, lines: usize) -> Term<VoidListener> {
        Term::new(Default::default(), &TermSize::new(columns, lines), VoidListener)
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn retained_scrollback_is_not_on_the_scroll_hot_path() {
        assert!(!terminal_document_enabled());
    }

    fn size(columns: usize, lines: usize) -> SizeInfo {
        SizeInfo::new(columns as f32 * 10.0, lines as f32 * 20.0, 10.0, 20.0, 0.0, 0.0, false)
    }

    #[test]
    fn joins_soft_wraps_and_redacts_hidden_text() {
        let mut term = term(4, 2);
        term.grid_mut()[Point::new(Line(0), Column(0))].c = 'a';
        term.grid_mut()[Point::new(Line(0), Column(1))].c = 'x';
        term.grid_mut()[Point::new(Line(0), Column(1))].flags.insert(Flags::HIDDEN);
        term.grid_mut()[Point::new(Line(0), Column(3))].flags.insert(Flags::WRAPLINE);
        term.grid_mut()[Point::new(Line(1), Column(0))].c = 'b';

        let snapshot = AccessibilitySnapshot::new(&term, size(4, 2), "test");
        assert_eq!(snapshot.text, "a   b\n");
        assert!(!snapshot.text.contains('x'));
        assert!(!snapshot.lines[0].hard_break);
        assert!(snapshot.lines[1].hard_break);
    }

    #[test]
    fn keeps_scalar_and_utf16_offsets_distinct() {
        let mut term = term(4, 1);
        let cell = &mut term.grid_mut()[Point::new(Line(0), Column(0))];
        cell.c = '🦇';
        cell.flags.insert(Flags::WIDE_CHAR);
        cell.push_zerowidth('\u{301}');
        term.grid_mut()[Point::new(Line(0), Column(1))].flags.insert(Flags::WIDE_CHAR_SPACER);
        term.grid_mut().cursor.point = Point::new(Line(0), Column(1));

        let snapshot = AccessibilitySnapshot::new(&term, size(4, 1), "test");
        assert_eq!(snapshot.text, "🦇\u{301}\n");
        assert_eq!(snapshot.lines[0].range.scalar, 0..3);
        assert_eq!(snapshot.lines[0].range.utf16, 0..4);
        assert_eq!(snapshot.cursor.scalar, 0..0);
        assert_eq!(snapshot.cursor.utf16, 0..0);
        assert_eq!(snapshot.lines[0].characters[0].width, 20.0);
        assert_eq!(snapshot.lines[0].characters[1].width, 0.0);
    }

    #[test]
    fn visible_range_tracks_scrollback_viewport() {
        let mut term = term(3, 2);
        term.grid_mut()[Point::new(Line(0), Column(0))].c = 'a';
        term.grid_mut()[Point::new(Line(1), Column(0))].c = 'b';
        term.grid_mut().scroll_up(&(Line(0)..Line(2)), 1);

        let latest = AccessibilitySnapshot::new(&term, size(3, 2), "test");
        assert_eq!(latest.lines.len(), 3);
        assert_eq!(latest.lines[0].grid_line, -1);
        assert_eq!(latest.visible.scalar.start, latest.lines[1].range.scalar.start);
        assert!(latest.visible.scalar.start > 0);

        term.grid_mut().scroll_display(Scroll::Top);
        let history = AccessibilitySnapshot::new(&term, size(3, 2), "test");
        assert_eq!(history.visible.scalar.start, 0);
        assert_eq!(history.visible.scalar.end, history.lines[1].range.scalar.end);
    }

    #[test]
    fn block_selection_stays_discontiguous() {
        let mut term = term(3, 2);
        for (line, text) in ["abc", "def"].into_iter().enumerate() {
            for (column, character) in text.chars().enumerate() {
                term.grid_mut()[Point::new(Line(line as i32), Column(column))].c = character;
            }
        }
        let mut selection =
            Selection::new(SelectionType::Block, Point::new(Line(0), Column(0)), Side::Left);
        selection.update(Point::new(Line(1), Column(1)), Side::Right);
        term.selection = Some(selection);

        let snapshot = AccessibilitySnapshot::new(&term, size(3, 2), "test");
        assert!(snapshot.selection.is_none());
        assert_eq!(snapshot.block_selection.len(), 2);
        assert_eq!(snapshot.block_selection[0].scalar, 0..2);
        assert_eq!(snapshot.block_selection[1].scalar, 4..6);
    }

    #[test]
    fn cursor_extends_an_unoccupied_blank_line() {
        let mut term = term(6, 1);
        term.grid_mut().cursor.point = Point::new(Line(0), Column(4));
        let snapshot = AccessibilitySnapshot::new(&term, size(6, 1), "test");
        assert_eq!(snapshot.text, "     \n");
        assert_eq!(snapshot.cursor.scalar, 4..4);
    }

    #[test]
    fn tabs_keep_one_character_with_expanded_geometry() {
        let mut term = term(10, 1);
        term.grid_mut()[Point::new(Line(0), Column(0))].c = '\t';
        term.grid_mut()[Point::new(Line(0), Column(8))].c = 'x';
        let snapshot = AccessibilitySnapshot::new(&term, size(10, 1), "test");
        assert_eq!(snapshot.text, "\tx\n");
        assert_eq!(snapshot.lines[0].characters[0].width, 80.0);
        assert_eq!(snapshot.lines[0].characters[1].column, 8);
    }

    #[test]
    fn alternate_screen_replaces_the_accessible_document() {
        let mut term = term(3, 1);
        term.grid_mut()[Point::new(Line(0), Column(0))].c = 'p';
        term.swap_alt();
        term.grid_mut()[Point::new(Line(0), Column(0))].c = 'a';
        let alternate = AccessibilitySnapshot::new(&term, size(3, 1), "test");
        assert_eq!(alternate.text, "a\n");

        term.swap_alt();
        let primary = AccessibilitySnapshot::new(&term, size(3, 1), "test");
        assert_eq!(primary.text, "p\n");
    }

    #[test]
    fn linear_selection_is_one_exact_range() {
        let mut term = term(4, 1);
        for (column, character) in "abcd".chars().enumerate() {
            term.grid_mut()[Point::new(Line(0), Column(column))].c = character;
        }
        let mut selection =
            Selection::new(SelectionType::Simple, Point::new(Line(0), Column(1)), Side::Left);
        selection.update(Point::new(Line(0), Column(2)), Side::Right);
        term.selection = Some(selection);
        let snapshot = AccessibilitySnapshot::new(&term, size(4, 1), "test");
        assert_eq!(snapshot.selection.unwrap().scalar, 1..3);
        assert!(snapshot.block_selection.is_empty());
    }
}
