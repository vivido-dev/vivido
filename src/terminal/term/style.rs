//! Cell styles and the table that cells refer to them through.
//!
//! A cell does not hold its colors and attributes directly: it holds a [`StyleId`], and the
//! terminal's [`StyleTable`] turns that id back into a [`Style`]. Readers ask the table, writers
//! intern the style they want, so every cell drawn with the same SGR state shares one entry.
//!
//! The table only grows when output selects a style it has not seen. Styles nothing refers to any
//! more are reclaimed by a sweep: the terminal marks the ids its cells still use and the table
//! compacts around them (see [`StyleTable::needs_sweep`]).

use std::collections::HashMap;
use std::mem;

use crate::terminal::term::cell::{Flags, Hyperlink};
use crate::terminal::vvte::ansi::{Color, NamedColor};

/// Colors, styling attributes and link shared by every cell drawn with the same state.
///
/// Everything but the hyperlink comes from SGR; the hyperlink is OSC 8 state, which SGR resets
/// leave alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Style {
    /// Foreground color.
    pub fg: Color,
    /// Background color.
    pub bg: Color,
    /// Styling attributes; never contains [`Flags::LAYOUT`] bits.
    pub attrs: Flags,
    /// Underline color, when it differs from the foreground.
    pub underline_color: Option<Color>,
    /// OSC 8 hyperlink.
    pub hyperlink: Option<Hyperlink>,
}

impl Default for Style {
    #[inline]
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            attrs: Flags::empty(),
            underline_color: None,
            hyperlink: None,
        }
    }
}

impl Style {
    /// Whether a blank cell drawn in this style looks different from an unstyled one.
    ///
    /// Grid code uses this to tell empty cells from content without the table.
    fn shows_on_blank(&self) -> bool {
        self.bg != Color::Named(NamedColor::Background)
            || self.fg != Color::Named(NamedColor::Foreground)
            || self.attrs.intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
    }

    /// A hashable encoding of this style for interning, given its hyperlink's number.
    fn key(&self, link: u32) -> StyleKey {
        StyleKey {
            fg: color_key(self.fg),
            bg: color_key(self.bg),
            attrs: self.attrs.bits(),
            underline: self.underline_color.map_or(0, color_key),
            link,
        }
    }
}

/// [`Style`]'s identity as plain integers; vvte's `Color` does not implement `Hash`.
///
/// `underline` is zero without an underline color; `link` is zero without a hyperlink and
/// otherwise the table's number for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct StyleKey {
    fg: u32,
    bg: u32,
    attrs: u16,
    underline: u32,
    link: u32,
}

impl StyleKey {
    /// A cheap slot for the lookup cache. Collisions only cost a miss, so this need not resist
    /// crafted input; the table behind it keeps the standard, seeded hasher.
    #[inline]
    fn slot(&self) -> usize {
        let mixed = self.fg.wrapping_mul(0x9E37_79B1)
            ^ self.bg.wrapping_mul(0x85EB_CA77)
            ^ u32::from(self.attrs).wrapping_mul(0xC2B2_AE3D)
            ^ self.underline.wrapping_mul(0x27D4_EB2F)
            ^ self.link.wrapping_mul(0x1656_67B1);
        (mixed >> (32 - CACHE_BITS)) as usize
    }
}

/// Encode a color as a tag in the top byte and its value below.
fn color_key(color: Color) -> u32 {
    match color {
        Color::Named(named) => 0x0100_0000 | named as u32,
        Color::Spec(rgb) => {
            0x0200_0000 | u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b)
        },
        Color::Indexed(index) => 0x0300_0000 | u32::from(index),
    }
}

/// A cell's reference to its [`Style`], resolved through the [`StyleTable`] that issued it.
///
/// The default id is the default style in every table. The top bit caches
/// [`StyleId::shows_on_blank`] so grid code can answer it without the table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StyleId(u32);

impl StyleId {
    /// Bit set when a blank cell in this style is visible.
    const SHOWS_ON_BLANK: u32 = 1 << 31;

    /// Number of distinct entries an id can address.
    const CAPACITY: usize = Self::SHOWS_ON_BLANK as usize;

    fn new(index: usize, style: &Style) -> Self {
        debug_assert!(index < Self::CAPACITY);
        let bit = if style.shows_on_blank() { Self::SHOWS_ON_BLANK } else { 0 };
        Self(index as u32 | bit)
    }

    #[inline]
    fn index(self) -> usize {
        (self.0 & !Self::SHOWS_ON_BLANK) as usize
    }

    /// Whether a blank cell in this style looks different from an unstyled one.
    ///
    /// True for a non-default foreground or background, inverse, underlines and strike-through.
    #[inline]
    pub fn shows_on_blank(self) -> bool {
        self.0 & Self::SHOWS_ON_BLANK != 0
    }
}

/// Entries below which a sweep is never worth its scan of every cell.
///
/// A sweep walks all cells of both screens, scrollback included. Below this size the table costs
/// less memory than one screenful of cells, so reclaiming it would only add scans.
const SWEEP_FLOOR: usize = 4096;

/// Log2 of the number of recent lookups the table remembers.
///
/// Interning runs on every SGR change, so most of them should be answered without the seeded
/// hasher. 4096 slots (64 KiB per terminal) keep about nine in ten hits when output cycles
/// through a thousand 24-bit colors one character at a time; 64 slots kept none, 1024 about half.
const CACHE_BITS: u32 = 12;

/// The styles a terminal's cells refer to, shared by its primary and alternate screens.
#[derive(Clone, Debug)]
pub struct StyleTable {
    styles: Vec<Style>,
    ids: HashMap<StyleKey, StyleId>,
    /// Recent lookups by [`StyleKey::slot`]; every entry is valid for the current numbering.
    cache: Box<[(StyleKey, StyleId); 1 << CACHE_BITS]>,
    /// Numbers for the hyperlinks of the styles held, from one; renumbered by each sweep.
    links: HashMap<Hyperlink, u32>,
    /// The hyperlink numbered last, checked by identity before hashing its URI.
    last_link: Option<(Hyperlink, u32)>,
    /// Entries still in use after the last sweep; the next sweep waits until the table doubles.
    live: usize,
}

/// A cache whose every slot holds the default style, which is valid in every table.
fn empty_cache() -> Box<[(StyleKey, StyleId); 1 << CACHE_BITS]> {
    Box::new([(Style::default().key(0), StyleId::default()); 1 << CACHE_BITS])
}

impl Default for StyleTable {
    fn default() -> Self {
        let style = Style::default();
        Self {
            ids: HashMap::from([(style.key(0), StyleId::default())]),
            styles: vec![style],
            cache: empty_cache(),
            links: HashMap::new(),
            last_link: None,
            live: 1,
        }
    }
}

impl StyleTable {
    /// The style `id` refers to.
    ///
    /// # Panics
    ///
    /// Panics if `id` was issued by another table, or before a sweep that remapped it.
    #[inline]
    pub fn get(&self, id: StyleId) -> &Style {
        &self.styles[id.index()]
    }

    /// This table's number for `link`, assigning the next one if it has none.
    fn link_number(&mut self, link: Option<&Hyperlink>) -> u32 {
        let Some(link) = link else { return 0 };
        if let Some((last, number)) = &self.last_link
            && last.same(link)
        {
            return *number;
        }
        let next = u32::try_from(self.links.len() + 1).unwrap_or(u32::MAX);
        let number = *self.links.entry(link.clone()).or_insert(next);
        self.last_link = Some((link.clone(), number));
        number
    }

    /// An id for `style`, adding it to the table if needed.
    ///
    /// When the table already holds as many styles as an id can address, the default style's id
    /// is returned instead. Sweeps keep the table near the number of styles cells use, so this
    /// needs over two billion distinct styles on screen and in scrollback at once.
    pub fn intern(&mut self, style: Style) -> StyleId {
        debug_assert!(!style.attrs.intersects(Flags::LAYOUT), "{:?} contains layout", style.attrs);
        let link = self.link_number(style.hyperlink.as_ref());
        let key = style.key(link);
        let slot = key.slot();
        let (cached, id) = self.cache[slot];
        if cached == key {
            return id;
        }
        let id = self.lookup_or_insert(key, style);
        self.cache[slot] = (key, id);
        id
    }

    fn lookup_or_insert(&mut self, key: StyleKey, style: Style) -> StyleId {
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        let index = self.styles.len();
        if index >= StyleId::CAPACITY {
            log::warn!("Style table is full; drawing new styles with the default style");
            return StyleId::default();
        }
        let id = StyleId::new(index, &style);
        self.styles.push(style);
        self.ids.insert(key, id);
        id
    }

    /// Number of styles the table holds, including the default.
    pub fn len(&self) -> usize {
        self.styles.len()
    }

    /// Whether the table holds only the default style.
    pub fn is_empty(&self) -> bool {
        self.styles.len() == 1
    }

    /// Whether enough styles were added since the last sweep to make another worthwhile.
    pub fn needs_sweep(&self) -> bool {
        self.styles.len() > SWEEP_FLOOR.max(self.live.saturating_mul(2))
    }

    /// Start a sweep: the returned marks record which ids are still in use.
    pub fn marks(&self) -> StyleMarks {
        let mut used = vec![false; self.styles.len()];
        used[0] = true;
        StyleMarks { used }
    }

    /// Drop every style `marks` did not record and renumber the rest.
    ///
    /// Every id issued before the sweep must then go through the returned remap.
    pub fn compact(&mut self, marks: StyleMarks) -> StyleRemap {
        let mut remap = Vec::with_capacity(self.styles.len());
        let mut styles = Vec::with_capacity(marks.used.iter().filter(|used| **used).count());
        for (style, used) in mem::take(&mut self.styles).into_iter().zip(&marks.used) {
            if *used {
                remap.push(StyleId::new(styles.len(), &style));
                styles.push(style);
            } else {
                remap.push(StyleId::default());
            }
        }
        self.links.clear();
        self.last_link = None;
        self.ids = HashMap::with_capacity(styles.len());
        for (index, style) in styles.iter().enumerate() {
            let link = self.link_number(style.hyperlink.as_ref());
            self.ids.insert(style.key(link), StyleId::new(index, style));
        }
        self.cache = empty_cache();
        self.live = styles.len();
        self.styles = styles;
        StyleRemap { ids: remap }
    }
}

/// Ids found in use during a sweep; see [`StyleTable::marks`].
#[derive(Debug)]
pub struct StyleMarks {
    used: Vec<bool>,
}

impl StyleMarks {
    /// Record that a cell still refers to `id`.
    #[inline]
    pub fn mark(&mut self, id: StyleId) {
        self.used[id.index()] = true;
    }
}

/// Old ids mapped to their numbers after a sweep; see [`StyleTable::compact`].
#[derive(Debug)]
pub struct StyleRemap {
    ids: Vec<StyleId>,
}

impl StyleRemap {
    /// The id `id` became.
    #[inline]
    pub fn get(&self, id: StyleId) -> StyleId {
        self.ids[id.index()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::terminal::vvte::ansi::Rgb;

    fn colored(r: u8) -> Style {
        Style { fg: Color::Spec(Rgb { r, g: 0, b: 0 }), ..Style::default() }
    }

    #[test]
    fn an_interned_style_reads_back_unchanged() {
        let mut table = StyleTable::default();
        let style = Style { fg: Color::Indexed(3), attrs: Flags::BOLD, ..Style::default() };
        let id = table.intern(style.clone());
        assert_eq!(table.get(id), &style);
        assert_eq!(table.get(StyleId::default()), &Style::default());
        assert_eq!(table.intern(Style::default()), StyleId::default());
    }

    #[test]
    fn equal_styles_share_one_entry() {
        let mut table = StyleTable::default();
        let first = table.intern(colored(1));
        assert_eq!(table.intern(colored(1)), first);
        assert_ne!(table.intern(colored(2)), first);
        assert_eq!(table.len(), 3);
    }

    #[test]
    fn colors_with_the_same_value_but_kind_stay_distinct() {
        let mut table = StyleTable::default();
        let named = table.intern(Style { fg: Color::Named(NamedColor::Red), ..Style::default() });
        let indexed = table.intern(Style { fg: Color::Indexed(1), ..Style::default() });
        assert_ne!(named, indexed);
    }

    #[test]
    fn the_id_knows_whether_a_blank_cell_shows_its_style() {
        let mut table = StyleTable::default();
        assert!(!StyleId::default().shows_on_blank());
        assert!(!table.intern(Style { attrs: Flags::BOLD, ..Style::default() }).shows_on_blank());
        assert!(table.intern(Style { attrs: Flags::INVERSE, ..Style::default() }).shows_on_blank());
        assert!(table.intern(Style { bg: Color::Indexed(4), ..Style::default() }).shows_on_blank());
    }

    #[test]
    fn a_sweep_keeps_marked_styles_and_renumbers_them() {
        let mut table = StyleTable::default();
        let ids: Vec<_> = (0..10).map(|r| table.intern(colored(r + 1))).collect();
        let mut marks = table.marks();
        marks.mark(ids[3]);
        marks.mark(ids[7]);
        let remap = table.compact(marks);

        assert_eq!(table.len(), 3, "the default style always survives");
        assert_eq!(table.get(remap.get(ids[3])), &colored(4));
        assert_eq!(table.get(remap.get(ids[7])), &colored(8));
        assert_eq!(remap.get(StyleId::default()), StyleId::default());
        assert_eq!(table.intern(colored(8)), remap.get(ids[7]), "survivors are found again");
        let fresh = table.intern(colored(1));
        assert_eq!(table.get(fresh), &colored(1), "a dropped style can be added again");
    }

    #[test]
    fn sweeps_wait_until_the_table_doubles() {
        let mut table = StyleTable::default();
        for r in 0..SWEEP_FLOOR {
            table.intern(Style {
                fg: Color::Indexed((r % 256) as u8),
                bg: Color::Indexed((r / 256) as u8),
                ..Style::default()
            });
        }
        assert!(table.needs_sweep());
        let marks = table.marks();
        table.compact(marks);
        assert!(!table.needs_sweep());
    }
}
