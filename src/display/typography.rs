//! Bounded, grapheme-safe end truncation. The measured and painted layout is the same object.
use super::*;
use unicode_segmentation::UnicodeSegmentation;
use vivid_protocol::overlay::wire::text::styled::{StyledText, TextOverflow};

pub(crate) struct PreparedText {
    pub text: StyledText,
    pub layout: Layout<usize>,
    pub truncated_at: Option<usize>,
    /// Bytes of the host-inserted direction mark, excluded from public source offsets.
    pub prefix_bytes: usize,
}

impl TextSystem {
    pub(crate) fn prepare_styled(&mut self, text: &StyledText) -> PreparedText {
        let layout = self.shape_styled(text);
        if text.typography.overflow != TextOverflow::Ellipsis || text.text().is_empty() {
            return PreparedText {
                text: text.clone(),
                layout,
                truncated_at: None,
                prefix_bytes: 0,
            };
        }
        let width = text.max_width.map_or(f64::MAX, |w| w.get());
        let lines = usize::from(text.max_lines.unwrap_or(1));
        let fits = |layout: &Layout<usize>| {
            layout.len() <= lines
                && layout.lines().all(|line| f64::from(line.metrics().advance) <= width + 0.001)
        };
        if fits(&layout) {
            return PreparedText {
                text: text.clone(),
                layout,
                truncated_at: None,
                prefix_bytes: 0,
            };
        }
        let source = text.text();
        let boundaries: Vec<_> = source
            .grapheme_indices(true)
            .map(|(offset, _)| offset)
            .chain(std::iter::once(source.len()))
            .collect();
        let direction = if layout.is_rtl() { '\u{200f}' } else { '\u{200e}' };
        let mut best_text = prefix(text, 0, direction);
        let mut best_layout = self.shape_styled(&best_text);
        if !fits(&best_layout) {
            // Never draw a clipped fragment of the marker itself.
            best_text.runs.truncate(1);
            best_text.runs[0].text.clear();
            best_layout = self.shape_styled(&best_text);
            return PreparedText {
                text: best_text,
                layout: best_layout,
                truncated_at: Some(0),
                prefix_bytes: 0,
            };
        }
        let mut low = 0;
        let mut high = boundaries.len();
        // At most ceil(log2(4097)) probes. Keep only candidates whose complete layout fits.
        while low + 1 < high {
            let mid = low + (high - low) / 2;
            let candidate = prefix(text, boundaries[mid], direction);
            let shaped = self.shape_styled(&candidate);
            if fits(&shaped) {
                low = mid;
                best_text = candidate;
                best_layout = shaped;
            } else {
                high = mid;
            }
        }
        PreparedText {
            text: best_text,
            layout: best_layout,
            truncated_at: Some(boundaries[low]),
            prefix_bytes: direction.len_utf8(),
        }
    }
}

fn prefix(text: &StyledText, cut: usize, direction: char) -> StyledText {
    let mut result = text.clone();
    result.runs.clear();
    let mut offset = 0;
    for run in &text.runs {
        if offset >= cut {
            break;
        }
        let mut kept = run.clone();
        let count = (cut - offset).min(run.text.len());
        kept.text.truncate(count);
        result.runs.push(kept);
        offset += run.text.len();
    }
    if result.runs.is_empty() {
        let mut run = text.runs[0].clone();
        run.text.clear();
        result.runs.push(run);
    }
    result.runs[0].text.insert(0, direction);
    if let Some(last) = result.runs.last_mut() {
        last.text.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
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
            let prepared = system.prepare_styled(&text);
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
        let prepared = system.prepare_styled(&styled);
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
        let tiny = system.prepare_styled(&text);
        assert_eq!(tiny.truncated_at, Some(0));
        assert!(tiny.text.text().is_empty());
        text.runs[0].text.clear();
        assert!(system.prepare_styled(&text).truncated_at.is_none());
        text.runs[0].text = "a\nb".into();
        text.max_width = Some(Scalar::new(1000.).unwrap());
        text.max_lines = Some(3);
        assert!(system.prepare_styled(&text).truncated_at.is_none());
    }
}
