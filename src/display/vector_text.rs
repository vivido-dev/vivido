//! Immutable, host-shaped glyph scenes. Painting these never invokes the shaper again.
use super::*;
use vivid_protocol::overlay::wire::text::styled::StyledText;

pub struct MeasuredLayout {
    pub scene: Scene,
    pub width: f64,
    pub height: f64,
    pub token: Arc<()>,
}

pub fn paint(
    layout: &parley::Layout<usize>,
    text: &StyledText,
    width: f64,
    height: f64,
) -> Result<MeasuredLayout, InvalidScene> {
    check_bounds(Rect::new(0., 0., width, height), Affine::IDENTITY)?;
    let mut scene = Scene::new();
    // Clip truncation and non-wrapping overflow to the measured box.
    let clip_width = text.max_width.map_or(width.max(1.), |w| w.get());
    scene.push_layer(
        Fill::NonZero,
        Mix::Normal,
        1.,
        Affine::IDENTITY,
        &Rect::new(0., 0., clip_width, height),
    );
    for line in layout.lines().take(text.max_lines.map_or(usize::MAX, usize::from)) {
        for item in line.items() {
            let parley::layout::PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };
            let run = glyph_run.run();
            let style = &text.runs[glyph_run.style().brush].style;
            let start = glyph_run.offset();
            let baseline = glyph_run.baseline();
            let mut x = start;
            scene
                .draw_glyphs(run.font())
                .font_size(run.font_size())
                .normalized_coords(run.normalized_coords())
                .brush(color(style.color))
                .hint(false)
                .draw(
                    Fill::NonZero,
                    glyph_run.glyphs().map(|g| {
                        let glyph = Glyph { id: g.id, x: x + g.x, y: baseline - g.y };
                        x += g.advance;
                        glyph
                    }),
                );
            let metrics = run.metrics();
            for (enabled, offset, thickness) in [
                (style.underline, metrics.underline_offset, metrics.underline_size),
                (style.strikethrough, metrics.strikethrough_offset, metrics.strikethrough_size),
            ] {
                if enabled {
                    let y = f64::from(baseline - offset);
                    scene.fill(
                        Fill::NonZero,
                        Affine::IDENTITY,
                        color(style.color),
                        None,
                        &Rect::new(
                            f64::from(start),
                            y,
                            f64::from(x),
                            y + f64::from(thickness.max(0.5)),
                        ),
                    );
                }
            }
        }
    }
    scene.pop_layer();
    Ok(MeasuredLayout { scene, width: clip_width, height, token: Arc::new(()) })
}
