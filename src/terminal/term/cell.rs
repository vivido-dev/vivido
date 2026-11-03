use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::terminal::grid::{self, GridCell};
use crate::terminal::index::Column;
use crate::terminal::term::extra::{ExtraId, RowExtras};
use crate::terminal::term::style::{StyleId, StyleTable};
use crate::terminal::vvte::ansi::{Color, Hyperlink as VteHyperlink};

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

impl Flags {
    /// Flags that describe where a cell sits in the grid rather than how it looks.
    ///
    /// They stay with the cell when its style is shared, so grid code (wrapping, reflow, wide
    /// characters) can read them without the style.
    pub const LAYOUT: Self = Self::WRAPLINE
        .union(Self::WIDE_CHAR)
        .union(Self::WIDE_CHAR_SPACER)
        .union(Self::LEADING_WIDE_CHAR_SPACER);
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

    /// Whether `other` is this very hyperlink, not just an equal one; never hashes the URI.
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
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

impl ResetDiscriminant<StyleId> for Cell {
    fn discriminant(&self) -> StyleId {
        self.style()
    }
}

/// Content and attributes of a single cell in the terminal grid, packed into eight bytes.
///
/// Colors and styling attributes live in the terminal's [`StyleTable`] and combining characters in
/// the row's [`RowExtras`]; the cell holds an id for each. [`SerializedCell`] is its
/// self-contained form.
///
/// ```text
/// bits  0..21  character
/// bits 21..25  layout: WRAPLINE, WIDE_CHAR, WIDE_CHAR_SPACER, LEADING_WIDE_CHAR_SPACER
/// bits 25..50  style id (index and blank bit, `StyleId::packed`)
/// bits 50..64  extra id (row entry number and combining bit, `ExtraId::packed`)
/// ```
///
/// Eight bytes halve the memory a scroll clears and a print writes compared with an unpacked
/// cell, which is most of the time spent on plain text.
#[derive(Clone, PartialEq, Eq)]
pub struct Cell(u64);

// The packed fields fill the eight bytes exactly.
const _: () = assert!(Cell::EXTRA_SHIFT + ExtraId::PACKED_BITS == u64::BITS);

/// Layout flags in the order of their packed bits.
const LAYOUT_ORDER: [Flags; 4] =
    [Flags::WRAPLINE, Flags::WIDE_CHAR, Flags::WIDE_CHAR_SPACER, Flags::LEADING_WIDE_CHAR_SPACER];

impl Cell {
    const CHAR_BITS: u32 = 21;
    const LAYOUT_SHIFT: u32 = Self::CHAR_BITS;
    const STYLE_SHIFT: u32 = Self::LAYOUT_SHIFT + LAYOUT_ORDER.len() as u32;
    const EXTRA_SHIFT: u32 = Self::STYLE_SHIFT + StyleId::PACKED_BITS;
    const CHAR_MASK: u64 = (1 << Self::CHAR_BITS) - 1;
    const LAYOUT_MASK: u64 = ((1 << LAYOUT_ORDER.len()) - 1) << Self::LAYOUT_SHIFT;
    const STYLE_MASK: u64 = ((1 << StyleId::PACKED_BITS) - 1) << Self::STYLE_SHIFT;
    const EXTRA_MASK: u64 = !0 << Self::EXTRA_SHIFT;

    /// Layout flags as packed bits, in place.
    #[inline]
    fn layout_bits(flags: Flags) -> u64 {
        debug_assert!(Flags::LAYOUT.contains(flags), "{flags:?} is not layout");
        let mut bits = 0;
        for (bit, flag) in LAYOUT_ORDER.iter().enumerate() {
            if flags.contains(*flag) {
                bits |= 1 << (Self::LAYOUT_SHIFT + bit as u32);
            }
        }
        bits
    }

    /// Character stored in this cell.
    #[inline]
    pub fn c(&self) -> char {
        // Only `char`s are ever stored, so the fallback is unreachable.
        char::from_u32((self.0 & Self::CHAR_MASK) as u32).unwrap_or(' ')
    }

    /// Replace the character stored in this cell.
    #[inline]
    pub fn set_c(&mut self, c: char) {
        self.0 = self.0 & !Self::CHAR_MASK | u64::from(c);
    }

    /// Reference to this cell's colors and styling attributes.
    ///
    /// Resolve it with the terminal's [`StyleTable`].
    #[inline]
    pub fn style(&self) -> StyleId {
        StyleId::from_packed(self.0 >> Self::STYLE_SHIFT)
    }

    /// Replace this cell's style reference.
    #[inline]
    pub fn set_style(&mut self, style: StyleId) {
        self.0 = self.0 & !Self::STYLE_MASK | style.packed() << Self::STYLE_SHIFT;
    }

    /// Layout flags: line wrap and double-width occupancy.
    ///
    /// Never contains styling, which lives in the cell's style.
    #[inline]
    pub fn layout(&self) -> Flags {
        let mut flags = Flags::empty();
        for (bit, flag) in LAYOUT_ORDER.iter().enumerate() {
            if self.0 & 1 << (Self::LAYOUT_SHIFT + bit as u32) != 0 {
                flags |= *flag;
            }
        }
        flags
    }

    /// Replace the layout flags.
    #[inline]
    pub fn set_layout(&mut self, flags: Flags) {
        self.0 = self.0 & !Self::LAYOUT_MASK | Self::layout_bits(flags);
    }

    /// Set the given layout flags.
    #[inline]
    pub fn insert_layout(&mut self, flags: Flags) {
        self.0 |= Self::layout_bits(flags);
    }

    /// Clear the given layout flags.
    #[inline]
    pub fn remove_layout(&mut self, flags: Flags) {
        self.0 &= !Self::layout_bits(flags);
    }

    /// Whether any of the given layout flags is set; one mask test.
    #[inline]
    pub fn has_layout(&self, flags: Flags) -> bool {
        self.0 & Self::layout_bits(flags) != 0
    }

    /// Whether a print may overwrite this cell outright: no extra content to drop and not part of
    /// a double-width character to clean up. One mask test.
    #[inline]
    pub(crate) fn is_simple(&self) -> bool {
        const WIDE: Flags =
            Flags::WIDE_CHAR.union(Flags::WIDE_CHAR_SPACER).union(Flags::LEADING_WIDE_CHAR_SPACER);
        self.0 & (Self::EXTRA_MASK | Self::layout_bits(WIDE)) == 0
    }

    /// Whether this cell holds combining characters or resize tracking.
    #[inline]
    pub fn has_extra(&self) -> bool {
        self.0 & Self::EXTRA_MASK != 0
    }

    /// Reference to this cell's combining characters and resize tracking.
    ///
    /// Resolve it with the [`RowExtras`] of the row holding the cell.
    #[inline]
    pub fn extra(&self) -> ExtraId {
        ExtraId::from_packed(self.0 >> Self::EXTRA_SHIFT)
    }

    /// Point this cell at another entry of its row's extra content.
    ///
    /// An entry number a packed cell cannot hold drops the content instead; rows compact well
    /// before that, so it takes a row over 8,000 columns wide full of combining characters.
    #[inline]
    fn set_extra_id(&mut self, extra: ExtraId) {
        let bits = extra.packed().unwrap_or_else(|| {
            log::warn!("Row has too many combining-character entries; dropping one");
            0
        });
        self.0 = self.0 & !Self::EXTRA_MASK | bits << Self::EXTRA_SHIFT;
    }

    /// Replace the whole cell with `c` in `style` and `layout`, and no extra content.
    #[inline]
    pub(crate) fn write(&mut self, c: char, style: StyleId, layout: Flags) {
        self.0 = u64::from(c) | Self::layout_bits(layout) | style.packed() << Self::STYLE_SHIFT;
    }

    /// Remove all wide char data from a cell.
    ///
    /// Resize tracking goes too; it only exists while a resize is in progress, never during writes.
    #[inline(never)]
    pub fn clear_wide(&mut self) {
        self.remove_layout(Flags::WIDE_CHAR);
        self.0 &= !Self::EXTRA_MASK;
        self.set_c(' ');
    }
}

impl std::fmt::Debug for Cell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cell")
            .field("c", &self.c())
            .field("style", &self.style())
            .field("layout", &self.layout())
            .field("extra", &self.extra())
            .finish()
    }
}

/// A cell with its style written out, independent of any style table.
///
/// This is the form reference fixtures and diagnostic grid dumps store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedCell {
    c: char,
    fg: Color,
    bg: Color,
    flags: Flags,
    extra: Option<SerializedExtra>,
}

/// The rarely set parts of a [`SerializedCell`], present when any of them is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SerializedExtra {
    zerowidth: SmallVec<[char; 4]>,
    underline_color: Option<Color>,
    hyperlink: Option<Hyperlink>,
}

impl SerializedCell {
    /// Write out `cell`, whose style `styles` holds and whose extra content its row's `extras`.
    pub fn new(cell: &Cell, styles: &StyleTable, extras: &RowExtras) -> Self {
        let style = styles.get(cell.style());
        let flags = cell.layout().union(style.attrs);
        let extra =
            (cell.has_extra() || style.underline_color.is_some() || style.hyperlink.is_some())
                .then(|| SerializedExtra {
                    zerowidth: extras
                        .get(cell.extra())
                        .map(|extra| extra.zerowidth.clone())
                        .unwrap_or_default(),
                    underline_color: style.underline_color,
                    hyperlink: style.hyperlink.clone(),
                });
        Self { c: cell.c(), fg: style.fg, bg: style.bg, flags, extra }
    }
}

impl Default for SerializedCell {
    fn default() -> Self {
        Self::new(&Cell::default(), &StyleTable::default(), &RowExtras::default())
    }
}

impl Default for Cell {
    #[inline]
    fn default() -> Cell {
        Cell(u64::from(' '))
    }
}

impl GridCell for Cell {
    #[inline]
    fn is_empty(&self) -> bool {
        let c = self.c();
        (c == ' ' || c == '\t')
            && !self.style().shows_on_blank()
            && !self.has_layout(
                Flags::WRAPLINE | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
            )
            && !self.has_extra()
    }

    #[inline]
    fn layout(&self) -> Flags {
        Cell::layout(self)
    }

    #[inline]
    fn insert_layout(&mut self, flags: Flags) {
        Cell::insert_layout(self, flags);
    }

    #[inline]
    fn remove_layout(&mut self, flags: Flags) {
        Cell::remove_layout(self, flags);
    }

    #[inline]
    fn extra(&self) -> ExtraId {
        Cell::extra(self)
    }

    #[inline]
    fn set_extra(&mut self, extra: ExtraId) {
        self.set_extra_id(extra);
    }

    #[inline]
    fn reset(&mut self, template: &Self) {
        // A blank cell keeps only the template's style, so this is one store per cell.
        self.0 = u64::from(' ') | template.0 & Self::STYLE_MASK;
    }
}

impl From<char> for Cell {
    /// A cell holding `c` with default colors and no flags.
    #[inline]
    fn from(c: char) -> Self {
        Cell(u64::from(c))
    }
}

/// A [`Cell`] with each part in its own field, for resize.
///
/// Resize gathers every row's combining characters into one list while it moves cells between
/// rows, and that list can hold more entries than a packed cell can number. Grids are converted to
/// this form for the resize and back afterwards, when each row again holds only its own entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnpackedCell {
    c: char,
    style: StyleId,
    extra: ExtraId,
    layout: Flags,
}

impl Default for UnpackedCell {
    fn default() -> Self {
        Self::from(&Cell::default())
    }
}

impl From<&Cell> for UnpackedCell {
    fn from(cell: &Cell) -> Self {
        Self { c: cell.c(), style: cell.style(), extra: cell.extra(), layout: cell.layout() }
    }
}

impl From<&UnpackedCell> for Cell {
    fn from(cell: &UnpackedCell) -> Self {
        let mut packed = Cell::default();
        packed.write(cell.c, cell.style, cell.layout);
        packed.set_extra_id(cell.extra);
        packed
    }
}

impl ResetDiscriminant<StyleId> for UnpackedCell {
    fn discriminant(&self) -> StyleId {
        self.style
    }
}

impl GridCell for UnpackedCell {
    fn is_empty(&self) -> bool {
        (self.c == ' ' || self.c == '\t')
            && !self.style.shows_on_blank()
            && !self.layout.intersects(
                Flags::WRAPLINE | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
            )
            && self.extra == ExtraId::NONE
    }

    fn layout(&self) -> Flags {
        self.layout
    }

    fn insert_layout(&mut self, flags: Flags) {
        self.layout.insert(flags);
    }

    fn remove_layout(&mut self, flags: Flags) {
        self.layout.remove(flags);
    }

    fn extra(&self) -> ExtraId {
        self.extra
    }

    fn set_extra(&mut self, extra: ExtraId) {
        self.extra = extra;
    }

    fn reset(&mut self, template: &Self) {
        *self = Self { c: ' ', style: template.style, ..Self::default() };
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

        if self[Column(self.len() - 1)].has_layout(Flags::WRAPLINE) {
            return Column(self.len());
        }

        for (index, cell) in self[..].iter().rev().enumerate() {
            if cell.c() != ' ' || cell.extra().has_zerowidth() {
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

    use crate::terminal::term::style::Style;

    use std::mem;

    use crate::terminal::grid::Row;

    #[test]
    fn cell_size_is_below_cap() {
        // Character, layout flags, style id and extra-content id, packed.
        const EXPECTED_CELL_SIZE: usize = 8;

        // Ensure that cell size isn't growing by accident.
        assert!(mem::size_of::<Cell>() <= EXPECTED_CELL_SIZE);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn inline_suffixes_preserve_the_previous_storage_layout_size() {
        assert_eq!(mem::size_of::<SmallVec<[char; 4]>>(), mem::size_of::<Vec<char>>());
    }

    #[test]
    fn every_field_packs_and_unpacks_independently() {
        let mut styles = crate::terminal::term::style::StyleTable::default();
        let inverse = styles.intern(Style { attrs: Flags::INVERSE, ..Style::default() });
        let mut extras = RowExtras::default();
        let marks = extras.update(ExtraId::NONE, |extra| extra.zerowidth.push('\u{301}'));

        for c in ['\0', ' ', 'x', '界', '\u{10FFFF}'] {
            for layout in [Flags::empty(), Flags::LAYOUT, Flags::WIDE_CHAR_SPACER] {
                for style in [StyleId::default(), inverse] {
                    let mut cell = Cell::default();
                    cell.write(c, style, layout);
                    GridCell::set_extra(&mut cell, marks);
                    assert_eq!(
                        (cell.c(), cell.layout(), cell.style(), cell.extra()),
                        (c, layout, style, marks)
                    );
                    assert!(cell.style().shows_on_blank() == (style == inverse));
                    assert_eq!(Cell::from(&UnpackedCell::from(&cell)), cell);

                    cell.set_c('y');
                    cell.set_style(StyleId::default());
                    assert_eq!((cell.layout(), cell.extra()), (layout, marks), "{c:?}");
                    cell.set_layout(Flags::WRAPLINE);
                    assert_eq!((cell.c(), cell.extra()), ('y', marks));
                }
            }
        }
        assert_eq!(std::mem::size_of::<Cell>(), 8);
    }

    #[test]
    fn layout_and_style_change_independently() {
        let mut table = crate::terminal::term::style::StyleTable::default();
        let bold = table.intern(Style { attrs: Flags::BOLD, ..Style::default() });
        let mut cell = Cell::default();
        cell.set_style(bold);
        cell.insert_layout(Flags::WIDE_CHAR);
        assert_eq!(cell.layout(), Flags::WIDE_CHAR);
        assert_eq!(table.get(cell.style()).attrs, Flags::BOLD);

        cell.set_style(StyleId::default());
        assert_eq!(cell.layout(), Flags::WIDE_CHAR, "replacing the style keeps layout");
        cell.set_layout(Flags::WRAPLINE);
        assert_eq!(cell.style(), StyleId::default(), "replacing layout keeps the style");
    }

    #[test]
    fn serialization_keeps_the_flat_cell_form() {
        let mut table = crate::terminal::term::style::StyleTable::default();
        let mut cell = Cell::from('x');
        cell.set_style(table.intern(Style {
            fg: Color::Indexed(1),
            bg: Color::Indexed(2),
            attrs: Flags::BOLD | Flags::UNDERLINE,
            ..Style::default()
        }));
        cell.set_layout(Flags::WIDE_CHAR);
        let serialized = SerializedCell::new(&cell, &table, &RowExtras::default());
        let value = serde_json::to_value(&serialized).unwrap();
        assert_eq!(value["flags"], "BOLD | UNDERLINE | WIDE_CHAR");
        assert_eq!(value["fg"], serde_json::json!({"Indexed": 1}));
        assert_eq!(serde_json::from_value::<SerializedCell>(value).unwrap(), serialized);
    }

    #[test]
    fn line_length_works() {
        let mut row = Row::<Cell>::new(10);
        row[Column(5)].set_c('a');

        assert_eq!(row.line_length(), Column(6));
    }

    #[test]
    fn line_length_works_with_wrapline() {
        let mut row = Row::<Cell>::new(10);
        row[Column(9)].insert_layout(super::Flags::WRAPLINE);

        assert_eq!(row.line_length(), Column(10));
    }
}
