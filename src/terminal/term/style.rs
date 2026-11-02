//! Cell styles and the table that cells refer to them through.
//!
//! A cell does not hold its colors and attributes directly: it holds a [`StyleId`], and the
//! terminal's [`StyleTable`] turns that id back into a [`Style`]. Readers ask the table, writers
//! intern the style they want. Today an id carries its style inline and the table holds nothing;
//! the interface is what lets the id shrink to an index without touching its callers.

use crate::terminal::term::cell::Flags;
use crate::terminal::vvte::ansi::{Color, NamedColor};

/// Colors and styling attributes shared by every cell drawn with the same SGR state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    /// Foreground color.
    pub fg: Color,
    /// Background color.
    pub bg: Color,
    /// Styling attributes; never contains [`Flags::LAYOUT`] bits.
    pub attrs: Flags,
}

impl Default for Style {
    #[inline]
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            attrs: Flags::empty(),
        }
    }
}

/// A cell's reference to its [`Style`], resolved through a [`StyleTable`].
///
/// The default id is the default style in every table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StyleId(Style);

impl StyleId {
    /// The style this id stands for, without a table.
    ///
    /// Only the cell module uses this, for grid operations that run without the terminal: emptiness
    /// checks and row resets. They are the places a table-backed id has to answer differently.
    #[inline]
    pub(super) fn inline(self) -> Style {
        self.0
    }

    /// An id for `style` that needs no table, for the same cell-module operations as
    /// [`StyleId::inline`].
    #[inline]
    pub(super) fn from_inline(style: Style) -> Self {
        Self(style)
    }
}

/// The styles a terminal's cells refer to, shared by its primary and alternate screens.
#[derive(Clone, Debug, Default)]
pub struct StyleTable {
    _private: (),
}

impl StyleTable {
    /// The style `id` refers to.
    #[inline]
    pub fn get(&self, id: StyleId) -> Style {
        id.0
    }

    /// An id for `style`, adding it to the table if needed.
    #[inline]
    pub fn intern(&mut self, style: Style) -> StyleId {
        debug_assert!(!style.attrs.intersects(Flags::LAYOUT), "{:?} contains layout", style.attrs);
        StyleId(style)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interned_style_reads_back_unchanged() {
        let mut table = StyleTable::default();
        let style = Style { fg: Color::Indexed(3), attrs: Flags::BOLD, ..Style::default() };
        let id = table.intern(style);
        assert_eq!(table.get(id), style);
        assert_eq!(table.get(StyleId::default()), Style::default());
        assert_eq!(table.intern(Style::default()), StyleId::default());
    }
}
