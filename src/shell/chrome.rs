//! Integrated Windows/Linux title and tab strip.

use std::sync::Arc;

use vello::Scene;
use vello::kurbo::{Affine, Rect};
use vello::peniko::{Color, Fill};
use winit::dpi::PhysicalSize;
use winit::window::Window;

use crate::config::UiConfig;
use crate::config::font::FontSize;
use crate::display::color::Rgb;
use crate::display::rects::{RenderRect, paint_rects};
use crate::display::renderer::{EmbeddedFramePlacement, Error, SceneRenderer};
use crate::display::text::TextSystem;
use crate::display::window::RenderSource;

use super::launch::LaunchEntry;
use super::menu::NewTabMenu;
use super::{PhysicalRect, Tabs};

pub const TAB_BAR_LOGICAL: f64 = 35.0;
const TAB_WIDTH_LOGICAL: f64 = 150.0;
const MIN_TAB_WIDTH_LOGICAL: f64 = 80.0;
const NEW_TAB_LOGICAL: f64 = 36.0;
const OVERFLOW_LOGICAL: f64 = 28.0;
const MIN_CONTENT_HEIGHT_LOGICAL: f64 = 80.0;
/// Corner radius of the window frame the shell draws for itself.
///
/// Wayland compositors such as Mutter leave a Vivido chrome window undecorated, so its frame —
/// rounded corners included — is ours to paint. Windows draws the rounded frame itself around the
/// undecorated shadow, so this only applies on Linux.
#[cfg(target_os = "linux")]
const CORNER_RADIUS_LOGICAL: f64 = 12.0;

const BACKGROUND: Rgb = Rgb::new(24, 24, 29);
pub(super) const ACTIVE: Rgb = Rgb::new(53, 53, 65);
pub(super) const BORDER: Rgb = Rgb::new(68, 68, 82);
pub(super) const TEXT: Rgb = Rgb::new(232, 232, 238);
const MUTED: Rgb = Rgb::new(155, 155, 170);
const ACCENT: Rgb = Rgb::new(129, 140, 248);
/// Surface of a panel floating above terminal content, a shade lighter than the tab strip so its
/// edge reads against both the bar and the pane behind it.
pub(super) const SURFACE: Rgb = Rgb::new(38, 38, 46);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChromeLayout {
    pub tab_bar: PhysicalRect,
    pub content: PhysicalRect,
}

#[derive(Clone, Debug, Default)]
pub struct ChromeHitMap {
    pub tabs: Vec<(usize, PhysicalRect)>,
    pub tab_closes: Vec<(usize, PhysicalRect)>,
    pub previous: PhysicalRect,
    pub next: PhysicalRect,
    pub new_tab: PhysicalRect,
    pub minimize: PhysicalRect,
    pub maximize: PhysicalRect,
    pub close: PhysicalRect,
}

pub fn compute_layout(size: PhysicalSize<u32>, scale: f64) -> ChromeLayout {
    let bottom_gutter = if cfg!(windows) { (10.0 * scale).round() as u32 } else { 0 };
    let available = size.height.saturating_sub(bottom_gutter);
    let tab_height = ((TAB_BAR_LOGICAL * scale).round() as u32)
        .min(available.saturating_sub((MIN_CONTENT_HEIGHT_LOGICAL * scale).round() as u32));
    ChromeLayout {
        tab_bar: PhysicalRect { x: 0, y: 0, width: size.width, height: tab_height },
        content: PhysicalRect {
            x: 0,
            y: i32::try_from(tab_height).unwrap_or(i32::MAX),
            width: size.width,
            height: available.saturating_sub(tab_height),
        },
    }
}

pub struct ChromeRenderer {
    #[cfg(target_os = "linux")]
    window: Arc<Window>,
    renderer: SceneRenderer,
    text: TextSystem,
    scale: f64,
    background: Rgb,
    opacity: f32,
}

impl ChromeRenderer {
    pub fn new(window: Arc<Window>, config: &UiConfig) -> Result<Self, Error> {
        let scale = window.scale_factor();
        Ok(Self {
            renderer: SceneRenderer::new(
                RenderSource::Surface(Arc::clone(&window)),
                window.inner_size(),
                true,
            )?,
            #[cfg(target_os = "linux")]
            window,
            text: text_system(config, scale),
            scale,
            background: config.colors.primary.background,
            opacity: config.window_opacity(),
        })
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>, scale: f64, config: &UiConfig) {
        self.renderer.resize(size);
        self.background = config.colors.primary.background;
        self.opacity = config.window_opacity();
        if (self.scale - scale).abs() > f64::EPSILON {
            self.scale = scale;
            self.text = text_system(config, scale);
        }
    }

    /// Lay a `+` menu out with the chrome's own text system and scale factor.
    pub fn layout_menu(
        &mut self,
        entries: Vec<LaunchEntry>,
        anchor: PhysicalRect,
        bounds: PhysicalSize<u32>,
    ) -> Option<NewTabMenu> {
        NewTabMenu::new(entries, anchor, bounds, &mut self.text, self.scale)
    }

    /// Draw the chrome, optionally with an open `+` menu floating over the pane area.
    pub fn render(
        &mut self,
        size: PhysicalSize<u32>,
        tabs: &mut Tabs,
        draw_controls: bool,
        frames: &[EmbeddedFramePlacement<'_>],
        menu: Option<&NewTabMenu>,
    ) -> Result<(ChromeLayout, ChromeHitMap, bool), Error> {
        let scale = self.scale;
        #[cfg(target_os = "linux")]
        self.renderer.set_corner_radius(self.corner_radius());
        let layout = compute_layout(size, scale);
        let mut hits = ChromeHitMap::default();
        let mut scene = Scene::new();
        let mut tab_bar = rect(layout.tab_bar, self.background);
        tab_bar.alpha = self.opacity;
        paint_rects(
            &mut scene,
            [
                tab_bar,
                RenderRect::new(
                    0.0,
                    layout.tab_bar.height.saturating_sub(1) as f32,
                    layout.tab_bar.width as f32,
                    scale.max(1.0) as f32,
                    BORDER,
                    1.0,
                ),
            ],
        );

        let controls_width = if draw_controls { system_control_width(scale) } else { 0 };
        if draw_controls {
            paint_system_controls(
                &mut scene,
                &mut self.text,
                layout.tab_bar,
                scale,
                self.background,
                &mut hits,
            );
        }
        self.paint_tabs(
            &mut scene,
            tabs,
            PhysicalRect {
                x: 0,
                y: 0,
                width: layout.tab_bar.width.saturating_sub(controls_width),
                height: layout.tab_bar.height,
            },
            &mut hits,
        );

        // Windows presents the menu in its own child window above the pane HWND, so only the
        // Linux compositing path draws it here and cuts it out of the frame copy.
        let exclude = menu.filter(|_| cfg!(target_os = "linux")).map(|menu| {
            menu.paint(&mut scene, &mut self.text, scale, (0.0, 0.0));
            let rect = menu.rect();
            (
                winit::dpi::PhysicalPosition::new(
                    u32::try_from(rect.x.max(0)).unwrap_or_default(),
                    u32::try_from(rect.y.max(0)).unwrap_or_default(),
                ),
                PhysicalSize::new(rect.width, rect.height),
            )
        });

        let presented = self.renderer.render_composited(
            &scene,
            Color::from_rgba8(BACKGROUND.r, BACKGROUND.g, BACKGROUND.b, 0),
            frames,
            exclude,
        )?;
        Ok((layout, hits, presented))
    }

    /// Physical corner radius for the current window state.
    ///
    /// A maximized or fullscreen window fills its output edge to edge, where a rounded corner
    /// would only cut a notch out of the desktop behind it.
    #[cfg(target_os = "linux")]
    fn corner_radius(&self) -> f32 {
        if self.window.is_maximized() || self.window.fullscreen().is_some() {
            return 0.0;
        }
        (CORNER_RADIUS_LOGICAL * self.scale) as f32
    }

    fn paint_tabs(
        &mut self,
        scene: &mut Scene,
        tabs: &mut Tabs,
        area: PhysicalRect,
        hits: &mut ChromeHitMap,
    ) {
        let scale = self.scale;
        let new_width = (NEW_TAB_LOGICAL * scale).round() as u32;
        let overflow_width = (OVERFLOW_LOGICAL * scale).round() as u32;
        let minimum = (MIN_TAB_WIDTH_LOGICAL * scale).round() as u32;
        let preferred = (TAB_WIDTH_LOGICAL * scale).round() as u32;
        let base_width = area.width.saturating_sub(new_width);
        let all_capacity = usize::try_from(base_width / minimum.max(1)).unwrap_or_default().max(1);
        let overflow = tabs.as_slice().len() > all_capacity;
        let reserved_overflow = if overflow { overflow_width.saturating_mul(2) } else { 0 };
        let tabs_width = base_width.saturating_sub(reserved_overflow);
        let capacity = usize::try_from(tabs_width / minimum.max(1)).unwrap_or_default().max(1);
        let visible = tabs.visible(capacity);
        let count = visible.range.len().max(1);
        let tab_width = preferred.min(tabs_width / u32::try_from(count).unwrap_or(u32::MAX).max(1));

        let mut x = area.x;
        if overflow {
            hits.previous = PhysicalRect { x, y: 0, width: overflow_width, height: area.height };
            self.text.paint_text(
                scene,
                "‹",
                (x as f32 + 8.0 * scale as f32, 8.0 * scale as f32),
                if visible.has_previous { TEXT } else { MUTED },
                true,
            );
            x = x.saturating_add(i32::try_from(overflow_width).unwrap_or(i32::MAX));
        }

        let active = tabs.active_index();
        for index in visible.range.clone() {
            let tab = &tabs.as_slice()[index];
            let row = PhysicalRect { x, y: 0, width: tab_width, height: area.height };
            hits.tabs.push((index, row));
            if active == Some(index) {
                paint_rects(scene, [rect(row, ACTIVE)]);
            }
            let close_width = (24.0 * scale).round() as u32;
            let close = PhysicalRect {
                x: row.right().saturating_sub(i32::try_from(close_width).unwrap_or(i32::MAX)),
                y: row.y,
                width: close_width.min(row.width),
                height: row.height,
            };
            hits.tab_closes.push((index, close));
            if let Some(clip) = title_clip(row, close_width, scale) {
                scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
                let title = ellipsize(&tab.title, clip.width(), scale);
                self.text.paint_text(
                    scene,
                    &title,
                    (row.x as f32 + 12.0 * scale as f32, 8.0 * scale as f32),
                    if active == Some(index) { TEXT } else { MUTED },
                    active == Some(index),
                );
                scene.pop_layer();
            }
            self.text.paint_text(
                scene,
                "×",
                (close.x as f32 + 5.0 * scale as f32, 8.0 * scale as f32),
                MUTED,
                false,
            );
            x = x.saturating_add(i32::try_from(tab_width).unwrap_or(i32::MAX));
        }

        if overflow {
            hits.next = PhysicalRect { x, y: 0, width: overflow_width, height: area.height };
            self.text.paint_text(
                scene,
                "›",
                (x as f32 + 8.0 * scale as f32, 8.0 * scale as f32),
                if visible.has_next { TEXT } else { MUTED },
                true,
            );
            x = x.saturating_add(i32::try_from(overflow_width).unwrap_or(i32::MAX));
        }
        hits.new_tab = PhysicalRect {
            x,
            y: 0,
            width: new_width.min(u32::try_from(area.right().saturating_sub(x)).unwrap_or_default()),
            height: area.height,
        };
        self.text.paint_text(
            scene,
            "+",
            (x as f32 + 10.0 * scale as f32, 8.0 * scale as f32),
            ACCENT,
            true,
        );
    }
}

fn paint_system_controls(
    scene: &mut Scene,
    text: &mut TextSystem,
    bar: PhysicalRect,
    scale: f64,
    background: Rgb,
    hits: &mut ChromeHitMap,
) {
    let total = system_control_width(scale).min(bar.width);
    let width = total / 3;
    let start = bar.width.saturating_sub(total);
    hits.minimize = control(start, bar, width);
    hits.maximize = control(start.saturating_add(width), bar, width);
    hits.close = control(
        start.saturating_add(width.saturating_mul(2)),
        bar,
        total.saturating_sub(width.saturating_mul(2)),
    );
    for control_rect in [hits.minimize, hits.maximize, hits.close] {
        paint_rects(scene, [rect(control_rect, background)]);
    }
    for (rect, label) in [(hits.minimize, "−"), (hits.maximize, "□"), (hits.close, "×")] {
        text.paint_text(
            scene,
            label,
            (rect.x as f32 + (rect.width as f32 / 2.0) - 5.0 * scale as f32, 8.0 * scale as f32),
            TEXT,
            false,
        );
    }
}

fn control(offset: u32, bar: PhysicalRect, width: u32) -> PhysicalRect {
    PhysicalRect { x: i32::try_from(offset).unwrap_or(i32::MAX), y: 0, width, height: bar.height }
}

fn system_control_width(scale: f64) -> u32 {
    if cfg!(windows) { (138.0 * scale).round() as u32 } else { (114.0 * scale).round() as u32 }
}

fn title_clip(tab: PhysicalRect, close_width: u32, scale: f64) -> Option<Rect> {
    let padding = (12.0 * scale).round() as u32;
    let width = tab.width.checked_sub(padding.saturating_add(close_width))?;
    (width > 0).then(|| {
        Rect::new(
            f64::from(tab.x) + f64::from(padding),
            0.0,
            f64::from(tab.x) + f64::from(padding) + f64::from(width),
            f64::from(tab.height),
        )
    })
}

pub(super) fn ellipsize(title: &str, width: f64, scale: f64) -> String {
    let capacity = (width / (7.0 * scale.max(0.1))).floor() as usize;
    let count = title.chars().count();
    if count <= capacity {
        return title.to_owned();
    }
    if capacity == 0 {
        return String::new();
    }
    let mut clipped = title.chars().take(capacity.saturating_sub(1)).collect::<String>();
    clipped.push('…');
    clipped
}

pub(super) fn text_system(config: &UiConfig, scale: f64) -> TextSystem {
    TextSystem::new(config.font.clone().with_size(FontSize::new((13.0 * scale) as f32)))
}

fn rect(rect: PhysicalRect, color: Rgb) -> RenderRect {
    RenderRect::new(rect.x as f32, rect.y as f32, rect.width as f32, rect.height as f32, color, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_reserves_the_tab_strip_from_terminal_content() {
        let layout = compute_layout(PhysicalSize::new(800, 600), 1.0);
        assert_eq!(layout.tab_bar.height, 35);
        assert_eq!(layout.content.y, 35);
        assert_eq!(layout.content.height, if cfg!(windows) { 555 } else { 565 });
    }

    #[test]
    fn layout_scales_tab_strip_and_uses_checked_small_geometry() {
        let scaled = compute_layout(PhysicalSize::new(1600, 1200), 2.0);
        assert_eq!(scaled.tab_bar.height, 70);
        assert_eq!(scaled.content.y, 70);

        let tiny = compute_layout(PhysicalSize::new(1, 1), 2.0);
        assert_eq!(tiny.tab_bar.height, 0);
        assert_eq!(tiny.content.height, if cfg!(windows) { 0 } else { 1 });
    }

    #[test]
    fn title_clip_rejects_tabs_without_text_space() {
        let tab = PhysicalRect { x: 0, y: 0, width: 20, height: 35 };
        assert!(title_clip(tab, 24, 1.0).is_none());
    }

    #[test]
    fn long_titles_are_ellipsized_without_splitting_unicode() {
        assert_eq!(ellipsize("abcdefgh", 28.0, 1.0), "abc…");
        assert_eq!(ellipsize("窗口标题", 21.0, 1.0), "窗口…");
        assert_eq!(ellipsize("short", 100.0, 1.0), "short");
    }
}
