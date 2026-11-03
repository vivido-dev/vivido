//! Defines the Row type which makes up lines in the grid.

use std::cmp::{max, min};
use std::ops::{Index, IndexMut, Range, RangeFrom, RangeFull, RangeTo, RangeToInclusive};
use std::slice;

use serde::{Deserialize, Serialize};

use crate::terminal::grid::GridCell;
use crate::terminal::index::Column;
use crate::terminal::term::cell::ResetDiscriminant;
use crate::terminal::term::extra::{CellExtra, ExtraId, RowExtras};

/// A row in the grid.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Row<T> {
    inner: Vec<T>,

    /// Maximum number of occupied entries.
    ///
    /// This is the upper bound on the number of elements in the row, which have been modified
    /// since the last reset. All cells after this point are guaranteed to be equal.
    pub(crate) occ: usize,

    /// Combining characters and resize tracking of this row's cells.
    ///
    /// Not serialized: serialized grids carry each cell's content written out.
    #[serde(skip)]
    extras: RowExtras,

    /// Cells that read as `blank` but still hold what was there before a lazy reset.
    ///
    /// Empty except between [`Row::reset_lazily`] and the moment the row is settled. Cells outside
    /// it hold their true content. Single-cell access works through it; slices and iteration need
    /// the row settled first.
    #[serde(skip)]
    stale: Range<usize>,

    /// What the cells in `stale` read as: the lazy reset's blank cell.
    #[serde(skip)]
    blank: T,
}

impl<T: PartialEq> PartialEq for Row<T> {
    fn eq(&self, other: &Self) -> bool {
        self.debug_assert_settled();
        other.debug_assert_settled();
        self.inner == other.inner
    }
}

impl<T: Default> Row<T> {
    /// Create a new terminal row.
    ///
    /// Each cell is initialized independently with `T::default()`.
    ///
    /// # Panics
    /// Panics if `columns` is zero or an element's default constructor panics.
    pub fn new(columns: usize) -> Row<T> {
        assert!(columns >= 1, "a terminal row requires at least one column");

        let mut inner: Vec<T> = Vec::with_capacity(columns);
        inner.resize_with(columns, T::default);

        Row { inner, occ: 0, extras: RowExtras::default(), stale: 0..0, blank: T::default() }
    }

    /// Backing capacity, in cells. Only used to assert that narrowing reclaims it.
    #[cfg(test)]
    pub fn inner_capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Increase the number of columns in the row.
    #[inline]
    pub fn grow(&mut self, columns: usize)
    where
        T: Clone,
    {
        self.settle();
        if self.inner.len() >= columns {
            return;
        }

        self.inner.resize_with(columns, T::default);
    }

    /// Reduce the number of columns in the row.
    ///
    /// This will return all non-empty cells that were removed.
    pub fn shrink(&mut self, columns: usize) -> Option<Vec<T>>
    where
        T: GridCell + Clone,
    {
        self.settle();
        if self.inner.len() <= columns {
            return None;
        }

        // Split off cells for a new row.
        let mut new_row = self.inner.split_off(columns);
        let index = new_row.iter().rposition(|c| !c.is_empty()).map_or(0, |i| i + 1);
        new_row.truncate(index);

        self.occ = min(self.occ, columns);

        // `split_off` leaves this row's capacity at the width it used to have, so narrowing a
        // window keeps every scrollback row sized for its old column count — at 24 bytes a cell
        // that is tens of megabytes across a full history. Reclaim it only once the waste is worth
        // a reallocation, so dragging a window narrower a column at a time does not rebuild the
        // whole scrollback on every step.
        if self.inner.capacity() >= self.inner.len().saturating_mul(2) {
            self.inner.shrink_to_fit();
        }

        if new_row.is_empty() { None } else { Some(new_row) }
    }

    /// Reset all cells in the row to the `template` cell.
    #[inline]
    pub fn reset<D>(&mut self, template: &T)
    where
        T: ResetDiscriminant<D> + GridCell,
        D: PartialEq,
    {
        debug_assert!(!self.inner.is_empty());

        // Stale cells are overwritten below like any other dirty cell.
        self.occ = max(self.occ, self.stale.end);
        self.stale = 0..0;

        // Mark all cells as dirty if template cell changed.
        let len = self.inner.len();
        if self.inner[len - 1].discriminant() != template.discriminant() {
            self.occ = len;
        }

        // Reset every dirty cell in the row.
        for item in &mut self.inner[0..self.occ] {
            item.reset(template);
        }

        // Cells past `occ` were already reset, so no cell refers to the extra content now.
        self.extras.clear();
        self.occ = 0;
    }

    /// Reset the row like [`Row::reset`] without writing any cell yet.
    ///
    /// The cells that need clearing become stale: they read as the reset value and are written
    /// only when something writes near them or the row is settled. Text that overwrites them
    /// first is then stored once instead of twice.
    #[cfg_attr(not(test), expect(dead_code, reason = "the lazy scroll arrives in the next step"))]
    pub(crate) fn reset_lazily<D>(&mut self, template: &T)
    where
        T: ResetDiscriminant<D> + GridCell + Clone,
        D: PartialEq,
    {
        debug_assert!(!self.inner.is_empty());

        let len = self.inner.len();
        let dirty = if self[Column(len - 1)].discriminant() == template.discriminant() {
            // Cells past both bounds were reset with an equal template and still hold it.
            max(self.occ, self.stale.end)
        } else {
            len
        };
        let mut blank = template.clone();
        blank.reset(template);
        self.blank = blank;
        self.stale = 0..dirty;
        self.extras.clear();
        self.occ = 0;
    }
}

impl<T> Row<T> {
    #[inline]
    /// Create a row from cells and an occupied-prefix length.
    pub fn from_vec(vec: Vec<T>, occ: usize) -> Row<T>
    where
        T: Default,
    {
        Row { inner: vec, occ, extras: RowExtras::default(), stale: 0..0, blank: T::default() }
    }

    /// Whether some cells only read as blank and still need writing.
    #[inline]
    pub(crate) fn is_pending(&self) -> bool {
        !self.stale.is_empty()
    }

    /// Write every stale cell as the blank it reads as.
    #[inline]
    pub(crate) fn settle(&mut self)
    where
        T: Clone,
    {
        if self.is_pending() {
            self.settle_slow();
        }
    }

    #[inline(never)]
    fn settle_slow(&mut self)
    where
        T: Clone,
    {
        let stale = std::mem::replace(&mut self.stale, 0..0);
        self.inner[stale].fill(self.blank.clone());
    }

    /// Write the stale cells up to and including `column`, so it can be changed in place.
    #[inline]
    fn materialize_through(&mut self, column: usize)
    where
        T: Clone,
    {
        if self.stale.contains(&column) {
            self.inner[self.stale.start..=column].fill(self.blank.clone());
            self.stale.start = column + 1;
            if self.stale.is_empty() {
                self.stale = 0..0;
            }
        }
    }

    /// Slice and iterator access reads cells directly, so the row must not be pending.
    #[inline]
    fn debug_assert_settled(&self) {
        debug_assert!(!self.is_pending(), "row read as a slice while lazily cleared");
    }

    /// Whether this row has no backing cells.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    #[inline]
    /// Return the number of backing cells in this row.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[inline]
    pub(crate) fn occupied_len(&self) -> usize {
        self.occ
    }

    #[inline]
    /// Borrow the last backing cell, if the row is nonempty.
    pub fn last(&self) -> Option<&T> {
        self.debug_assert_settled();
        self.inner.last()
    }

    #[inline]
    /// Borrow the last backing cell mutably and mark the row occupied.
    pub fn last_mut(&mut self) -> Option<&mut T>
    where
        T: Clone,
    {
        self.settle();
        self.occ = self.inner.len();
        self.inner.last_mut()
    }

    #[inline]
    /// Move cells to the row's end and extend its occupied prefix.
    pub fn append(&mut self, vec: &mut Vec<T>)
    where
        T: GridCell + Clone,
    {
        self.settle();
        self.occ += vec.len();
        self.inner.append(vec);
    }

    #[inline]
    /// Move cells to the row's beginning and extend its occupied prefix.
    pub fn append_front(&mut self, mut vec: Vec<T>)
    where
        T: Clone,
    {
        self.settle();
        self.occ += vec.len();

        vec.append(&mut self.inner);
        self.inner = vec;
    }

    /// Check if all cells in the row are empty.
    #[inline]
    pub fn is_clear(&self) -> bool
    where
        T: GridCell,
    {
        self.debug_assert_settled();
        self.inner.iter().all(GridCell::is_empty)
    }

    #[inline]
    /// Remove and return the first `at` cells from this row.
    pub fn front_split_off(&mut self, at: usize) -> Vec<T>
    where
        T: Clone,
    {
        self.settle();
        self.occ = self.occ.saturating_sub(at);

        let mut split = self.inner.split_off(at);
        std::mem::swap(&mut split, &mut self.inner);
        split
    }
}

impl<T> Row<T> {
    /// Visit every stored cell, including any kept beyond the current width, without marking
    /// cells occupied.
    pub(crate) fn for_each_stored_mut(&mut self, f: impl FnMut(&mut T))
    where
        T: Clone,
    {
        self.settle();
        self.inner.iter_mut().for_each(f);
    }

    /// Convert every stored cell, keeping occupancy.
    pub(crate) fn map<U: Default>(&self, mut f: impl FnMut(&Self, &T) -> U) -> Row<U> {
        self.debug_assert_settled();
        let inner = self.inner.iter().map(|cell| f(self, cell)).collect();
        Row { inner, occ: self.occ, extras: RowExtras::default(), stale: 0..0, blank: U::default() }
    }
}

impl<T> Row<T> {
    /// Let `write` fill a prefix of the `len` cells from `start`, and mark only that prefix
    /// occupied; `write` returns how many cells it wrote and must leave the rest untouched.
    ///
    /// `write` also gets how many leading cells of its slice read as blank but still hold stale
    /// content: it must treat them as blank instead of looking at them. Stale cells before `start`
    /// are written as blank first, so text written left to right into a lazily reset row stores
    /// each cell once.
    #[inline]
    pub(crate) fn write_prefix(
        &mut self,
        start: Column,
        len: usize,
        write: impl FnOnce(&mut [T], usize) -> usize,
    ) -> usize
    where
        T: Clone,
    {
        let (start, end) = (start.0, start.0 + len);
        let mut fresh = 0;
        if start < self.stale.end && self.stale.start < end {
            if self.stale.start <= start {
                self.inner[self.stale.start..start].fill(self.blank.clone());
                self.stale.start = start;
                fresh = self.stale.end.min(end) - start;
            } else {
                // Real cells come before the stale ones in the slice; this never happens for text
                // written left to right, so keep it simple.
                self.settle();
            }
        }
        let written = write(&mut self.inner[start..end], fresh);
        debug_assert!(written <= len);
        if written > 0 {
            self.occ = max(self.occ, start + written);
            if fresh > 0 {
                self.stale.start = start + written.min(fresh);
                if self.stale.is_empty() {
                    self.stale = 0..0;
                }
            }
        }
        written
    }

    /// The same row with every stored cell converted by `f`, keeping its extra content.
    pub(crate) fn map_owned<U: Default>(mut self, f: impl FnMut(&T) -> U) -> Row<U>
    where
        T: Clone,
    {
        self.settle();
        let inner = self.inner.iter().map(f).collect();
        Row { inner, occ: self.occ, extras: self.extras, stale: 0..0, blank: U::default() }
    }
}

impl<T: GridCell + Clone> Row<T> {
    /// The extra content this row's cells refer to.
    #[inline]
    pub fn extras(&self) -> &RowExtras {
        &self.extras
    }

    /// The combining characters drawn over `cell`, a cell of this row, if any.
    #[inline]
    pub fn zerowidth(&self, cell: &T) -> Option<&[char]> {
        self.extras.zerowidth(cell.extra())
    }

    /// Point the cell at `column` at its extra content with `update` applied.
    ///
    /// Compacts the row's entries when dead ones, left by overwrites, reach twice its width, so a
    /// row redrawn in place keeps its memory bounded.
    pub(crate) fn update_extra(&mut self, column: Column, update: impl FnOnce(&mut CellExtra)) {
        // Compaction below reads every cell's id, and stale cells may hold ids of dropped entries.
        self.settle();
        let id = self.extras.update(self.inner[column.0].extra(), update);
        self[column].set_extra(id);
        // Twice the width, but never beyond what a packed cell can number.
        let limit = self.inner.len().max(8).saturating_mul(2).min(ExtraId::PACKED_CAPACITY - 1);
        if self.extras.len() > limit {
            let remap = self.extras.compact(self.inner.iter().map(GridCell::extra));
            for cell in &mut self.inner {
                cell.set_extra(RowExtras::remapped(&remap, cell.extra()));
            }
        }
    }

    /// Move every cell's extra content into `gathered`, pointing the cells at it there.
    ///
    /// Used while resize moves cells between rows; [`Row::import_extras`] reverses it.
    pub(crate) fn export_extras(&mut self, gathered: &mut Vec<CellExtra>) {
        self.settle();
        if self.extras.is_empty() {
            return;
        }
        for cell in &mut self.inner {
            cell.set_extra(self.extras.export(cell.extra(), gathered));
        }
        self.extras.clear();
    }

    /// Take the extra content this row's cells point at in `gathered` back into the row.
    pub(crate) fn import_extras(&mut self, gathered: &mut [CellExtra]) {
        for (column, cell) in self.inner.iter_mut().enumerate() {
            if cell.extra() != ExtraId::NONE {
                cell.set_extra(self.extras.import(cell.extra(), gathered));
                self.occ = self.occ.max(column + 1);
            }
        }
    }
}

impl<'a, T> IntoIterator for &'a Row<T> {
    type IntoIter = slice::Iter<'a, T>;
    type Item = &'a T;

    #[inline]
    fn into_iter(self) -> slice::Iter<'a, T> {
        self.debug_assert_settled();
        self.inner.iter()
    }
}

impl<'a, T: Clone> IntoIterator for &'a mut Row<T> {
    type IntoIter = slice::IterMut<'a, T>;
    type Item = &'a mut T;

    #[inline]
    fn into_iter(self) -> slice::IterMut<'a, T> {
        self.settle();
        self.occ = self.len();
        self.inner.iter_mut()
    }
}

impl<T> Index<Column> for Row<T> {
    type Output = T;

    #[inline]
    fn index(&self, index: Column) -> &T {
        let cell = &self.inner[index.0];
        if self.stale.contains(&index.0) { &self.blank } else { cell }
    }
}

impl<T: Clone> IndexMut<Column> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: Column) -> &mut T {
        self.materialize_through(index.0);
        self.occ = max(self.occ, *index + 1);
        &mut self.inner[index.0]
    }
}

impl<T> Index<Range<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: Range<Column>) -> &[T] {
        self.debug_assert_settled();
        &self.inner[(index.start.0)..(index.end.0)]
    }
}

impl<T: Clone> IndexMut<Range<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: Range<Column>) -> &mut [T] {
        self.settle();
        self.occ = max(self.occ, *index.end);
        &mut self.inner[(index.start.0)..(index.end.0)]
    }
}

impl<T> Index<RangeTo<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeTo<Column>) -> &[T] {
        self.debug_assert_settled();
        &self.inner[..(index.end.0)]
    }
}

impl<T: Clone> IndexMut<RangeTo<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeTo<Column>) -> &mut [T] {
        self.settle();
        self.occ = max(self.occ, *index.end);
        &mut self.inner[..(index.end.0)]
    }
}

impl<T> Index<RangeFrom<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeFrom<Column>) -> &[T] {
        self.debug_assert_settled();
        &self.inner[(index.start.0)..]
    }
}

impl<T: Clone> IndexMut<RangeFrom<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeFrom<Column>) -> &mut [T] {
        self.settle();
        self.occ = self.len();
        &mut self.inner[(index.start.0)..]
    }
}

impl<T> Index<RangeFull> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, _: RangeFull) -> &[T] {
        self.debug_assert_settled();
        &self.inner[..]
    }
}

impl<T: Clone> IndexMut<RangeFull> for Row<T> {
    #[inline]
    fn index_mut(&mut self, _: RangeFull) -> &mut [T] {
        self.settle();
        self.occ = self.len();
        &mut self.inner[..]
    }
}

impl<T> Index<RangeToInclusive<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeToInclusive<Column>) -> &[T] {
        self.debug_assert_settled();
        &self.inner[..=(index.end.0)]
    }
}

impl<T: Clone> IndexMut<RangeToInclusive<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeToInclusive<Column>) -> &mut [T] {
        self.settle();
        self.occ = max(self.occ, *index.end + 1);
        &mut self.inner[..=(index.end.0)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::terminal::term::cell::{Cell, fill_ascii};
    use crate::terminal::term::style::{Style, StyleId, StyleTable};
    use crate::terminal::vvte::ansi::Color;

    const COLUMNS: usize = 12;

    fn written(row: &mut Row<Cell>, text: &str) {
        let style = StyleId::default();
        let bytes = text.as_bytes();
        let n = row.write_prefix(Column(0), bytes.len(), |cells, fresh| {
            fill_ascii(cells, fresh, bytes, style)
        });
        assert_eq!(n, bytes.len());
    }

    /// Every cell as single-cell access reads it.
    fn cells(row: &Row<Cell>) -> Vec<Cell> {
        (0..row.len()).map(|column| row[Column(column)].clone()).collect()
    }

    fn erase(bg: u8) -> Cell {
        let mut styles = StyleTable::default();
        let mut cell = Cell::default();
        cell.set_style(styles.intern(Style { bg: Color::Indexed(bg), ..Style::default() }));
        cell
    }

    #[test]
    fn a_lazily_reset_row_reads_blank_until_written() {
        let mut row = Row::<Cell>::new(COLUMNS);
        written(&mut row, "hello world");
        row.reset_lazily(&Cell::default());
        assert!(row.is_pending());
        assert!(cells(&row).iter().all(|cell| *cell == Cell::default()));

        // Writing from column 0 stores each cell once and leaves the rest stale.
        written(&mut row, "hi");
        assert_eq!((row[Column(0)].c(), row[Column(1)].c(), row[Column(2)].c()), ('h', 'i', ' '));
        assert_eq!(row.stale, 2..11, "only the cells the text did not reach stay stale");
        row.settle();
        assert!(!row.is_pending());
        assert!(row[..].iter().skip(2).all(|cell| *cell == Cell::default()));
    }

    #[test]
    fn writing_past_the_stale_start_blanks_the_gap_first() {
        let mut row = Row::<Cell>::new(COLUMNS);
        written(&mut row, "abcdefghij");
        row.reset_lazily(&Cell::default());
        let bytes = b"xy";
        row.write_prefix(Column(4), 2, |cells, fresh| {
            assert_eq!(fresh, 2, "both target cells are stale");
            fill_ascii(cells, fresh, bytes, StyleId::default())
        });
        row.settle();
        let text: String = row[..].iter().map(Cell::c).collect();
        assert_eq!(text, "    xy      ");
    }

    #[test]
    fn changing_one_cell_materializes_only_up_to_it() {
        let mut row = Row::<Cell>::new(COLUMNS);
        written(&mut row, "abcdefghij");
        row.reset_lazily(&Cell::default());
        row[Column(3)].set_c('\t');
        assert_eq!(row.stale, 4..10);
        assert_eq!(row[Column(3)].c(), '\t');
        assert!((0..3).all(|column| row[Column(column)] == Cell::default()));
    }

    #[test]
    fn a_new_background_makes_every_cell_stale() {
        let mut row = Row::<Cell>::new(COLUMNS);
        written(&mut row, "ab");
        row.reset_lazily(&erase(4));
        assert_eq!(row.stale, 0..COLUMNS, "cells past the text still hold the old background");
        assert!(cells(&row).iter().all(|cell| *cell == erase(4)));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "lazily cleared")]
    fn a_slice_of_a_pending_row_is_a_bug() {
        let mut row = Row::<Cell>::new(COLUMNS);
        written(&mut row, "ab");
        row.reset_lazily(&Cell::default());
        let _ = &row[..];
    }

    /// Drive a lazily reset row and an eagerly reset one through the same operations and compare
    /// what single-cell access reads after every step.
    #[test]
    fn lazy_and_eager_rows_read_the_same_through_any_sequence() {
        let templates = [Cell::default(), erase(1), erase(2)];
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        let (mut lazy, mut eager) = (Row::<Cell>::new(COLUMNS), Row::<Cell>::new(COLUMNS));
        for step in 0..20_000 {
            match next(6) {
                0 => {
                    let template = &templates[next(templates.len())];
                    lazy.reset_lazily(template);
                    eager.reset(template);
                },
                1 | 2 => {
                    let start = next(COLUMNS);
                    let len = next(COLUMNS - start) + 1;
                    let text: Vec<u8> = (0..len).map(|_| b'a' + next(26) as u8).collect();
                    let write = |row: &mut Row<Cell>| {
                        row.write_prefix(Column(start), len, |cells, fresh| {
                            fill_ascii(cells, fresh, &text, StyleId::default())
                        })
                    };
                    assert_eq!(write(&mut lazy), write(&mut eager), "step {step}");
                },
                3 => {
                    let column = Column(next(COLUMNS));
                    lazy[column].set_c('\t');
                    eager[column].set_c('\t');
                },
                4 => {
                    let column = Column(next(COLUMNS));
                    lazy[column].insert_layout(crate::terminal::term::cell::Flags::WIDE_CHAR);
                    eager[column].insert_layout(crate::terminal::term::cell::Flags::WIDE_CHAR);
                },
                _ => lazy.settle(),
            }
            assert_eq!(cells(&lazy), cells(&eager), "step {step}");
            assert!(lazy.occ <= COLUMNS && lazy.stale.end <= COLUMNS);
        }
        lazy.settle();
        assert_eq!(lazy, eager);
    }
}
