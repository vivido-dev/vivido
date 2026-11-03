//! Rarely set cell content kept by each row: combining characters and resize tracking.
//!
//! A cell holds an [`ExtraId`], an index into its row's [`RowExtras`]. Entries never change once
//! added: adding a combining mark adds the longer sequence as a new entry, and the cell points at
//! it. The empty value is always [`ExtraId::NONE`], which keeps "has nothing extra" a single
//! comparison for grid code.
//!
//! Keeping entries with their row means clearing the row frees them, with no terminal-wide sweep.
//! A row that is overwritten in place compacts its own entries once the dead ones outnumber its
//! width. Cells only change rows during resize and reflow; the grid gathers every row's entries
//! into one list before it moves cells and hands them back to their new rows afterwards (see
//! [`RowExtras::export`] and [`RowExtras::import`]).

use std::mem;

use smallvec::SmallVec;

/// Combining characters and temporary resize tracking attached to one cell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CellExtra {
    /// Zero-width characters drawn on top of the cell's character, in input order.
    ///
    /// Four fit without a second allocation; longer graphemes spill without truncation.
    pub(crate) zerowidth: SmallVec<[char; 4]>,
    /// Indices used to carry authenticated Vivid anchors through grid resize and reflow.
    ///
    /// They never survive a resize and are not part of serialized terminal state.
    pub(crate) vivid_resize_tracking: Vec<usize>,
}

impl CellExtra {
    fn is_empty(&self) -> bool {
        self.zerowidth.is_empty() && self.vivid_resize_tracking.is_empty()
    }
}

/// A cell's reference to its [`CellExtra`] in the [`RowExtras`] of the row that holds it.
///
/// The top bit caches [`ExtraId::has_zerowidth`] so grid code can answer it without the row's
/// entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ExtraId(u32);

impl ExtraId {
    /// Nothing extra.
    pub const NONE: Self = Self(0);

    /// Bit set when the entry has combining characters.
    const HAS_ZEROWIDTH: u32 = 1 << 31;

    /// Entry `position` (from zero) of a list, for `extra`.
    ///
    /// # Panics
    ///
    /// Panics if `position` does not fit below the flag bit; a row holds at most a few times its
    /// width in entries, and a list gathered for resize at most one per cell.
    fn new(position: usize, extra: &CellExtra) -> Self {
        let number = u32::try_from(position + 1)
            .ok()
            .filter(|number| *number < Self::HAS_ZEROWIDTH)
            .expect("more combining-character entries than cells can address");
        let bit = if extra.zerowidth.is_empty() { 0 } else { Self::HAS_ZEROWIDTH };
        Self(number | bit)
    }

    /// Position in the list, or `None` for [`ExtraId::NONE`].
    #[inline]
    fn position(self) -> Option<usize> {
        ((self.0 & !Self::HAS_ZEROWIDTH) as usize).checked_sub(1)
    }

    /// Whether the entry holds combining characters, not just resize tracking.
    #[inline]
    pub fn has_zerowidth(self) -> bool {
        self.0 & Self::HAS_ZEROWIDTH != 0
    }

    /// Entry numbers a packed cell can hold; see [`ExtraId::packed`].
    const PACKED_NUMBER_BITS: u32 = 13;

    /// Bits this id takes in a packed cell: the entry number, then the combining bit.
    pub(crate) const PACKED_BITS: u32 = Self::PACKED_NUMBER_BITS + 1;

    /// Most entries a row whose cells are packed can address.
    pub(crate) const PACKED_CAPACITY: usize = (1 << Self::PACKED_NUMBER_BITS) - 1;

    /// This id in [`ExtraId::PACKED_BITS`] bits, or `None` if its entry number does not fit.
    #[inline]
    pub(crate) fn packed(self) -> Option<u64> {
        let number = self.0 & !Self::HAS_ZEROWIDTH;
        (number < 1 << Self::PACKED_NUMBER_BITS).then(|| {
            u64::from(number) | u64::from(self.has_zerowidth()) << Self::PACKED_NUMBER_BITS
        })
    }

    /// The id [`ExtraId::packed`] produced `bits` from; higher bits are ignored.
    #[inline]
    pub(crate) fn from_packed(bits: u64) -> Self {
        let number = (bits & ((1 << Self::PACKED_NUMBER_BITS) - 1)) as u32;
        let zerowidth = bits >> Self::PACKED_NUMBER_BITS & 1 != 0;
        Self(number | if zerowidth { Self::HAS_ZEROWIDTH } else { 0 })
    }
}

/// The extra content of one row's cells, by the [`ExtraId`]s they hold.
#[derive(Clone, Debug, Default)]
pub struct RowExtras {
    entries: Vec<CellExtra>,
}

impl RowExtras {
    /// The extra content `id` refers to, or `None` for [`ExtraId::NONE`].
    ///
    /// # Panics
    ///
    /// Panics if `id` belongs to another row.
    #[inline]
    pub fn get(&self, id: ExtraId) -> Option<&CellExtra> {
        id.position().map(|position| &self.entries[position])
    }

    /// The combining characters `id` refers to, if any.
    #[inline]
    pub fn zerowidth(&self, id: ExtraId) -> Option<&[char]> {
        if id.has_zerowidth() { self.get(id).map(|extra| extra.zerowidth.as_slice()) } else { None }
    }

    /// Add `extra` and return its id; the empty value is not stored and is always `NONE`.
    pub fn push(&mut self, extra: CellExtra) -> ExtraId {
        if extra.is_empty() {
            return ExtraId::NONE;
        }
        let id = ExtraId::new(self.entries.len(), &extra);
        self.entries.push(extra);
        id
    }

    /// The id for the content of `id` with `update` applied; `id`'s entry is left unchanged.
    pub fn update(&mut self, id: ExtraId, update: impl FnOnce(&mut CellExtra)) -> ExtraId {
        let mut extra = self.get(id).cloned().unwrap_or_default();
        update(&mut extra);
        self.push(extra)
    }

    /// Number of entries, live or not.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the row holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop every entry; for a row whose cells were all reset.
    #[inline]
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Keep only the entries `ids` refer to, renumbered, and return each id's replacement.
    ///
    /// `ids` must be every id the row's cells hold, in any order and with repeats allowed.
    pub(crate) fn compact(&mut self, ids: impl Iterator<Item = ExtraId>) -> Vec<ExtraId> {
        let mut used = vec![false; self.entries.len()];
        for position in ids.filter_map(ExtraId::position) {
            used[position] = true;
        }
        let mut remap = vec![ExtraId::NONE; self.entries.len()];
        let mut kept = 0;
        for (position, used) in used.into_iter().enumerate() {
            if used {
                self.entries.swap(kept, position);
                remap[position] = ExtraId::new(kept, &self.entries[kept]);
                kept += 1;
            }
        }
        self.entries.truncate(kept);
        remap
    }

    /// Remap one id returned by [`RowExtras::compact`].
    #[inline]
    pub(crate) fn remapped(remap: &[ExtraId], id: ExtraId) -> ExtraId {
        id.position().map_or(ExtraId::NONE, |position| remap[position])
    }

    /// Move the entry `id` refers to into `gathered` and return its id there.
    ///
    /// The row's entry is left empty; call [`RowExtras::clear`] once every cell is exported.
    pub(crate) fn export(&mut self, id: ExtraId, gathered: &mut Vec<CellExtra>) -> ExtraId {
        let Some(position) = id.position() else { return ExtraId::NONE };
        let extra = mem::take(&mut self.entries[position]);
        let gathered_id = ExtraId::new(gathered.len(), &extra);
        gathered.push(extra);
        gathered_id
    }

    /// Move the entry `id` refers to in `gathered` into this row and return its id here.
    pub(crate) fn import(&mut self, id: ExtraId, gathered: &mut [CellExtra]) -> ExtraId {
        let Some(position) = id.position() else { return ExtraId::NONE };
        self.push(mem::take(&mut gathered[position]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(chars: &[char]) -> CellExtra {
        CellExtra { zerowidth: SmallVec::from_slice(chars), ..CellExtra::default() }
    }

    #[test]
    fn the_empty_value_is_none_and_others_get_their_own_entry() {
        let mut row = RowExtras::default();
        assert_eq!(row.push(CellExtra::default()), ExtraId::NONE);
        assert_eq!(row.get(ExtraId::NONE), None);
        let acute = row.push(marks(&['\u{301}']));
        assert_ne!(acute, ExtraId::NONE);
        assert_eq!(row.zerowidth(acute), Some(['\u{301}'].as_slice()));
        assert_eq!(row.len(), 1);
    }

    #[test]
    fn updating_leaves_the_original_entry_unchanged() {
        let mut row = RowExtras::default();
        let acute = row.push(marks(&['\u{301}']));
        let both = row.update(acute, |extra| extra.zerowidth.push('\u{302}'));
        assert_eq!(row.zerowidth(acute), Some(['\u{301}'].as_slice()));
        assert_eq!(row.zerowidth(both), Some(['\u{301}', '\u{302}'].as_slice()));
        assert_eq!(row.update(both, |extra| extra.zerowidth.clear()), ExtraId::NONE);
    }

    #[test]
    fn long_graphemes_spill_and_the_id_knows_it_has_marks() {
        let mut row = RowExtras::default();
        let long = row.push(marks(&['\u{302}'; 132]));
        assert!(row.get(long).unwrap().zerowidth.spilled());
        assert!(long.has_zerowidth());
        let tracked =
            row.push(CellExtra { vivid_resize_tracking: vec![7], ..CellExtra::default() });
        assert!(!tracked.has_zerowidth(), "tracking alone is not content");
        assert_eq!(row.zerowidth(tracked), None);
    }

    #[test]
    fn compaction_keeps_used_entries_and_renumbers_them() {
        let mut row = RowExtras::default();
        let ids: Vec<_> =
            (0..10).map(|i| row.push(marks(&[char::from_u32(0x300 + i).unwrap()]))).collect();
        let remap = row.compact([ids[2], ids[9], ids[2], ExtraId::NONE].into_iter());
        assert_eq!(row.len(), 2);
        let (two, nine) =
            (RowExtras::remapped(&remap, ids[2]), RowExtras::remapped(&remap, ids[9]));
        assert_eq!(row.zerowidth(two), Some(['\u{302}'].as_slice()));
        assert_eq!(row.zerowidth(nine), Some(['\u{309}'].as_slice()));
        assert_eq!(RowExtras::remapped(&remap, ExtraId::NONE), ExtraId::NONE);
    }

    #[test]
    fn entries_move_between_rows_through_a_gathered_list() {
        let (mut from, mut to) = (RowExtras::default(), RowExtras::default());
        to.push(marks(&['\u{300}']));
        let acute = from.push(marks(&['\u{301}']));
        let mut gathered = Vec::new();
        let moved = from.export(acute, &mut gathered);
        from.clear();
        let landed = to.import(moved, &mut gathered);
        assert_eq!(to.zerowidth(landed), Some(['\u{301}'].as_slice()));
        assert_eq!(to.len(), 2);
        assert!(from.is_empty());
    }
}
