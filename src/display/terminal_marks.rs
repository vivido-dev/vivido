//! Apply shared mark positioning with the current terminal font variant.
// Rust guideline compliant 2026-10-07
use super::{TextSystem, font_variant};
use crate::terminal::term::cell::Flags;
use vivid_sdk::vector_renderer::{marks, parley::FontData};

impl TextSystem {
    pub(in crate::display) fn fallback_marks(
        &mut self,
        source: &str,
        original: &FontData,
        flags: Flags,
        missing: usize,
    ) -> Option<Vec<marks::MarkLayout>> {
        let variant = font_variant(flags);
        marks::fallback(source, original, missing, |anchored| self.shape_text(anchored, variant))
    }
}
