//! Unicode placeholders: images positioned by ordinary text.
//!
//! A program that cannot send graphics commands where it wants an image, because a multiplexer
//! such as tmux sits between it and the terminal, instead prints the private-use character
//! `U+10EEEE`. The cell's foreground color carries the image id, its underline color the
//! placement id, and up to three combining marks give the row, the column and the id's high byte.
//! Because the cells are plain text, the multiplexer moves, scrolls and redraws them without
//! knowing what they are, and the image follows.
//!
//! The image itself is described once by a *virtual placement* (`U=1`), which has a size in cells
//! but no position. This module decodes the cells and works out which part of the image each run
//! of them shows; it draws nothing.

// Rust guideline compliant 2026-10-07

use super::store::CellPixels;
use crate::terminal::term::cell::Cell;
use crate::terminal::vvte::ansi::Color;

/// The character whose cells stand for part of an image.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Combining marks that encode a number: the mark at index `n` means `n`.
///
/// Copied from `gen/rowcolumn-diacritics.txt` in the Kitty sources, which fixes both the set and
/// its order as part of the protocol. The marks are in ascending code-point order, which
/// [`diacritic_number`] relies on.
const DIACRITICS: [char; 297] = [
    '\u{0305}',
    '\u{030D}',
    '\u{030E}',
    '\u{0310}',
    '\u{0312}',
    '\u{033D}',
    '\u{033E}',
    '\u{033F}',
    '\u{0346}',
    '\u{034A}',
    '\u{034B}',
    '\u{034C}',
    '\u{0350}',
    '\u{0351}',
    '\u{0352}',
    '\u{0357}',
    '\u{035B}',
    '\u{0363}',
    '\u{0364}',
    '\u{0365}',
    '\u{0366}',
    '\u{0367}',
    '\u{0368}',
    '\u{0369}',
    '\u{036A}',
    '\u{036B}',
    '\u{036C}',
    '\u{036D}',
    '\u{036E}',
    '\u{036F}',
    '\u{0483}',
    '\u{0484}',
    '\u{0485}',
    '\u{0486}',
    '\u{0487}',
    '\u{0592}',
    '\u{0593}',
    '\u{0594}',
    '\u{0595}',
    '\u{0597}',
    '\u{0598}',
    '\u{0599}',
    '\u{059C}',
    '\u{059D}',
    '\u{059E}',
    '\u{059F}',
    '\u{05A0}',
    '\u{05A1}',
    '\u{05A8}',
    '\u{05A9}',
    '\u{05AB}',
    '\u{05AC}',
    '\u{05AF}',
    '\u{05C4}',
    '\u{0610}',
    '\u{0611}',
    '\u{0612}',
    '\u{0613}',
    '\u{0614}',
    '\u{0615}',
    '\u{0616}',
    '\u{0617}',
    '\u{0657}',
    '\u{0658}',
    '\u{0659}',
    '\u{065A}',
    '\u{065B}',
    '\u{065D}',
    '\u{065E}',
    '\u{06D6}',
    '\u{06D7}',
    '\u{06D8}',
    '\u{06D9}',
    '\u{06DA}',
    '\u{06DB}',
    '\u{06DC}',
    '\u{06DF}',
    '\u{06E0}',
    '\u{06E1}',
    '\u{06E2}',
    '\u{06E4}',
    '\u{06E7}',
    '\u{06E8}',
    '\u{06EB}',
    '\u{06EC}',
    '\u{0730}',
    '\u{0732}',
    '\u{0733}',
    '\u{0735}',
    '\u{0736}',
    '\u{073A}',
    '\u{073D}',
    '\u{073F}',
    '\u{0740}',
    '\u{0741}',
    '\u{0743}',
    '\u{0745}',
    '\u{0747}',
    '\u{0749}',
    '\u{074A}',
    '\u{07EB}',
    '\u{07EC}',
    '\u{07ED}',
    '\u{07EE}',
    '\u{07EF}',
    '\u{07F0}',
    '\u{07F1}',
    '\u{07F3}',
    '\u{0816}',
    '\u{0817}',
    '\u{0818}',
    '\u{0819}',
    '\u{081B}',
    '\u{081C}',
    '\u{081D}',
    '\u{081E}',
    '\u{081F}',
    '\u{0820}',
    '\u{0821}',
    '\u{0822}',
    '\u{0823}',
    '\u{0825}',
    '\u{0826}',
    '\u{0827}',
    '\u{0829}',
    '\u{082A}',
    '\u{082B}',
    '\u{082C}',
    '\u{082D}',
    '\u{0951}',
    '\u{0953}',
    '\u{0954}',
    '\u{0F82}',
    '\u{0F83}',
    '\u{0F86}',
    '\u{0F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

/// One-based number a combining mark stands for, or zero when it is not one of [`DIACRITICS`].
fn diacritic_number(mark: char) -> u32 {
    // The table has 297 entries, so the index plus one always fits.
    DIACRITICS.binary_search(&mark).map_or(0, |index| index as u32 + 1)
}

/// Number a cell color encodes: the 24 bits of a direct color, or a palette index.
fn color_number(color: Color) -> u32 {
    match color {
        Color::Spec(rgb) => u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b),
        Color::Indexed(index) => u32::from(index),
        // The sixteen named palette entries are indices too; the semantic ones (default
        // foreground and friends) name no image.
        Color::Named(named) => Some(named as u32).filter(|index| *index < 16).unwrap_or(0),
    }
}

/// What one placeholder cell says. Zero stands for "not given" throughout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CellCode {
    /// Low 24 bits of the image id.
    pub id_low: u32,
    /// Placement id.
    pub placement: u32,
    /// One-based row of the image box this cell shows.
    pub row: u32,
    /// One-based column of the image box this cell shows.
    pub col: u32,
    /// One-based high byte of the image id.
    pub id_high: u32,
}

/// Decode `cell` if it is a placeholder.
pub(crate) fn decode_cell(cell: &Cell) -> Option<CellCode> {
    if cell.c != PLACEHOLDER {
        return None;
    }
    let marks = cell.zerowidth().unwrap_or_default();
    let mark = |index: usize| marks.get(index).map_or(0, |mark| diacritic_number(*mark));
    Some(CellCode {
        id_low: color_number(cell.fg),
        placement: cell.underline_color().map_or(0, color_number),
        row: mark(0),
        col: mark(1),
        id_high: mark(2),
    })
}

/// Consecutive placeholder cells of one line that show adjacent columns of one image row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    /// Screen column of the run's first cell.
    pub column: usize,
    /// Cells in the run.
    pub len: u32,
    /// Image id the cells name.
    pub image: u32,
    /// Placement id the cells name, zero for any.
    pub placement: u32,
    /// Zero-based row of the image box the run shows.
    pub img_row: u32,
    /// Zero-based column of the image box the run's first cell shows.
    pub img_col: u32,
}

/// Group the cells of one line into runs, calling `emit` for each.
///
/// `cells` yields one item per column, `None` for a cell that is not a placeholder. A cell with
/// missing marks inherits them from its left neighbor under the protocol's rules: same colors,
/// same row if a row is given, and the next column if a column is given.
pub(crate) fn runs(cells: impl Iterator<Item = Option<CellCode>>, mut emit: impl FnMut(Run)) {
    let mut flush = |end: usize, len: u32, last: CellCode| {
        if len > 0 {
            emit(Run {
                column: end - len as usize,
                len,
                // Only eight bits of the third mark are an id byte; Kitty shifts the rest out.
                image: last.id_low | ((last.id_high - 1) & 0xFF) << 24,
                placement: last.placement,
                img_row: last.row - 1,
                img_col: last.col - len,
            });
        }
    };
    let (mut len, mut last, mut columns) = (0_u32, CellCode::default(), 0);
    for (column, cell) in cells.enumerate() {
        columns = column + 1;
        let continues = cell.filter(|cell| {
            len > 0
                && cell.id_low == last.id_low
                && cell.placement == last.placement
                && (cell.row == 0 || cell.row == last.row)
                && (cell.col == 0 || cell.col == last.col + 1)
                && (cell.id_high == 0 || cell.id_high == last.id_high)
        });
        match (continues, cell) {
            (Some(cell), _) => {
                len += 1;
                last = CellCode { row: last.row, col: last.col + 1, id_high: last.id_high, ..cell };
            },
            (None, started) => {
                flush(column, len, last);
                len = u32::from(started.is_some());
                let cell = started.unwrap_or_default();
                // A first cell with nothing given shows the image's top-left corner.
                last = CellCode {
                    row: cell.row.max(1),
                    col: cell.col.max(1),
                    id_high: cell.id_high.max(1),
                    ..cell
                };
            },
        }
    }
    flush(columns, len, last);
}

/// The part of an image one run shows, in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Slice {
    /// Where drawing starts, relative to the top-left corner of the run's first cell.
    pub dest_x: f32,
    pub dest_y: f32,
    pub dest_w: f32,
    pub dest_h: f32,
    /// Source rectangle in image pixels.
    pub src_x: f32,
    pub src_y: f32,
    pub src_w: f32,
    pub src_h: f32,
}

/// Work out what `run` shows of an image fitted into its virtual placement's box.
///
/// The box is `cols` by `rows` cells, each defaulting to the image's natural extent when zero.
/// The image is scaled to fit the box with its aspect ratio kept, and centered. The run covers
/// one row of cells of that box; the result is the intersection of the two, or `None` when the
/// run lies entirely in the box's padding or outside it.
pub(crate) fn slice(
    (width, height): (u32, u32),
    (cols, rows): (u32, u32),
    run: &Run,
    cell: CellPixels,
) -> Option<Slice> {
    if width == 0 || height == 0 {
        return None;
    }
    let (cell_w, cell_h) = (cell.width as f32, cell.height as f32);
    let (width, height) = (width as f32, height as f32);
    let cols = if cols == 0 { (width / cell_w).ceil() } else { cols as f32 };
    let rows = if rows == 0 { (height / cell_h).ceil() } else { rows as f32 };
    let (box_w, box_h) = (cols * cell_w, rows * cell_h);
    let scale = (box_w / width).min(box_h / height);
    let (image_x, image_y) = ((box_w - width * scale) / 2., (box_h - height * scale) / 2.);

    // The run and the fitted image, both in box pixels.
    let (run_x, run_y) = (run.img_col as f32 * cell_w, run.img_row as f32 * cell_h);
    let left = run_x.max(image_x);
    let top = run_y.max(image_y);
    let right = (run_x + run.len as f32 * cell_w).min(image_x + width * scale);
    let bottom = (run_y + cell_h).min(image_y + height * scale);
    if left >= right || top >= bottom {
        return None;
    }
    Some(Slice {
        dest_x: left - run_x,
        dest_y: top - run_y,
        dest_w: right - left,
        dest_h: bottom - top,
        src_x: (left - image_x) / scale,
        src_y: (top - image_y) / scale,
        src_w: (right - left) / scale,
        src_h: (bottom - top) / scale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::vvte::ansi::{NamedColor, Rgb};

    const CELL: CellPixels = CellPixels { width: 10, height: 20 };

    fn code(id_low: u32, row: u32, col: u32, id_high: u32) -> Option<CellCode> {
        Some(CellCode { id_low, placement: 0, row, col, id_high })
    }

    fn collect(cells: &[Option<CellCode>]) -> Vec<Run> {
        let mut found = Vec::new();
        runs(cells.iter().copied(), |run| found.push(run));
        found
    }

    fn run(column: usize, len: u32, image: u32, img_row: u32, img_col: u32) -> Run {
        Run { column, len, image, placement: 0, img_row, img_col }
    }

    #[test]
    fn the_table_is_the_protocols_and_is_searchable() {
        assert!(DIACRITICS.is_sorted(), "binary search needs ascending order");
        assert_eq!(diacritic_number('\u{0305}'), 1);
        assert_eq!(diacritic_number('\u{030D}'), 2);
        assert_eq!(diacritic_number('\u{030E}'), 3);
        assert_eq!(diacritic_number('\u{1D244}'), 297);
        assert_eq!(diacritic_number('\u{0301}'), 0, "the acute accent is deliberately not in it");
        assert_eq!(diacritic_number('a'), 0);
    }

    #[test]
    fn a_cell_is_decoded_from_its_colors_and_marks() {
        let mut cell =
            Cell { c: PLACEHOLDER, fg: Color::Spec(Rgb { r: 1, g: 2, b: 3 }), ..Cell::default() };
        assert_eq!(decode_cell(&cell), code(0x010203, 0, 0, 0));
        for mark in ['\u{030D}', '\u{0305}', '\u{030E}', '\u{0305}'] {
            cell.push_zerowidth(mark);
        }
        assert_eq!(decode_cell(&cell), code(0x010203, 2, 1, 3), "a fourth mark means nothing");

        cell.fg = Color::Indexed(42);
        assert_eq!(decode_cell(&cell).unwrap().id_low, 42);
        cell.fg = Color::Named(NamedColor::Red);
        assert_eq!(decode_cell(&cell).unwrap().id_low, 1);
        cell.fg = Color::Named(NamedColor::Foreground);
        assert_eq!(decode_cell(&cell).unwrap().id_low, 0, "the default color names no image");

        cell.c = 'x';
        assert_eq!(decode_cell(&cell), None);
    }

    #[test]
    fn fully_marked_cells_form_one_run_per_image_row() {
        // What kitim prints for a 3x1 box under vvmux: row, column and high byte on every cell.
        let cells = [code(7, 1, 1, 5), code(7, 1, 2, 5), code(7, 1, 3, 5)];
        assert_eq!(collect(&cells), [run(0, 3, 7 | 4 << 24, 0, 0)]);
    }

    #[test]
    fn missing_marks_are_inherited_from_the_left() {
        // Row mark on the first cell only, as the protocol's own example does.
        let cells = [None, code(42, 2, 0, 0), code(42, 0, 0, 0), code(42, 0, 0, 0), None];
        assert_eq!(collect(&cells), [run(1, 3, 42, 1, 0)]);
        // Nothing at all: the top-left corner, then onwards.
        assert_eq!(collect(&[code(42, 0, 0, 0), code(42, 0, 0, 0)]), [run(0, 2, 42, 0, 0)]);
        // A run that begins in the middle of the image.
        assert_eq!(collect(&[code(42, 3, 5, 0), code(42, 3, 6, 0)]), [run(0, 2, 42, 2, 4)]);
    }

    #[test]
    fn a_run_breaks_wherever_the_cells_stop_agreeing() {
        let other_image = [code(1, 1, 1, 0), code(2, 1, 2, 0)];
        assert_eq!(collect(&other_image), [run(0, 1, 1, 0, 0), run(1, 1, 2, 0, 1)]);
        let other_row = [code(1, 1, 1, 0), code(1, 2, 2, 0)];
        assert_eq!(collect(&other_row), [run(0, 1, 1, 0, 0), run(1, 1, 1, 1, 1)]);
        let column_jump = [code(1, 1, 1, 0), code(1, 1, 3, 0)];
        assert_eq!(collect(&column_jump), [run(0, 1, 1, 0, 0), run(1, 1, 1, 0, 2)]);
        let other_high_byte = [code(1, 1, 1, 2), code(1, 1, 2, 3)];
        assert_eq!(
            collect(&other_high_byte),
            [run(0, 1, 1 | 1 << 24, 0, 0), run(1, 1, 1 | 2 << 24, 0, 1)]
        );
        let gap = [code(1, 1, 1, 0), None, code(1, 1, 2, 0)];
        assert_eq!(collect(&gap), [run(0, 1, 1, 0, 0), run(2, 1, 1, 0, 1)]);

        let placement =
            |placement| Some(CellCode { id_low: 1, placement, row: 1, col: 0, id_high: 0 });
        let found = collect(&[placement(3), placement(4)]);
        assert_eq!(
            found.iter().map(|run| (run.len, run.placement)).collect::<Vec<_>>(),
            [(1, 3), (1, 4)]
        );
    }

    #[test]
    fn no_placeholders_means_no_runs() {
        assert_eq!(collect(&[None, None]), []);
        assert_eq!(collect(&[]), []);
    }

    #[test]
    fn an_image_that_fills_its_box_is_cut_into_cell_rows() {
        // 40x40 pixels in a 4x2 box of 10x20 cells: no scaling, no padding.
        let slice = |img_row, img_col, len| {
            slice((40, 40), (4, 2), &run(0, len, 1, img_row, img_col), CELL).unwrap()
        };
        assert_eq!(
            slice(0, 0, 4),
            Slice {
                dest_x: 0.,
                dest_y: 0.,
                dest_w: 40.,
                dest_h: 20.,
                src_x: 0.,
                src_y: 0.,
                src_w: 40.,
                src_h: 20.,
            }
        );
        let second = slice(1, 1, 2);
        assert_eq!((second.src_x, second.src_y, second.src_w, second.src_h), (10., 20., 20., 20.));
        assert_eq!((second.dest_x, second.dest_w), (0., 20.));
    }

    #[test]
    fn a_wide_image_is_centered_vertically_and_its_padding_is_not_drawn() {
        // 100x10 into a 10x3 box (100x60 pixels): full width, 10 pixels tall, 25 of padding
        // above and below.
        let middle = slice((100, 10), (10, 3), &run(0, 10, 1, 1, 0), CELL).unwrap();
        assert_eq!(
            (middle.dest_y, middle.dest_h),
            (5., 10.),
            "25..35 of the box, inside row 20..40"
        );
        assert_eq!((middle.src_y, middle.src_h), (0., 10.));
        assert!(slice((100, 10), (10, 3), &run(0, 10, 1, 0, 0), CELL).is_none(), "all padding");
        assert!(slice((100, 10), (10, 3), &run(0, 10, 1, 2, 0), CELL).is_none());
    }

    #[test]
    fn a_tall_image_is_centered_horizontally_and_scaled() {
        // 10x80 into a 4x2 box (40x40): scaled by a half to 5x40, 17.5 of padding either side.
        let top = slice((10, 80), (4, 2), &run(0, 4, 1, 0, 0), CELL).unwrap();
        assert_eq!((top.dest_x, top.dest_w), (17.5, 5.));
        assert_eq!((top.src_x, top.src_w, top.src_y, top.src_h), (0., 10., 0., 40.));
        // A run of only the first column sits wholly in the padding.
        assert!(slice((10, 80), (4, 2), &run(0, 1, 1, 0, 0), CELL).is_none());
        // One starting at the second column begins 7.5 pixels into its own first cell.
        let inner = slice((10, 80), (4, 2), &run(0, 1, 1, 0, 1), CELL).unwrap();
        assert_eq!((inner.dest_x, inner.dest_w), (7.5, 2.5));
    }

    #[test]
    fn an_unsized_box_takes_the_images_natural_extent_in_cells() {
        // 25x30 needs 3x2 cells (30x40): scaled up by 1.2 to fill the width, centered vertically.
        let first = slice((25, 30), (0, 0), &run(0, 3, 1, 0, 0), CELL).unwrap();
        assert_eq!((first.dest_x, first.dest_w), (0., 30.));
        assert_eq!((first.dest_y, first.dest_h), (2., 18.));
    }

    #[test]
    fn cells_outside_the_box_show_nothing() {
        assert!(slice((40, 40), (4, 2), &run(0, 2, 1, 2, 0), CELL).is_none(), "below the box");
        assert!(slice((40, 40), (4, 2), &run(0, 2, 1, 0, 4), CELL).is_none(), "right of the box");
        let partial = slice((40, 40), (4, 2), &run(0, 4, 1, 0, 2), CELL).unwrap();
        assert_eq!(partial.dest_w, 20., "only the two columns that exist");
        assert!(slice((0, 40), (4, 2), &run(0, 1, 1, 0, 0), CELL).is_none());
    }
}
