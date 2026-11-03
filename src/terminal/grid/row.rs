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
}

impl<T: PartialEq> PartialEq for Row<T> {
    fn eq(&self, other: &Self) -> bool {
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

        Row { inner, occ: 0, extras: RowExtras::default() }
    }

    /// Backing capacity, in cells. Only used to assert that narrowing reclaims it.
    #[cfg(test)]
    pub fn inner_capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Increase the number of columns in the row.
    #[inline]
    pub fn grow(&mut self, columns: usize) {
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
        T: GridCell,
    {
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
}

impl<T> Row<T> {
    #[inline]
    /// Create a row from cells and an occupied-prefix length.
    pub fn from_vec(vec: Vec<T>, occ: usize) -> Row<T> {
        Row { inner: vec, occ, extras: RowExtras::default() }
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
        self.inner.last()
    }

    #[inline]
    /// Borrow the last backing cell mutably and mark the row occupied.
    pub fn last_mut(&mut self) -> Option<&mut T> {
        self.occ = self.inner.len();
        self.inner.last_mut()
    }

    #[inline]
    /// Move cells to the row's end and extend its occupied prefix.
    pub fn append(&mut self, vec: &mut Vec<T>)
    where
        T: GridCell,
    {
        self.occ += vec.len();
        self.inner.append(vec);
    }

    #[inline]
    /// Move cells to the row's beginning and extend its occupied prefix.
    pub fn append_front(&mut self, mut vec: Vec<T>) {
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
        self.inner.iter().all(GridCell::is_empty)
    }

    #[inline]
    /// Remove and return the first `at` cells from this row.
    pub fn front_split_off(&mut self, at: usize) -> Vec<T> {
        self.occ = self.occ.saturating_sub(at);

        let mut split = self.inner.split_off(at);
        std::mem::swap(&mut split, &mut self.inner);
        split
    }
}

impl<T> Row<T> {
    /// Visit every stored cell, including any kept beyond the current width, without marking
    /// cells occupied.
    pub(crate) fn for_each_stored_mut(&mut self, f: impl FnMut(&mut T)) {
        self.inner.iter_mut().for_each(f);
    }

    /// Convert every stored cell, keeping occupancy.
    pub(crate) fn map<U>(&self, mut f: impl FnMut(&Self, &T) -> U) -> Row<U> {
        let inner = self.inner.iter().map(|cell| f(self, cell)).collect();
        Row { inner, occ: self.occ, extras: RowExtras::default() }
    }
}

impl<T> Row<T> {
    /// Hand cells from `start` to `write` until it declines one or `len` cells are written.
    ///
    /// Returns how many it accepted; only those are marked occupied. Checking and writing in one
    /// pass keeps a print from walking the same cells twice.
    #[inline]
    pub(crate) fn write_while(
        &mut self,
        start: Column,
        len: usize,
        mut write: impl FnMut(usize, &mut T) -> bool,
    ) -> usize {
        let mut written = 0;
        for cell in &mut self.inner[start.0..start.0 + len] {
            if !write(written, cell) {
                break;
            }
            written += 1;
        }
        if written > 0 {
            self.occ = max(self.occ, start.0 + written);
        }
        written
    }

    /// The same row with every stored cell converted by `f`, keeping its extra content.
    pub(crate) fn map_owned<U>(self, f: impl FnMut(&T) -> U) -> Row<U> {
        Row { inner: self.inner.iter().map(f).collect(), occ: self.occ, extras: self.extras }
    }
}

impl<T: GridCell> Row<T> {
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
        self.inner.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut Row<T> {
    type IntoIter = slice::IterMut<'a, T>;
    type Item = &'a mut T;

    #[inline]
    fn into_iter(self) -> slice::IterMut<'a, T> {
        self.occ = self.len();
        self.inner.iter_mut()
    }
}

impl<T> Index<Column> for Row<T> {
    type Output = T;

    #[inline]
    fn index(&self, index: Column) -> &T {
        &self.inner[index.0]
    }
}

impl<T> IndexMut<Column> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: Column) -> &mut T {
        self.occ = max(self.occ, *index + 1);
        &mut self.inner[index.0]
    }
}

impl<T> Index<Range<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: Range<Column>) -> &[T] {
        &self.inner[(index.start.0)..(index.end.0)]
    }
}

impl<T> IndexMut<Range<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: Range<Column>) -> &mut [T] {
        self.occ = max(self.occ, *index.end);
        &mut self.inner[(index.start.0)..(index.end.0)]
    }
}

impl<T> Index<RangeTo<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeTo<Column>) -> &[T] {
        &self.inner[..(index.end.0)]
    }
}

impl<T> IndexMut<RangeTo<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeTo<Column>) -> &mut [T] {
        self.occ = max(self.occ, *index.end);
        &mut self.inner[..(index.end.0)]
    }
}

impl<T> Index<RangeFrom<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeFrom<Column>) -> &[T] {
        &self.inner[(index.start.0)..]
    }
}

impl<T> IndexMut<RangeFrom<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeFrom<Column>) -> &mut [T] {
        self.occ = self.len();
        &mut self.inner[(index.start.0)..]
    }
}

impl<T> Index<RangeFull> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, _: RangeFull) -> &[T] {
        &self.inner[..]
    }
}

impl<T> IndexMut<RangeFull> for Row<T> {
    #[inline]
    fn index_mut(&mut self, _: RangeFull) -> &mut [T] {
        self.occ = self.len();
        &mut self.inner[..]
    }
}

impl<T> Index<RangeToInclusive<Column>> for Row<T> {
    type Output = [T];

    #[inline]
    fn index(&self, index: RangeToInclusive<Column>) -> &[T] {
        &self.inner[..=(index.end.0)]
    }
}

impl<T> IndexMut<RangeToInclusive<Column>> for Row<T> {
    #[inline]
    fn index_mut(&mut self, index: RangeToInclusive<Column>) -> &mut [T] {
        self.occ = max(self.occ, *index.end + 1);
        &mut self.inner[..=(index.end.0)]
    }
}
