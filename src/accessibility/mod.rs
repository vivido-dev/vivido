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
#[allow(
    dead_code,
    reason = "platform-dependent native declarations are retained for ABI completeness"
)]
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
///
/// Offsets are `u32` rather than `usize`: this is the per-cell axis of a document covering the
/// whole scrollback, so the width shows up directly in the retained size — 56 bytes a character
/// against 32. A terminal cannot reach 4 GiB of text; `MAX_SCROLLBACK_LINES` is 100 000 and the
/// column count is bounded by the maximum texture dimension.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AccessibleCharacter {
    pub bytes: Range<u32>,
    pub scalar: u32,
    pub utf16: Range<u32>,
    pub column: u32,
    pub x: f32,
    pub width: f32,
}

impl AccessibleCharacter {
    /// UTF-16 range widened for the platform accessibility APIs, which index in `usize`.
    #[cfg(target_os = "macos")]
    pub fn utf16_range(&self) -> Range<usize> {
        self.utf16.start as usize..self.utf16.end as usize
    }
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
    /// The application's own semantic tree, when an overlay published one for the scene it is
    /// currently showing. `None` means the terminal beneath is the whole story.
    pub semantics: Option<OverlaySemantics>,
}

/// A window's semantic tree together with the overlay window that published it.
///
/// The identity travels with the tree because a node ID is only unique within one application's
/// tree, so an action naming node 7 needs to know whose node 7 it is.
#[derive(Clone, Debug, PartialEq)]
pub struct OverlaySemantics {
    pub(crate) window: vivid_protocol::identity::SurfaceIdentity,
    pub(crate) first_node: u64,
    pub(crate) tree: std::sync::Arc<vivid_protocol::overlay::Semantics>,
    pub(crate) bounds: vivid_protocol::vector::Rect,
    pub(crate) scale: f64,
}

pub(crate) type OverlayActionCallback = std::sync::Arc<
    dyn Fn(&OverlaySemantics, u64, vivid_protocol::overlay::AccessibleAction) + Send + Sync,
>;

impl OverlaySemantics {
    /// The complete tree in window-local logical pixels.
    pub fn tree(&self) -> &vivid_protocol::overlay::Semantics {
        &self.tree
    }

    /// First host-assigned node ID. Each subsequent node occupies the next ID.
    /// IDs are unique for the lifetime of the originating terminal host.
    pub fn first_node(&self) -> u64 {
        self.first_node
    }

    /// Presented overlay placement in terminal-local logical pixels.
    pub fn bounds(&self) -> vivid_protocol::vector::Rect {
        self.bounds
    }

    /// Physical terminal pixels per overlay logical pixel.
    pub fn scale(&self) -> f64 {
        self.scale
    }
}

impl AccessibilitySnapshot {
    /// Minimal native tree used where exposing retained terminal history would be too expensive.
    pub(crate) fn window(size: SizeInfo, title: &str, focused: bool) -> Self {
        Self {
            title: title.to_owned(),
            text: String::new(),
            lines: Vec::new(),
            cursor: AccessibleRange::default(),
            selection: None,
            block_selection: Vec::new(),
            visible: AccessibleRange::default(),
            focused,
            width: size.width(),
            height: size.height(),
            cell_width: size.cell_width(),
            cell_height: size.cell_height(),
            padding_x: size.padding_x(),
            padding_y: size.padding_y(),
            semantics: None,
        }
    }

    pub(crate) fn new<T>(term: &Term<T>, size: SizeInfo, title: &str) -> Self {
        Self::capture(term, size, title, term.grid().topmost_line(), term.grid().bottommost_line())
    }

    fn capture<T>(term: &Term<T>, size: SizeInfo, title: &str, top: Line, bottom: Line) -> Self {
        let grid = term.grid();
        let display_offset = i32::try_from(grid.display_offset()).unwrap_or(i32::MAX);
        let visible_top = Line(-display_offset);
        let visible_bottom = cmp::min(
            visible_top + i32::try_from(grid.screen_lines()).unwrap_or(i32::MAX) - 1,
            bottom,
        );
        let mut cursor = grid.cursor.point;
        if grid[cursor].layout().contains(Flags::WIDE_CHAR_SPACER) && cursor.column.0 > 0 {
            cursor.column -= 1;
        }
        let selection = term.selection.as_ref().and_then(|selection| selection.to_range(term));

        let mut text = String::new();
        let mut lines = Vec::with_capacity((bottom.0 - top.0 + 1) as usize);
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
            let mut characters = Vec::with_capacity(end_column.saturating_add(1));
            // Sized to the line's own content, not the grid width. Every retained scrollback row
            // carried `columns + 1` offsets whether or not it held any text, which is 1.5 KB per
            // blank line at 185 columns. Columns past the content are answered by `point_offset`
            // instead, which reproduces what the trailing fill used to store.
            let mut column_offsets = vec![scalar_offset; end_column.saturating_add(1)];
            let mut tab_mode = false;

            for column_index in 0..end_column {
                let column = Column(column_index);
                let cell = &row[column];
                column_offsets[column_index] = scalar_offset;

                if tab_mode {
                    if term.is_tab_stop(column) || cell.c() != ' ' {
                        tab_mode = false;
                    } else {
                        column_offsets[column_index + 1] = scalar_offset;
                        continue;
                    }
                }

                if cell
                    .layout()
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    column_offsets[column_index + 1] = scalar_offset;
                    continue;
                }

                if cell.c() == '\t' {
                    tab_mode = true;
                }

                let cell_width = if cell.layout().contains(Flags::WIDE_CHAR) {
                    size.cell_width() * 2.0
                } else if cell.c() == '\t' {
                    let next_stop = ((column_index / 8) + 1) * 8;
                    size.cell_width() * cmp::max(1, next_stop.saturating_sub(column_index)) as f32
                } else {
                    size.cell_width()
                };
                let hidden = term.style(cell).attrs.contains(Flags::HIDDEN);
                push_character(
                    &mut text,
                    &mut characters,
                    if hidden { ' ' } else { cell.c() },
                    column_index,
                    size.padding_x() + column_index as f32 * size.cell_width(),
                    cell_width,
                    &mut scalar_offset,
                    &mut utf16_offset,
                );
                for mark in row.zerowidth(cell).into_iter().flatten() {
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
            let hard_break =
                !row.last().is_some_and(|cell| cell.layout().contains(Flags::WRAPLINE));
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
            semantics: None,
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn utf16_text(&self, range: Range<usize>) -> String {
        let start = utf16_to_byte(&self.text, range.start);
        let end = utf16_to_byte(&self.text, range.end);
        self.text.get(start..end).unwrap_or_default().to_owned()
    }
}

/// Captures a bounded explicit automation range using the native visible-text rules.
pub(crate) fn visible_text<T>(
    term: &Term<T>,
    size: SizeInfo,
    start: usize,
    count: usize,
) -> Result<Vec<String>, vivid_sdk::accessible_text::InvalidDocument> {
    use vivid_sdk::accessible_text::InvalidDocument;
    // Bound both scanning and transient character geometry before making a snapshot.
    const MAX_ROWS: usize = 32;
    const MAX_CELLS: usize = 4096;
    const MAX_BYTES: usize = 16 * 1024;
    let grid = term.grid();
    let end = start
        .checked_add(count)
        .filter(|end| *end <= grid.screen_lines())
        .ok_or(InvalidDocument)?;
    if count == 0
        || count > MAX_ROWS
        || grid.columns() == 0
        || grid.columns().checked_mul(count).is_none_or(|n| n > MAX_CELLS)
        || ![size.width(), size.height(), size.cell_height()]
            .iter()
            .all(|n| n.is_finite() && *n > 0.)
    {
        return Err(InvalidDocument);
    }
    let offset = i32::try_from(grid.display_offset()).map_err(|_| InvalidDocument)?;
    let first = Line(-offset) + i32::try_from(start).map_err(|_| InvalidDocument)?;
    let last = Line(-offset) + i32::try_from(end - 1).map_err(|_| InvalidDocument)?;
    if first < grid.topmost_line() || last > grid.bottommost_line() {
        return Err(InvalidDocument);
    }
    let mut bytes = count;
    for raw_line in first.0..=last.0 {
        let row = &grid[Line(raw_line)];
        for column in 0..grid.columns() {
            let cell = &row[Column(column)];
            let hidden = term.style(cell).attrs.contains(Flags::HIDDEN);
            bytes = bytes
                .checked_add(if hidden { 1 } else { cell.c().len_utf8() })
                .filter(|n| *n <= MAX_BYTES)
                .ok_or(InvalidDocument)?;
            for mark in row.zerowidth(cell).unwrap_or_default() {
                bytes = bytes
                    .checked_add(if hidden { 1 } else { mark.len_utf8() })
                    .filter(|n| *n <= MAX_BYTES)
                    .ok_or(InvalidDocument)?;
            }
        }
    }
    let snapshot = AccessibilitySnapshot::capture(term, size, "", first, last);
    Ok(snapshot.lines.iter().map(|line| snapshot.text[line.bytes.clone()].to_owned()).collect())
}

/// Captures only visible rows after validating aggregate allocation bounds.
pub(crate) fn visible_document<T>(
    term: &Term<T>,
    size: SizeInfo,
    title: &str,
    focused: bool,
) -> Result<vivid_sdk::accessible_text::Document, vivid_sdk::accessible_text::InvalidDocument> {
    use vivid_sdk::accessible_text::{
        Builder, InvalidDocument, MAX_CHARACTERS, MAX_LABEL_BYTES, MAX_LINES, MAX_TEXT_BYTES,
        Selection,
    };
    let grid = term.grid();
    let rows = grid.screen_lines();
    let columns = grid.columns();
    if rows == 0
        || rows > MAX_LINES
        || title.len() > MAX_LABEL_BYTES
        || rows
            .checked_mul(columns)
            .and_then(|n| n.checked_add(rows))
            .is_none_or(|n| n > MAX_CHARACTERS)
        || ![size.width(), size.height(), size.cell_height()]
            .iter()
            .all(|n| n.is_finite() && *n > 0.)
    {
        return Err(InvalidDocument);
    }
    let offset = i32::try_from(grid.display_offset()).map_err(|_| InvalidDocument)?;
    let first = cmp::max(grid.topmost_line(), Line(-offset));
    let last = cmp::min(
        grid.bottommost_line(),
        first + i32::try_from(rows).map_err(|_| InvalidDocument)? - 1,
    );
    let mut characters = rows;
    let mut bytes = rows;
    // Validate before the shared native capture allocates row/character storage.
    for raw_line in first.0..=last.0 {
        let row = &grid[Line(raw_line)];
        for column in 0..columns {
            let cell = &row[Column(column)];
            let marks = row.zerowidth(cell).unwrap_or_default();
            characters = characters
                .checked_add(1)
                .and_then(|n| n.checked_add(marks.len()))
                .filter(|n| *n <= MAX_CHARACTERS)
                .ok_or(InvalidDocument)?;
            let hidden = term.style(cell).attrs.contains(Flags::HIDDEN);
            bytes = bytes
                .checked_add(if hidden { 1 } else { cell.c().len_utf8() })
                .filter(|n| *n <= MAX_TEXT_BYTES)
                .ok_or(InvalidDocument)?;
            for mark in marks {
                bytes = bytes
                    .checked_add(if hidden { 1 } else { mark.len_utf8() })
                    .filter(|n| *n <= MAX_TEXT_BYTES)
                    .ok_or(InvalidDocument)?;
            }
        }
    }
    let snapshot = AccessibilitySnapshot::capture(term, size, title, first, last);
    let mut builder = Builder::new(title)?;
    let block = term
        .selection
        .as_ref()
        .filter(|s| s.ty == crate::terminal::selection::SelectionType::Block)
        .and_then(|s| s.to_range(term));
    for (index, line) in snapshot.lines.iter().enumerate() {
        let y = (line.y / snapshot.height).clamp(0., 1.);
        let height = (snapshot.cell_height / snapshot.height).min(1. - y);
        let selected_characters =
            block.filter(|s| (s.start.line.0..=s.end.line.0).contains(&line.grid_line)).map(|s| {
                let offset = |col| {
                    line.column_offsets.get(col).copied().unwrap_or_else(|| line_content_end(line))
                        - line.range.scalar.start
                };
                let mut start = s.start.column.0;
                // Selecting the trailing cell includes the full glyph and its combining marks,
                // matching the terminal's rectangular copy behavior.
                if start > 0
                    && start < columns
                    && grid[Line(line.grid_line)][Column(start)]
                        .layout()
                        .contains(Flags::WIDE_CHAR_SPACER)
                {
                    start -= 1;
                }
                offset(start)..offset(s.end.column.0.saturating_add(1))
            });
        builder.push_line_with_selection(
            index as u64,
            [0., y, 1., height],
            line.characters.iter().map(|character| {
                let value = snapshot.text
                    [character.bytes.start as usize..character.bytes.end as usize]
                    .chars()
                    .next()
                    .unwrap_or(' ');
                let x = character.x / snapshot.width;
                let width = (character.width / snapshot.width).min(1. - x);
                (value, x, width)
            }),
            selected_characters,
        )?;
    }
    let selected = snapshot.selection.as_ref().map(|range| range.scalar.clone()).or_else(|| {
        let cursor = grid.cursor.point.line;
        (block.is_none() && cursor >= first && cursor <= last)
            .then(|| snapshot.cursor.scalar.clone())
    });
    let selection = selected.and_then(|range| {
        Some(Selection {
            anchor: builder.position_at(range.start)?,
            focus: builder.position_at(range.end)?,
        })
    });
    builder.finish(selection, focused)
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

/// Clamp a document offset into the `u32` the accessible document stores.
///
/// Saturating rather than wrapping: a terminal cannot produce 4 GiB of text, and a clamped offset
/// degrades navigation at the very end of an impossible document rather than aliasing to the start.
fn narrow(offset: usize) -> u32 {
    u32::try_from(offset).unwrap_or(u32::MAX)
}

#[expect(
    clippy::too_many_arguments,
    reason = "these established rendering and protocol entry points carry independently required context"
)]
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
        bytes: narrow(byte_start)..narrow(byte_end),
        scalar: narrow(*scalar_offset),
        utf16: narrow(*utf16_offset)..narrow(utf16_offset.saturating_add(utf16_len)),
        column: narrow(column),
        x,
        width,
    });
    *scalar_offset = scalar_offset.saturating_add(1);
    *utf16_offset = utf16_offset.saturating_add(utf16_len);
}

fn point_offset(lines: &[AccessibleLine], point: Point, after: bool) -> Option<usize> {
    let line = lines.iter().find(|line| line.grid_line == point.line.0)?;
    let column = point.column.0.saturating_add(usize::from(after));
    line.column_offsets.get(column).copied().or_else(|| Some(line_content_end(line)))
}

/// Scalar offset just past a line's content, before any trailing newline.
///
/// `column_offsets` only covers the columns a line actually occupies, so every column past that
/// shares this offset — which is what the full-width trailing fill used to store in each of them.
fn line_content_end(line: &AccessibleLine) -> usize {
    line.range.scalar.end.saturating_sub(usize::from(line.hard_break))
}

fn scalar_to_utf16(lines: &[AccessibleLine], scalar: usize) -> Option<usize> {
    let line = lines.iter().find(|line| line.range.scalar.contains(&scalar))?;
    let scalar = narrow(scalar);
    line.characters
        .iter()
        .find(|character| character.scalar == scalar)
        .map(|character| character.utf16.start as usize)
        .or(Some(line.range.utf16.end))
}

fn selection_ranges(
    lines: &[AccessibleLine],
    selection: Option<SelectionRange>,
) -> (Option<AccessibleRange>, Vec<AccessibleRange>) {
    let Some(selection) = selection else { return (None, Vec::new()) };
    if selection.is_block {
        let mut ranges = Vec::new();
        for line in lines.iter().filter(|line| {
            (selection.start.line.0..=selection.end.line.0).contains(&line.grid_line)
        }) {
            let start = Point::new(Line(line.grid_line), selection.start.column);
            let end = Point::new(Line(line.grid_line), selection.end.column);
            if let Some(range) = grid_range(std::slice::from_ref(line), start, end) {
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
    use crate::terminal::term::style::Style;
    use crate::terminal::term::test::TermSize;

    #[test]
    fn lightweight_window_snapshot_has_no_terminal_document() {
        let snapshot = AccessibilitySnapshot::window(size(4, 2), "test", true);
        assert!(snapshot.text.is_empty());
        assert!(snapshot.lines.is_empty());
        assert!(snapshot.focused);
        assert_eq!(snapshot.title, "test");
    }

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
        term.grid_mut()[Point::new(Line(0), Column(0))].set_c('a');
        term.grid_mut()[Point::new(Line(0), Column(1))].set_c('x');
        let hidden = term.styles_mut().intern(Style { attrs: Flags::HIDDEN, ..Style::default() });
        term.grid_mut()[Point::new(Line(0), Column(1))].set_style(hidden);
        term.grid_mut()[Point::new(Line(0), Column(3))].insert_layout(Flags::WRAPLINE);
        term.grid_mut()[Point::new(Line(1), Column(0))].set_c('b');

        let snapshot = AccessibilitySnapshot::new(&term, size(4, 2), "test");
        assert_eq!(snapshot.text, "a   b\n");
        assert!(!snapshot.text.contains('x'));
        assert!(!snapshot.lines[0].hard_break);
        assert!(snapshot.lines[1].hard_break);
    }

    #[test]
    fn hosted_document_keeps_wide_combining_selection_and_hidden_cells_bounded() {
        let mut term = term(4, 1);
        let point = Point::new(Line(0), Column(0));
        term.grid_mut()[point].set_c('🦇');
        term.grid_mut()[point].insert_layout(Flags::WIDE_CHAR);
        term.push_zerowidths(point, std::iter::once('\u{301}'));
        term.grid_mut()[Point::new(Line(0), Column(1))].insert_layout(Flags::WIDE_CHAR_SPACER);
        let hidden = term.styles_mut().intern(Style { attrs: Flags::HIDDEN, ..Style::default() });
        term.grid_mut()[Point::new(Line(0), Column(2))].set_c('x');
        term.grid_mut()[Point::new(Line(0), Column(2))].set_style(hidden);
        let mut selection = Selection::new(SelectionType::Simple, point, Side::Left);
        selection.update(Point::new(Line(0), Column(2)), Side::Right);
        term.selection = Some(selection);
        let doc = visible_document(&term, size(4, 1), "terminal", true).unwrap();
        assert_eq!(doc.lines()[0].text(), "🦇\u{301} \n");
        assert_eq!(doc.lines()[0].character_lengths(), &[4, 2, 1, 1]);
        assert_eq!(doc.lines()[0].character_widths(), &[0.5, 0., 0.25, 0.]);
        assert_eq!(doc.selection().unwrap().focus.character, 3);
        assert!(doc.is_focused());
        assert!(!format!("{doc:?}").contains("terminal"));
        let mut block =
            Selection::new(SelectionType::Block, Point::new(Line(0), Column(1)), Side::Left);
        block.update(Point::new(Line(0), Column(2)), Side::Right);
        term.selection = Some(block);
        let doc = visible_document(&term, size(4, 1), "terminal", true).unwrap();
        assert_eq!(doc.lines()[0].selected_characters(), Some(0..3));
        assert!(doc.selection().is_none());
    }

    #[test]
    fn hosted_visible_text_ranges_preserve_unicode_masking_and_bounded_work() {
        let mut terminal = term(4, 2);
        let point = Point::new(Line(0), Column(0));
        terminal.grid_mut()[point].set_c('🦇');
        terminal.grid_mut()[point].insert_layout(Flags::WIDE_CHAR);
        terminal.push_zerowidths(point, std::iter::once('\u{301}'));
        terminal.grid_mut()[Point::new(Line(0), Column(1))].insert_layout(Flags::WIDE_CHAR_SPACER);
        let hidden =
            terminal.styles_mut().intern(Style { attrs: Flags::HIDDEN, ..Style::default() });
        terminal.grid_mut()[Point::new(Line(0), Column(2))].set_c('x');
        terminal.grid_mut()[Point::new(Line(0), Column(2))].set_style(hidden);
        terminal.grid_mut()[Point::new(Line(1), Column(0))].set_c('b');
        assert_eq!(visible_text(&terminal, size(4, 2), 0, 1).unwrap(), ["🦇\u{301} \n"]);
        assert_eq!(visible_text(&terminal, size(4, 2), 1, 1).unwrap(), ["b\n"]);
        for (start, count) in [(0, 0), (usize::MAX, 2), (1, 2), (2, 1)] {
            assert!(visible_text(&terminal, size(4, 2), start, count).is_err());
        }
        let excessive = term(129, 32);
        assert!(visible_text(&excessive, size(129, 32), 0, 32).is_err());
    }

    #[test]
    fn hosted_document_copies_the_viewport_instead_of_retained_history() {
        let mut term = term(4, 2);
        for (line, text) in ["past", "live"].into_iter().enumerate() {
            for (column, c) in text.chars().enumerate() {
                term.grid_mut()[Point::new(Line(line as i32), Column(column))].set_c(c);
            }
        }
        term.grid_mut().scroll_up(&(Line(0)..Line(2)), 1);
        let current = visible_document(&term, size(4, 2), "terminal", false).unwrap();
        assert_eq!(current.lines().len(), 2);
        assert!(!current.lines().iter().any(|line| line.text().contains("past")));
        term.grid_mut().scroll_display(Scroll::Top);
        let history = visible_document(&term, size(4, 2), "terminal", false).unwrap();
        assert_eq!(history.lines().len(), 2);
        assert!(history.lines()[0].text().contains("past"));
        assert!(!history.is_focused());
    }

    #[test]
    fn hosted_document_rejects_excess_viewports_and_labels_before_capture() {
        let oversized = term(2, vivid_sdk::accessible_text::MAX_LINES + 1);
        assert!(
            visible_document(
                &oversized,
                size(2, vivid_sdk::accessible_text::MAX_LINES + 1),
                "terminal",
                false
            )
            .is_err()
        );
        let term = term(4, 1);
        assert!(
            visible_document(
                &term,
                size(4, 1),
                &"x".repeat(vivid_sdk::accessible_text::MAX_LABEL_BYTES + 1),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn keeps_scalar_and_utf16_offsets_distinct() {
        let mut term = term(4, 1);
        let cell = &mut term.grid_mut()[Point::new(Line(0), Column(0))];
        cell.set_c('🦇');
        cell.insert_layout(Flags::WIDE_CHAR);
        term.push_zerowidths(Point::new(Line(0), Column(0)), std::iter::once('\u{301}'));
        term.grid_mut()[Point::new(Line(0), Column(1))].insert_layout(Flags::WIDE_CHAR_SPACER);
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
        term.grid_mut()[Point::new(Line(0), Column(0))].set_c('a');
        term.grid_mut()[Point::new(Line(1), Column(0))].set_c('b');
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
                term.grid_mut()[Point::new(Line(line as i32), Column(column))].set_c(character);
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
        let document = visible_document(&term, size(3, 2), "test", true).unwrap();
        assert!(document.selection().is_none());
        assert_eq!(document.lines()[0].selected_characters(), Some(0..2));
        assert_eq!(document.lines()[1].selected_characters(), Some(0..2));
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
    fn an_accessible_character_stays_narrow() {
        // This is the per-cell axis of a document spanning the whole scrollback, so its width is
        // the difference between tens of megabytes and twice that on a full history.
        assert!(
            size_of::<AccessibleCharacter>() <= 32,
            "AccessibleCharacter grew to {} bytes",
            size_of::<AccessibleCharacter>(),
        );
    }

    #[test]
    fn offsets_past_a_line_stop_before_its_newline() {
        // `column_offsets` used to be allocated at the full grid width and its tail filled with the
        // offset reached before the trailing newline. Sizing it to the line's own content saves
        // that fill, but only if columns past the content still answer with the same value —
        // `line.range.scalar.end` is one further along whenever the line hard-breaks.
        let mut term = term(8, 2);
        for (column, character) in "ab".chars().enumerate() {
            term.grid_mut()[Point::new(Line(0), Column(column))].set_c(character);
        }
        let snapshot = AccessibilitySnapshot::new(&term, size(8, 2), "test");
        let line = &snapshot.lines[0];

        assert!(line.hard_break, "the fixture line ends in a newline");
        assert_eq!(line.column_offsets.len(), 3, "two cells plus the trailing boundary");
        assert_eq!(line.range.scalar.end, 3, "the range covers the newline");

        // Every column past the content resolves to the pre-newline offset, not the range end.
        for column in 2..8 {
            assert_eq!(
                point_offset(&snapshot.lines, Point::new(Line(0), Column(column)), false),
                Some(2),
                "column {column} must stop before the newline",
            );
        }
    }

    #[test]
    fn a_blank_line_carries_one_offset() {
        // The whole point of the change: a retained scrollback row with nothing on it used to hold
        // `columns + 1` offsets, which is most of a full history.
        let term = term(200, 2);
        let snapshot = AccessibilitySnapshot::new(&term, size(200, 2), "test");

        // Line 0 holds the cursor, which extends it by one column; line 1 is untouched.
        assert_eq!(snapshot.lines[1].column_offsets.len(), 1);
        assert_eq!(snapshot.lines[0].column_offsets.len(), 2, "the cursor occupies one column");
    }

    #[test]
    fn tabs_keep_one_character_with_expanded_geometry() {
        let mut term = term(10, 1);
        term.grid_mut()[Point::new(Line(0), Column(0))].set_c('\t');
        term.grid_mut()[Point::new(Line(0), Column(8))].set_c('x');
        let snapshot = AccessibilitySnapshot::new(&term, size(10, 1), "test");
        assert_eq!(snapshot.text, "\tx\n");
        assert_eq!(snapshot.lines[0].characters[0].width, 80.0);
        assert_eq!(snapshot.lines[0].characters[1].column, 8);
    }

    #[test]
    fn alternate_screen_replaces_the_accessible_document() {
        let mut term = term(3, 1);
        term.grid_mut()[Point::new(Line(0), Column(0))].set_c('p');
        term.swap_alt();
        term.grid_mut()[Point::new(Line(0), Column(0))].set_c('a');
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
            term.grid_mut()[Point::new(Line(0), Column(column))].set_c(character);
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
