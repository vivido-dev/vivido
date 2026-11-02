use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::terminal::grid::{self, GridCell};
use crate::terminal::index::Column;
use crate::terminal::vvte::ansi::{Color, Hyperlink as VteHyperlink, NamedColor};

bitflags! {
    /// Styling and grid-occupancy flags attached to a terminal cell.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[derive(Serialize, Deserialize)]
    pub struct Flags: u16 {
        /// Exchange cell foreground and background.
        const INVERSE                   = 0b0000_0000_0000_0001;
        /// Render with bold weight.
        const BOLD                      = 0b0000_0000_0000_0010;
        /// Render with italic styling.
        const ITALIC                    = 0b0000_0000_0000_0100;
        /// Combine bold and italic styling.
        const BOLD_ITALIC               = 0b0000_0000_0000_0110;
        /// Draw a single underline.
        const UNDERLINE                 = 0b0000_0000_0000_1000;
        /// The following row continues this wrapped line.
        const WRAPLINE                  = 0b0000_0000_0001_0000;
        /// The cell starts a double-width character.
        const WIDE_CHAR                 = 0b0000_0000_0010_0000;
        /// The cell is the trailing half of a double-width character.
        const WIDE_CHAR_SPACER          = 0b0000_0000_0100_0000;
        /// Render with reduced intensity.
        const DIM                       = 0b0000_0000_1000_0000;
        /// Combine reduced intensity with bold weight.
        const DIM_BOLD                  = 0b0000_0000_1000_0010;
        /// Suppress visible glyph rendering.
        const HIDDEN                    = 0b0000_0001_0000_0000;
        /// Draw a strike-through decoration.
        const STRIKEOUT                 = 0b0000_0010_0000_0000;
        /// Reserve the final column before a wrapped double-width character.
        const LEADING_WIDE_CHAR_SPACER  = 0b0000_0100_0000_0000;
        /// Draw a double underline.
        const DOUBLE_UNDERLINE          = 0b0000_1000_0000_0000;
        /// Draw a wavy underline.
        const UNDERCURL                 = 0b0001_0000_0000_0000;
        /// Draw a dotted underline.
        const DOTTED_UNDERLINE          = 0b0010_0000_0000_0000;
        /// Draw a dashed underline.
        const DASHED_UNDERLINE          = 0b0100_0000_0000_0000;
        /// All supported underline styles.
        const ALL_UNDERLINES            = Self::UNDERLINE.bits() | Self::DOUBLE_UNDERLINE.bits()
                                        | Self::UNDERCURL.bits() | Self::DOTTED_UNDERLINE.bits()
                                        | Self::DASHED_UNDERLINE.bits();
    }
}

/// Counter for hyperlinks without explicit ID.
static HYPERLINK_ID_SUFFIX: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
/// An OSC 8 hyperlink identifier and destination URI.
pub struct Hyperlink {
    inner: Arc<HyperlinkInner>,
}

impl Hyperlink {
    /// Create an OSC 8 hyperlink, generating an identifier when absent.
    pub fn new<T: ToString>(id: Option<T>, uri: String) -> Self {
        let inner = Arc::new(HyperlinkInner::new(id, uri));
        Self { inner }
    }

    /// Return the identifier assigned to this object.
    pub fn id(&self) -> &str {
        &self.inner.id
    }

    /// Borrow the OSC 8 hyperlink destination URI.
    pub fn uri(&self) -> &str {
        &self.inner.uri
    }
}

impl From<VteHyperlink> for Hyperlink {
    fn from(value: VteHyperlink) -> Self {
        Self::new(value.id, value.uri)
    }
}

impl From<Hyperlink> for VteHyperlink {
    fn from(val: Hyperlink) -> Self {
        VteHyperlink { id: Some(val.id().to_owned()), uri: val.uri().to_owned() }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct HyperlinkInner {
    /// Identifier for the given hyperlink.
    id: String,

    /// Resource identifier of the hyperlink.
    uri: String,
}

impl HyperlinkInner {
    pub fn new<T: ToString>(id: Option<T>, uri: String) -> Self {
        let id = match id {
            Some(id) => id.to_string(),
            None => {
                let mut id = HYPERLINK_ID_SUFFIX.fetch_add(1, Ordering::Relaxed).to_string();
                id.push_str("_vivido");
                id
            },
        };

        Self { id, uri }
    }
}

/// Trait for determining if a reset should be performed.
pub trait ResetDiscriminant<T> {
    /// Value based on which equality for the reset will be determined.
    fn discriminant(&self) -> T;
}

impl<T: Copy> ResetDiscriminant<T> for T {
    fn discriminant(&self) -> T {
        *self
    }
}

impl ResetDiscriminant<Color> for Cell {
    fn discriminant(&self) -> Color {
        self.bg
    }
}

/// Dynamically allocated cell content.
///
/// This storage is reserved for cell attributes which are rarely set. This allows reducing the
/// allocation required ahead of time for every cell, with some additional overhead when the extra
/// storage is actually required.
#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CellExtra {
    // Four suffixes fit without a second allocation; longer graphemes spill without truncation.
    zerowidth: SmallVec<[char; 4]>,
    underline_color: Option<Color>,
    hyperlink: Option<Hyperlink>,
    /// Temporary indices used to carry authenticated Vivid anchors through grid resize/reflow.
    ///
    /// These never survive a resize operation and are not part of serialized terminal state.
    #[serde(skip)]
    vivid_resize_tracking: Vec<usize>,
}

/// Content and attributes of a single cell in the terminal grid.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    /// Character stored in this terminal cell.
    c: char,
    /// Foreground color.
    fg: Color,
    /// Background color.
    bg: Color,
    /// Terminal cell or mode flags.
    flags: Flags,
    /// Optional extended cell attributes.
    extra: Option<Arc<CellExtra>>,
}

impl Default for Cell {
    #[inline]
    fn default() -> Cell {
        Cell {
            c: ' ',
            bg: Color::Named(NamedColor::Background),
            fg: Color::Named(NamedColor::Foreground),
            flags: Flags::empty(),
            extra: None,
        }
    }
}

impl Cell {
    /// Character stored in this cell.
    #[inline]
    pub fn c(&self) -> char {
        self.c
    }

    /// Replace the character stored in this cell.
    #[inline]
    pub fn set_c(&mut self, c: char) {
        self.c = c;
    }

    /// Foreground color.
    #[inline]
    pub fn fg(&self) -> Color {
        self.fg
    }

    /// Replace the foreground color.
    #[inline]
    pub fn set_fg(&mut self, color: Color) {
        self.fg = color;
    }

    /// Background color.
    #[inline]
    pub fn bg(&self) -> Color {
        self.bg
    }

    /// Replace the background color.
    #[inline]
    pub fn set_bg(&mut self, color: Color) {
        self.bg = color;
    }

    /// Styling and layout flags.
    #[inline]
    pub fn flags(&self) -> Flags {
        self.flags
    }

    /// Replace the styling and layout flags.
    #[inline]
    pub fn set_flags(&mut self, flags: Flags) {
        self.flags = flags;
    }

    /// Mutably borrow the styling and layout flags.
    #[inline]
    pub fn flags_mut(&mut self) -> &mut Flags {
        &mut self.flags
    }

    /// Whether this cell holds extended attributes.
    #[inline]
    pub fn has_extra(&self) -> bool {
        self.extra.is_some()
    }

    /// Borrow the shared extended attributes, if any.
    #[inline]
    pub(crate) fn extra(&self) -> Option<&Arc<CellExtra>> {
        self.extra.as_ref()
    }

    /// Replace the shared extended attributes.
    #[inline]
    pub(crate) fn set_extra(&mut self, extra: Option<Arc<CellExtra>>) {
        self.extra = extra;
    }

    /// Zerowidth characters stored in this cell.
    #[inline]
    pub fn zerowidth(&self) -> Option<&[char]> {
        self.extra.as_ref().map(|extra| extra.zerowidth.as_slice())
    }

    /// Write a new zerowidth character to this cell.
    #[inline]
    pub fn push_zerowidth(&mut self, character: char) {
        let extra = self.extra.get_or_insert_with(Default::default);
        Arc::make_mut(extra).zerowidth.push(character);
    }

    /// Append a combining run with one copy-on-write ownership check.
    pub(crate) fn push_zerowidths(&mut self, characters: impl Iterator<Item = char>) {
        let extra = self.extra.get_or_insert_with(Default::default);
        Arc::make_mut(extra).zerowidth.extend(characters);
    }

    /// Attach a temporary Vivid anchor index before terminal resize/reflow.
    pub(crate) fn push_vivid_resize_tracking(&mut self, index: usize) {
        let extra = self.extra.get_or_insert_with(Default::default);
        Arc::make_mut(extra).vivid_resize_tracking.push(index);
    }

    /// Remove and return temporary Vivid anchor indices after terminal resize/reflow.
    pub(crate) fn take_vivid_resize_tracking(&mut self) -> Vec<usize> {
        let Some(extra) = self.extra.as_mut() else {
            return Vec::new();
        };
        let extra = Arc::make_mut(extra);
        let tracking = std::mem::take(&mut extra.vivid_resize_tracking);
        if extra.zerowidth.is_empty()
            && extra.underline_color.is_none()
            && extra.hyperlink.is_none()
        {
            self.extra = None;
        }
        tracking
    }

    /// Remove all wide char data from a cell.
    #[inline(never)]
    pub fn clear_wide(&mut self) {
        self.flags.remove(Flags::WIDE_CHAR);
        if let Some(extra) = self.extra.as_mut() {
            Arc::make_mut(extra).zerowidth = SmallVec::new();
        }
        self.c = ' ';
    }

    /// Set underline color on the cell.
    pub fn set_underline_color(&mut self, color: Option<Color>) {
        // If we reset color and we don't have zerowidth we should drop extra storage.
        if color.is_none()
            && self.extra.as_ref().is_none_or(|extra| {
                extra.zerowidth.is_empty()
                    && extra.hyperlink.is_none()
                    && extra.vivid_resize_tracking.is_empty()
            })
        {
            self.extra = None;
        } else {
            let extra = self.extra.get_or_insert_with(Default::default);
            Arc::make_mut(extra).underline_color = color;
        }
    }

    /// Underline color stored in this cell.
    #[inline]
    pub fn underline_color(&self) -> Option<Color> {
        self.extra.as_ref()?.underline_color
    }

    /// Set hyperlink.
    pub fn set_hyperlink(&mut self, hyperlink: Option<Hyperlink>) {
        let should_drop = hyperlink.is_none()
            && self.extra.as_ref().is_none_or(|extra| {
                extra.zerowidth.is_empty()
                    && extra.underline_color.is_none()
                    && extra.vivid_resize_tracking.is_empty()
            });

        if should_drop {
            self.extra = None;
        } else {
            let extra = self.extra.get_or_insert_with(Default::default);
            Arc::make_mut(extra).hyperlink = hyperlink;
        }
    }

    /// Hyperlink stored in this cell.
    #[inline]
    pub fn hyperlink(&self) -> Option<Hyperlink> {
        self.extra.as_ref()?.hyperlink.clone()
    }
}

impl GridCell for Cell {
    #[inline]
    fn is_empty(&self) -> bool {
        (self.c == ' ' || self.c == '\t')
            && self.bg == Color::Named(NamedColor::Background)
            && self.fg == Color::Named(NamedColor::Foreground)
            && !self.flags.intersects(
                Flags::INVERSE
                    | Flags::ALL_UNDERLINES
                    | Flags::STRIKEOUT
                    | Flags::WRAPLINE
                    | Flags::WIDE_CHAR_SPACER
                    | Flags::LEADING_WIDE_CHAR_SPACER,
            )
            && self.extra.as_ref().map(|extra| extra.zerowidth.is_empty()) != Some(false)
            && self.extra.as_ref().is_none_or(|extra| extra.vivid_resize_tracking.is_empty())
    }

    #[inline]
    fn flags(&self) -> &Flags {
        &self.flags
    }

    #[inline]
    fn flags_mut(&mut self) -> &mut Flags {
        &mut self.flags
    }

    #[inline]
    fn reset(&mut self, template: &Self) {
        // `extra` is the only field that owns an allocation, and it is unset for all but a handful
        // of cells. Releasing it under its own branch leaves the rest of the reset a straight-line
        // store, so clearing a scrolled-in row does not run drop glue over every cell.
        if self.extra.is_some() {
            self.extra = None;
        }
        self.c = ' ';
        self.fg = Color::Named(NamedColor::Foreground);
        self.bg = template.bg;
        self.flags = Flags::empty();
    }
}

impl From<char> for Cell {
    /// A cell holding `c` with default colors and no flags.
    #[inline]
    fn from(c: char) -> Self {
        Self { c, ..Cell::default() }
    }
}

impl From<Color> for Cell {
    #[inline]
    fn from(color: Color) -> Self {
        Self { bg: color, ..Cell::default() }
    }
}

/// Get the length of occupied cells in a line.
pub trait LineLength {
    /// Calculate the occupied line length.
    fn line_length(&self) -> Column;
}

impl LineLength for grid::Row<Cell> {
    fn line_length(&self) -> Column {
        let mut length = Column(0);

        if self[Column(self.len() - 1)].flags.contains(Flags::WRAPLINE) {
            return Column(self.len());
        }

        for (index, cell) in self[..].iter().rev().enumerate() {
            if cell.c != ' '
                || cell.extra.as_ref().map(|extra| extra.zerowidth.is_empty()) == Some(false)
            {
                length = Column(self.len() - index);
                break;
            }
        }

        length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::mem;

    use crate::terminal::grid::Row;

    #[test]
    fn cell_size_is_below_cap() {
        // Expected cell size on 64-bit architectures.
        const EXPECTED_CELL_SIZE: usize = 24;

        // Ensure that cell size isn't growing by accident.
        assert!(mem::size_of::<Cell>() <= EXPECTED_CELL_SIZE);
    }

    #[test]
    fn extra_updates_preserve_shared_cell_contents() {
        let mut cell = Cell::default();
        cell.push_zerowidth('\u{301}');
        let original = cell.clone();
        cell.push_zerowidth('\u{302}');
        cell.set_underline_color(Some(Color::Indexed(3)));
        cell.push_vivid_resize_tracking(7);

        assert_eq!(original.zerowidth(), Some(['\u{301}'].as_slice()));
        assert_eq!(original.underline_color(), None);
        assert!(original.extra.as_ref().unwrap().vivid_resize_tracking.is_empty());
        assert_eq!(cell.zerowidth(), Some(['\u{301}', '\u{302}'].as_slice()));
        assert_eq!(cell.underline_color(), Some(Color::Indexed(3)));
        assert_eq!(cell.take_vivid_resize_tracking(), [7]);
        cell.set_underline_color(None);
        cell.set_hyperlink(None);
        assert_eq!(cell.zerowidth(), Some(['\u{301}', '\u{302}'].as_slice()));
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn inline_suffixes_preserve_the_previous_storage_layout_size() {
        assert_eq!(mem::size_of::<SmallVec<[char; 4]>>(), mem::size_of::<Vec<char>>());
    }

    #[test]
    fn combining_storage_spills_and_preserves_serialization_and_cow() {
        let mut cell = Cell::default();
        cell.push_zerowidths(['\u{301}'; 4].into_iter());
        assert!(!cell.extra.as_ref().unwrap().zerowidth.spilled());
        let original = cell.clone();
        cell.push_zerowidths(std::iter::repeat_n('\u{302}', 128));
        assert!(cell.extra.as_ref().unwrap().zerowidth.spilled());
        assert_eq!(original.zerowidth().unwrap(), ['\u{301}'; 4]);
        assert_eq!(cell.zerowidth().unwrap().len(), 132);
        let value = serde_json::to_value(&cell).unwrap();
        let suffix: Vec<char> =
            serde_json::from_value(value["extra"]["zerowidth"].clone()).unwrap();
        assert_eq!(suffix, cell.zerowidth().unwrap());
        assert_eq!(serde_json::from_value::<Cell>(value).unwrap(), cell);
        cell.clear_wide();
        assert!(cell.zerowidth().unwrap().is_empty());
        assert_eq!(original.zerowidth().unwrap().len(), 4);
    }

    #[test]
    fn line_length_works() {
        let mut row = Row::<Cell>::new(10);
        row[Column(5)].c = 'a';

        assert_eq!(row.line_length(), Column(6));
    }

    #[test]
    fn line_length_works_with_wrapline() {
        let mut row = Row::<Cell>::new(10);
        row[Column(9)].flags.insert(super::Flags::WRAPLINE);

        assert_eq!(row.line_length(), Column(10));
    }
}
