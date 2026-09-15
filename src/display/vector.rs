//! Compilation of portable display lists into cached Vello scenes and matching hit geometry.
//! This work is independent of a GPU and belongs on a media worker, not the UI event loop.

use std::collections::BTreeMap;
use std::sync::Arc;

use vello::kurbo::{
    Affine, BezPath, Cap, Join, Point, Rect, RoundedRect, RoundedRectRadii, Shape, Stroke,
};
use vello::peniko::color::ColorSpaceTag;
use vello::peniko::{
    Brush, Color, ColorStop, Extend as ImageExtend, Fill, Gradient, ImageBrush, ImageData, Mix,
};
use vello::{Glyph, Scene};
use vivid_protocol::vector::{self as wire, Canvas, Command, HitRole, InvalidScene, Segment};

use super::text::TextSystem;

#[path = "vector_text.rs"]
pub mod text_layout;
use text_layout::MeasuredLayout;

#[derive(Clone)]
struct HitPath {
    path: Arc<BezPath>,
    inverse: Affine,
    even_odd: bool,
}
impl HitPath {
    fn contains(&self, point: Point) -> bool {
        let winding = self.path.winding(self.inverse * point);
        if self.even_odd { winding % 2 != 0 } else { winding != 0 }
    }
}

struct HitRegion {
    id: u64,
    role: HitRole,
    path: HitPath,
    clips: Vec<HitPath>,
}

/// Drawing and hit testing are published as one immutable object.
pub struct CompiledScene {
    pub scene: Scene,
    pub(crate) retained: Vec<Arc<()>>,
    hits: Vec<HitRegion>,
}
impl CompiledScene {
    /// Coordinates are already window-local logical pixels. The caller clips to the window.
    /// A window without explicit regions has a rectangular input area.
    pub fn hit(&self, point: wire::Point) -> Option<(u64, HitRole)> {
        if self.hits.is_empty() {
            return Some((0, HitRole::Input));
        }
        let point = Point::new(point.x.get(), point.y.get());
        self.hits
            .iter()
            .rev()
            .find(|hit| {
                hit.path.contains(point) && hit.clips.iter().all(|clip| clip.contains(point))
            })
            .and_then(|hit| (hit.role != HitRole::Transparent).then_some((hit.id, hit.role)))
    }
}

#[derive(Clone)]
struct DrawState {
    transform: Affine,
    opacity: f32,
    clips: Vec<HitPath>,
}

/// Compile an entire validated list before exposing any of its drawing or input state.
/// Retained images are resolved within the caller's complete channel identity.
pub fn compile(
    canvas: &Canvas,
    text: &mut TextSystem,
    images: &BTreeMap<u64, ImageData>,
) -> Result<CompiledScene, InvalidScene> {
    compile_with_layouts(canvas, text, images, &BTreeMap::new())
}

pub fn compile_with_layouts(
    canvas: &Canvas,
    text: &mut TextSystem,
    images: &BTreeMap<u64, ImageData>,
    layouts: &BTreeMap<u64, Arc<MeasuredLayout>>,
) -> Result<CompiledScene, InvalidScene> {
    canvas.validate()?;
    let mut result = CompiledScene { scene: Scene::new(), retained: Vec::new(), hits: Vec::new() };
    let mut state = DrawState { transform: Affine::IDENTITY, opacity: 1.0, clips: Vec::new() };
    let mut stack = Vec::new();
    for command in canvas.commands() {
        // Bound transformed geometry before it reaches Vello's GPU encoders. Individually
        // bounded scalar operands can otherwise multiply into an unbounded device coordinate.
        match command {
            Command::Fill(path, _) | Command::Clip(path) | Command::Hit { path, .. } => {
                check_bounds(path_geometry(path).bounding_box(), state.transform)?;
            },
            Command::Stroke(path, _, width) => {
                check_bounds(
                    path_geometry(path).bounding_box().inflate(width.get() * 4., width.get() * 4.),
                    state.transform,
                )?;
            },
            Command::Image { rect, .. } => {
                check_bounds(
                    Rect::new(
                        rect.origin.x.get(),
                        rect.origin.y.get(),
                        rect.origin.x.get() + rect.width.get(),
                        rect.origin.y.get() + rect.height.get(),
                    ),
                    state.transform,
                )?;
            },
            _ => {},
        }
        match command {
            Command::Save => stack.push(state.clone()),
            Command::Restore => {
                let previous = stack.pop().ok_or(InvalidScene("unbalanced restore"))?;
                for _ in previous.clips.len()..state.clips.len() {
                    result.scene.pop_layer();
                }
                state = previous;
            },
            Command::Transform(transform) => {
                let transform = state.transform * Affine::new(transform.0.map(|v| v.get()));
                // Invertibility and bounded accumulated transforms are needed for hit testing.
                if transform.as_coeffs().iter().any(|v| !v.is_finite() || v.abs() > 1_000_000.0)
                    || transform.determinant().abs() < 1e-12
                {
                    return Err(InvalidScene("unbounded or singular accumulated transform"));
                }
                state.transform = transform;
            },
            Command::Opacity(alpha) => state.opacity = f32::from(*alpha) / 65535.0,
            Command::Clip(path) => {
                if state.clips.len() >= wire::MAX_STACK_DEPTH {
                    return Err(InvalidScene("clip nesting limit exceeded"));
                }
                let compiled = path_geometry(path);
                result.scene.push_clip_layer(fill_rule(path), state.transform, &compiled);
                state.clips.push(HitPath {
                    path: Arc::new(compiled),
                    inverse: state.transform.inverse(),
                    even_odd: path.even_odd,
                });
            },
            Command::Hit { id, path, role } => result.hits.push(HitRegion {
                id: *id,
                role: *role,
                path: HitPath {
                    path: Arc::new(path_geometry(path)),
                    inverse: state.transform.inverse(),
                    even_odd: path.even_odd,
                },
                clips: state.clips.clone(),
            }),
            command => {
                let image_opacity = if let Command::Image { opacity, .. } = command {
                    f32::from(*opacity) / 65535.0
                } else {
                    1.0
                };
                let opacity = state.opacity * image_opacity;
                if opacity != 1.0 {
                    result.scene.push_layer(
                        Fill::NonZero,
                        Mix::Normal,
                        opacity,
                        Affine::IDENTITY,
                        &Rect::new(-1e6, -1e6, 1e6, 1e6),
                    );
                }
                match command {
                    Command::TextLayout { layout, origin } => {
                        let layout = layouts
                            .get(layout)
                            .ok_or(InvalidScene("unknown or released text layout"))?;
                        let transform =
                            state.transform * Affine::translate((origin.x.get(), origin.y.get()));
                        check_bounds(Rect::new(0., 0., layout.width, layout.height), transform)?;
                        result.scene.append(&layout.scene, Some(transform));
                        result.retained.push(layout.token.clone());
                    },
                    Command::Fill(path, brush) => result.scene.fill(
                        fill_rule(path),
                        state.transform,
                        &paint(brush, images)?,
                        brush_transform(brush),
                        &path_geometry(path),
                    ),
                    Command::Stroke(path, brush, width) => result.scene.stroke(
                        &Stroke::new(width.get()),
                        state.transform,
                        &paint(brush, images)?,
                        brush_transform(brush),
                        &path_geometry(path),
                    ),
                    Command::StyledStroke(path, brush, style) => {
                        let mut stroke = Stroke::new(style.width.get())
                            .with_caps(match style.cap {
                                wire::Cap::Butt => Cap::Butt,
                                wire::Cap::Round => Cap::Round,
                                wire::Cap::Square => Cap::Square,
                            })
                            .with_join(match style.join {
                                wire::Join::Miter => Join::Miter,
                                wire::Join::Bevel => Join::Bevel,
                                wire::Join::Round => Join::Round,
                            })
                            .with_miter_limit(style.miter_limit.get());
                        if !style.dashes.is_empty() {
                            stroke = stroke.with_dashes(
                                style.dash_offset.get(),
                                style.dashes.iter().map(|d| d.get()),
                            );
                        }
                        result.scene.stroke(
                            &stroke,
                            state.transform,
                            &paint(brush, images)?,
                            brush_transform(brush),
                            &path_geometry(path),
                        );
                    },
                    Command::Shadow(shadow) => {
                        draw_shadow(&mut result.scene, shadow, state.transform)?;
                    },
                    Command::Text(value) => {
                        let layout = text.shape_overlay(value);
                        let transform = state.transform
                            * Affine::translate((value.origin.x.get(), value.origin.y.get()));
                        check_bounds(
                            Rect::new(
                                0.,
                                0.,
                                f64::from(layout.width()),
                                f64::from(layout.height()),
                            ),
                            transform,
                        )?;
                        for line in layout.lines() {
                            for item in line.items() {
                                let parley::layout::PositionedLayoutItem::GlyphRun(glyph_run) =
                                    item
                                else {
                                    continue;
                                };
                                let run = glyph_run.run();
                                let mut x = glyph_run.offset();
                                let y = glyph_run.baseline();
                                result
                                    .scene
                                    .draw_glyphs(run.font())
                                    .font_size(run.font_size())
                                    .transform(transform)
                                    .normalized_coords(run.normalized_coords())
                                    .brush(color(value.color))
                                    .hint(false)
                                    .draw(
                                        Fill::NonZero,
                                        glyph_run.glyphs().map(|glyph| {
                                            let result = Glyph {
                                                id: glyph.id,
                                                x: x + glyph.x,
                                                y: y - glyph.y,
                                            };
                                            x += glyph.advance;
                                            result
                                        }),
                                    );
                            }
                        }
                    },
                    Command::Image { asset, rect, .. } => {
                        let image =
                            images.get(asset).ok_or(InvalidScene("unknown retained image"))?;
                        if image.width == 0 || image.height == 0 {
                            return Err(InvalidScene("empty retained image"));
                        }
                        if image.format.size_in_bytes(image.width, image.height)
                            != Some(image.data.len())
                            || image.data.len() > wire::MAX_ASSET_BYTES
                        {
                            return Err(InvalidScene("invalid retained image bytes"));
                        }
                        let transform = state.transform
                            * Affine::translate((rect.origin.x.get(), rect.origin.y.get()))
                            * Affine::scale_non_uniform(
                                rect.width.get() / f64::from(image.width),
                                rect.height.get() / f64::from(image.height),
                            );
                        result.scene.draw_image(image, transform);
                    },
                    _ => unreachable!("state commands were handled before drawing"),
                }
                if opacity != 1.0 {
                    result.scene.pop_layer();
                }
            },
        }
    }
    for _ in &state.clips {
        result.scene.pop_layer();
    }
    Ok(result)
}

fn check_bounds(bounds: Rect, transform: Affine) -> Result<(), InvalidScene> {
    for point in [
        Point::new(bounds.x0, bounds.y0),
        Point::new(bounds.x1, bounds.y0),
        Point::new(bounds.x0, bounds.y1),
        Point::new(bounds.x1, bounds.y1),
    ] {
        let p = transform * point;
        if !p.x.is_finite() || !p.y.is_finite() || p.x.abs() > 1e6 || p.y.abs() > 1e6 {
            return Err(InvalidScene("transformed drawing exceeds coordinate bounds"));
        }
    }
    Ok(())
}

fn fill_rule(path: &wire::Path) -> Fill {
    if path.even_odd { Fill::EvenOdd } else { Fill::NonZero }
}
fn point(p: wire::Point) -> Point {
    Point::new(p.x.get(), p.y.get())
}
fn path_geometry(path: &wire::Path) -> BezPath {
    let mut result = BezPath::new();
    for segment in &path.segments {
        match segment {
            Segment::Move(p) => result.move_to(point(*p)),
            Segment::Line(p) => result.line_to(point(*p)),
            Segment::Quad(a, b) => result.quad_to(point(*a), point(*b)),
            Segment::Cubic(a, b, c) => result.curve_to(point(*a), point(*b), point(*c)),
            Segment::Close => result.close_path(),
        }
    }
    result
}
fn color(c: wire::Color) -> Color {
    let [r, g, b, a] = c.0.to_be_bytes();
    Color::from_rgba8(r, g, b, a)
}
fn paint(brush: &wire::Brush, images: &BTreeMap<u64, ImageData>) -> Result<Brush, InvalidScene> {
    let (gradient, stops, space) = match brush {
        wire::Brush::Solid(c) => return Ok(Brush::Solid(color(*c))),
        wire::Brush::Linear { start, end, stops, color_space } => {
            (Gradient::new_linear(point(*start), point(*end)), stops, *color_space)
        },
        wire::Brush::Radial { center, radius, stops, color_space } => {
            (Gradient::new_radial(point(*center), radius.get() as f32), stops, *color_space)
        },
        wire::Brush::Image { asset, extend, .. } => {
            let image = images.get(asset).ok_or(InvalidScene("unknown retained image"))?;
            if image.width == 0 || image.height == 0 {
                return Err(InvalidScene("empty retained image"));
            }
            if image.format.size_in_bytes(image.width, image.height) != Some(image.data.len())
                || image.data.len() > wire::MAX_ASSET_BYTES
            {
                return Err(InvalidScene("invalid retained image bytes"));
            }
            let brush = ImageBrush::new(image.clone()).with_extend(match extend {
                wire::Extend::Pad => ImageExtend::Pad,
                wire::Extend::Repeat => ImageExtend::Repeat,
                wire::Extend::Reflect => ImageExtend::Reflect,
            });
            return Ok(Brush::Image(brush));
        },
    };
    let stops: Vec<_> = stops
        .iter()
        .map(|stop| ColorStop {
            offset: f32::from(stop.offset) / 65535.0,
            color: color(stop.color).into(),
        })
        .collect();
    let gradient = gradient.with_stops(stops.as_slice());
    Ok(Brush::Gradient(match space {
        wire::ColorSpace::Srgb => gradient,
        wire::ColorSpace::Oklab => gradient.with_interpolation_cs(ColorSpaceTag::Oklab),
    }))
}

/// The affine mapping an image brush paints through, if it carries one.
fn brush_transform(brush: &wire::Brush) -> Option<Affine> {
    let wire::Brush::Image { transform, .. } = brush else {
        return None;
    };
    transform.map(|t| {
        Affine::new([
            t.0[0].get(),
            t.0[1].get(),
            t.0[2].get(),
            t.0[3].get(),
            t.0[4].get(),
            t.0[5].get(),
        ])
    })
}

/// A wire shadow's shape after spread, with its radii grown or shrunk to match.
fn shadow_shape(shadow: &wire::Shadow) -> (Rect, RoundedRectRadii) {
    let spread = shadow.spread.get();
    let rect = Rect::new(
        shadow.rect.origin.x.get(),
        shadow.rect.origin.y.get(),
        shadow.rect.origin.x.get() + shadow.rect.width.get(),
        shadow.rect.origin.y.get() + shadow.rect.height.get(),
    )
    .inflate(spread, spread);
    let [tl, tr, br, bl] = shadow.radii.0.map(|r| (r.get() + spread).max(0.0));
    (rect, RoundedRectRadii::new(tl, tr, br, bl))
}

/// Paint one shadow, honouring the current transform.
///
/// Vello's blurred-rounded-rect primitive takes a single radius, so a shadow whose four corners
/// agree gets a true gaussian. Mixed corners and inset shadows are approximated with stepped
/// rings whose alphas follow the same gaussian falloff: bounded work, an exact wire value, and
/// an approximation that lives only in this host where a better renderer can replace it.
fn draw_shadow(
    scene: &mut Scene,
    shadow: &wire::Shadow,
    transform: Affine,
) -> Result<(), InvalidScene> {
    let (rect, radii) = shadow_shape(shadow);
    let sigma = shadow.blur.get() / 2.0;
    let offset = (shadow.offset.x.get(), shadow.offset.y.get());
    let paint = transform * Affine::translate(offset);
    check_bounds(rect, paint)?;
    let uniform = shadow.radii.uniform_value().map(|r| (r + shadow.spread.get()).max(0.0));
    if !shadow.inset
        && let Some(radius) = uniform
        && sigma >= 0.25
    {
        scene.draw_blurred_rounded_rect(paint, rect, color(shadow.color), radius, sigma);
        return Ok(());
    }
    let steps = SHADOW_STEPS as f64;
    if sigma < 0.25 {
        // No blur to approximate: the shadow is the spread-adjusted shape itself.
        let shape = RoundedRect::from_rect(rect, radii);
        if shadow.inset {
            scene.push_layer(Fill::NonZero, Mix::Normal, 1.0, paint, &shape);
            scene.fill(Fill::NonZero, paint, color(shadow.color), None, &shape);
            scene.pop_layer();
        } else {
            scene.fill(Fill::NonZero, paint, color(shadow.color), None, &shape);
        }
        return Ok(());
    }
    // How far the falloff runs before it is imperceptible, matching vello's own kernel extent.
    let extent = 2.5 * sigma;
    let [rtl, rtr, rbr, rbl] =
        [radii.top_left, radii.top_right, radii.bottom_right, radii.bottom_left];
    if shadow.inset {
        // Clip to the shape, then stack eroded fills deepest first: a point at depth p ends up
        // carrying the alpha of the largest erosion that still covers it, which is the falloff
        // at p. The erosion is translated by the offset so the shadow leans away from its light.
        let shape = RoundedRect::from_rect(rect, radii);
        scene.push_layer(Fill::NonZero, Mix::Normal, 1.0, transform, &shape);
        let lean = Affine::translate(offset);
        for step in (0..SHADOW_STEPS).rev() {
            let depth = (step as f64 + 0.5) / steps * extent;
            if rect.width() / 2.0 <= depth || rect.height() / 2.0 <= depth {
                continue;
            }
            let eroded = rect.inflate(-depth, -depth);
            let eroded = RoundedRect::new(
                eroded.x0,
                eroded.y0,
                eroded.x1,
                eroded.y1,
                RoundedRectRadii::new(
                    (rtl - depth).max(0.0),
                    (rtr - depth).max(0.0),
                    (rbr - depth).max(0.0),
                    (rbl - depth).max(0.0),
                ),
            );
            let alpha = falloff(depth, sigma);
            scene.fill(
                Fill::NonZero,
                transform * lean,
                color(shadow.color).multiply_alpha(alpha),
                None,
                &eroded,
            );
        }
        scene.pop_layer();
    } else {
        // Inflate outward, largest extent first, so each smaller fill lands on top with the
        // higher alpha a nearer distance deserves.
        for step in (0..SHADOW_STEPS).rev() {
            let distance = (step as f64 + 0.5) / steps * extent;
            let grown = rect.inflate(distance, distance);
            let grown = RoundedRect::new(
                grown.x0,
                grown.y0,
                grown.x1,
                grown.y1,
                RoundedRectRadii::new(
                    rtl + distance,
                    rtr + distance,
                    rbr + distance,
                    rbl + distance,
                ),
            );
            let alpha = falloff(distance, sigma);
            scene.fill(
                Fill::NonZero,
                paint,
                color(shadow.color).multiply_alpha(alpha),
                None,
                &grown,
            );
        }
    }
    Ok(())
}

/// One stepped falloff weight of a gaussian edge at `distance` from the shape.
fn falloff(distance: f64, sigma: f64) -> f32 {
    0.5 * (-distance * distance / (2.0 * sigma * sigma)).exp() as f32
}

/// How many stepped rings approximate a blur. Ten keeps a band below about a pixel for the
/// small blurs a user interface actually casts.
const SHADOW_STEPS: usize = 10;
