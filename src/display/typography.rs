//! Terminal font policy for the shared, grapheme-safe SDK text preparation.

// Rust guideline compliant 2026-10-07
use super::*;
use vivid_protocol::overlay::wire::text::styled::StyledText;
use vivid_protocol::vector::InvalidScene;

impl vivid_sdk::vector_renderer::text::StyledTextShaper for TextSystem {
    fn shape_styled(&mut self, text: &StyledText) -> Result<Layout<usize>, InvalidScene> {
        Ok(TextSystem::shape_styled(self, text))
    }
}

#[cfg(test)]
impl TextSystem {
    fn prepare_styled(
        &mut self,
        text: &StyledText,
    ) -> Result<vivid_sdk::vector_renderer::text::PreparedText, InvalidScene> {
        vivid_sdk::vector_renderer::text::prepare_styled(self, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_segmentation::UnicodeSegmentation;
    use vivid_protocol::overlay::wire::text::styled::TextOverflow;
    use vivid_protocol::overlay::wire::text::styled::{TextRun, TextStyle};
    use vivid_protocol::vector::Scalar;

    #[test]
    fn ellipsis_preserves_graphemes_styles_direction_and_fits_the_final_lines() {
        let mut system = TextSystem::new(Default::default());
        for source in [
            "ASCII words with many letters",
            "e\u{301}e\u{301}e\u{301}abc long text",
            "👨‍👩‍👧‍👦🇺🇸 many words after emoji",
            "אבג דהו זחט יכל מנס",
            "a\nb\nc\nd",
        ] {
            let mut text = StyledText::new(source, TextStyle::default());
            text.max_width = Some(Scalar::new(85.).unwrap());
            text.max_lines = Some(1);
            text.typography.overflow = TextOverflow::Ellipsis;
            let full = system.shape_styled(&text);
            let prepared = system.prepare_styled(&text).unwrap();
            let cut = prepared.truncated_at.expect("fixture must truncate");
            assert!(source.grapheme_indices(true).any(|(i, _)| i == cut));
            assert!(prepared.text.text().ends_with('…'));
            assert_eq!(prepared.layout.is_rtl(), full.is_rtl());
            assert!(prepared.layout.len() <= 1);
            assert!(prepared.layout.lines().all(|l| l.metrics().advance <= 85.001));
        }
        let mut styled = StyledText::new("small ", TextStyle::default());
        styled.runs.push(TextRun {
            text: "BIG SECOND RUN TO TRUNCATE".into(),
            style: TextStyle {
                size: Scalar::new(30.).unwrap(),
                underline: true,
                ..Default::default()
            },
        });
        styled.max_width = Some(Scalar::new(240.).unwrap());
        styled.typography.overflow = TextOverflow::Ellipsis;
        let prepared = system.prepare_styled(&styled).unwrap();
        assert!(prepared.truncated_at.unwrap() > 6);
        assert!(prepared.text.runs.last().unwrap().style.underline);
    }

    #[test]
    fn typography_spacing_line_height_and_tiny_or_unconstrained_text() {
        let mut system = TextSystem::new(Default::default());
        let mut text = StyledText::new("a b", TextStyle::default());
        let original = system.shape_styled(&text).full_width();
        text.typography.letter_spacing = Scalar::new(2.).unwrap();
        text.typography.word_spacing = Scalar::new(5.).unwrap();
        assert!(system.shape_styled(&text).full_width() > original + 5.);
        text.runs[0].text = "a\nb".into();
        text.typography.line_height = Some(Scalar::new(40.).unwrap());
        text.typography.ligatures = false;
        text.typography.kerning = false;
        assert!((system.shape_styled(&text).height() - 80.).abs() < 0.01);
        text.typography.overflow = TextOverflow::Ellipsis;
        text.max_width = Some(Scalar::new(0.01).unwrap());
        let tiny = system.prepare_styled(&text).unwrap();
        assert_eq!(tiny.truncated_at, Some(0));
        assert!(tiny.text.text().is_empty());
        text.runs[0].text.clear();
        assert!(system.prepare_styled(&text).unwrap().truncated_at.is_none());
        text.runs[0].text = "a\nb".into();
        text.max_width = Some(Scalar::new(1000.).unwrap());
        text.max_lines = Some(3);
        assert!(system.prepare_styled(&text).unwrap().truncated_at.is_none());
    }

    #[test]
    fn unwrapped_paragraphs_ignore_soft_breaks_but_keep_explicit_line_breaks() {
        let mut system = TextSystem::new(Default::default());
        let mut text =
            StyledText::new("many words that exceed the box width", TextStyle::default());
        text.max_width = Some(Scalar::new(30.).unwrap());
        text.wrap = false;
        let layout = system.shape_styled(&text);
        assert_eq!(layout.len(), 1);
        assert!(layout.lines().next().unwrap().metrics().advance > 30.);
        text.runs[0].text.push_str("\nsecond explicit line");
        assert_eq!(system.shape_styled(&text).len(), 2);
        text.typography.overflow = TextOverflow::Ellipsis;
        text.max_lines = Some(1);
        let prepared = system.prepare_styled(&text).unwrap();
        assert!(prepared.truncated_at.is_some());
        assert_eq!(prepared.layout.len(), 1);
        assert!(prepared.layout.lines().all(|line| line.metrics().advance <= 30.001));
    }
}
