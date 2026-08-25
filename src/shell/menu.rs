//! Dropdown anchored to the tab strip's `+` button.
//!
//! The menu is renderer-independent: it lays itself out against a [`TextSystem`], answers hit tests
//! and keyboard motion, and paints into a [`Scene`] at a caller-chosen offset. Linux draws that
//! scene straight into the chrome window, Windows into a child popup over the pane, and both share
//! everything here.

use vello::Scene;
use vello::kurbo::{Affine, Rect};
use vello::peniko::Fill;
use winit::dpi::PhysicalSize;

use crate::display::rects::{RenderRect, paint_rects};
use crate::display::text::TextSystem;

use super::PhysicalRect;
use super::chrome::{ACTIVE, BORDER, SURFACE, TEXT, ellipsize};
use super::launch::{LaunchAction, LaunchEntry};

const ROW_LOGICAL: f64 = 26.0;
const PADDING_X_LOGICAL: f64 = 12.0;
const PADDING_Y_LOGICAL: f64 = 4.0;
const MIN_WIDTH_LOGICAL: f64 = 140.0;
const MAX_WIDTH_LOGICAL: f64 = 360.0;
/// Cap height of the 13px UI font, used to centre a label in its row.
const LABEL_HEIGHT_LOGICAL: f64 = 17.0;

/// An open `+` menu, positioned in the chrome window's physical coordinates.
#[derive(Clone, Debug)]
pub struct NewTabMenu {
    entries: Vec<LaunchEntry>,
    rect: PhysicalRect,
    row_height: u32,
    padding: PhysicalSize<u32>,
    selected: Option<usize>,
}

impl NewTabMenu {
    /// Lay `entries` out under `anchor`, clamped inside `bounds`.
    ///
    /// Returns `None` when there is nothing to show or no room below the anchor to show it in.
    pub fn new(
        entries: Vec<LaunchEntry>,
        anchor: PhysicalRect,
        bounds: PhysicalSize<u32>,
        text: &mut TextSystem,
        scale: f64,
    ) -> Option<Self> {
        if entries.is_empty() {
            return None;
        }
        let row_height = ((ROW_LOGICAL * scale).round() as u32).max(1);
        let padding = PhysicalSize::new(
            (PADDING_X_LOGICAL * scale).round() as u32,
            (PADDING_Y_LOGICAL * scale).round() as u32,
        );

        let widest = entries
            .iter()
            .map(|entry| text.measure_text(&entry.label, false))
            .fold(0.0_f32, f32::max)
            .max(0.0)
            .ceil() as u32;
        let width = widest
            .saturating_add(padding.width.saturating_mul(2))
            .clamp((MIN_WIDTH_LOGICAL * scale) as u32, (MAX_WIDTH_LOGICAL * scale) as u32)
            .min(bounds.width);

        let top = anchor.bottom().max(0);
        let available = i32::try_from(bounds.height).unwrap_or(i32::MAX).saturating_sub(top);
        let available = u32::try_from(available).unwrap_or_default();
        let rows = u32::try_from(entries.len()).unwrap_or(u32::MAX);
        let height = row_height
            .saturating_mul(rows)
            .saturating_add(padding.height.saturating_mul(2))
            .min(available);
        if width == 0 || height == 0 {
            return None;
        }

        // Right-align with the button, then slide back inside the window if that overhangs.
        let last_x = i32::try_from(bounds.width.saturating_sub(width)).unwrap_or_default();
        let x = anchor
            .right()
            .saturating_sub(i32::try_from(width).unwrap_or(i32::MAX))
            .clamp(0, last_x.max(0));

        Some(Self {
            entries,
            rect: PhysicalRect { x, y: top, width, height },
            row_height,
            padding,
            selected: None,
        })
    }

    pub fn rect(&self) -> PhysicalRect {
        self.rect
    }

    pub fn entries(&self) -> &[LaunchEntry] {
        &self.entries
    }

    /// Every row which fits inside the panel, with its physical rectangle.
    pub fn rows(&self) -> impl Iterator<Item = (usize, PhysicalRect)> + '_ {
        (0..self.entries.len()).filter_map(|index| Some((index, self.row(index)?)))
    }

    /// The entry under a point in chrome coordinates.
    pub fn hit(&self, x: f64, y: f64) -> Option<usize> {
        if !self.rect.contains(x, y) {
            return None;
        }
        self.rows().find_map(|(index, row)| row.contains(x, y).then_some(index))
    }

    /// Track the pointer; returns whether the highlighted row changed.
    pub fn hover(&mut self, x: f64, y: f64) -> bool {
        let hovered = self.hit(x, y);
        let changed = hovered != self.selected;
        self.selected = hovered;
        changed
    }

    /// Move the highlight by `delta` rows, wrapping at both ends.
    pub fn move_selection(&mut self, delta: isize) -> bool {
        let Ok(len) = isize::try_from(self.entries.len()) else { return false };
        if len == 0 {
            return false;
        }
        let current = self.selected.and_then(|index| isize::try_from(index).ok());
        let next = match current {
            Some(current) => (current + delta).rem_euclid(len),
            // Entering from the keyboard starts at whichever end the motion came from.
            None if delta < 0 => len - 1,
            None => 0,
        };
        self.selected = usize::try_from(next).ok();
        true
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub fn selected(&self) -> Option<&LaunchAction> {
        self.entries.get(self.selected?).map(|entry| &entry.action)
    }

    pub fn action(&self, index: usize) -> Option<&LaunchAction> {
        self.entries.get(index).map(|entry| &entry.action)
    }

    /// Paint the panel, translated by `offset`.
    ///
    /// Linux passes `(0, 0)` because the menu shares the chrome's coordinates; Windows passes the
    /// negated panel origin so the same geometry lands at the popup window's own origin.
    pub fn paint(&self, scene: &mut Scene, text: &mut TextSystem, scale: f64, offset: (f32, f32)) {
        let edge = scale.max(1.0) as f32;
        paint_rects(
            scene,
            [
                rect(self.rect, BORDER, offset),
                RenderRect::new(
                    self.rect.x as f32 + offset.0 + edge,
                    self.rect.y as f32 + offset.1 + edge,
                    (self.rect.width as f32 - edge * 2.0).max(0.0),
                    (self.rect.height as f32 - edge * 2.0).max(0.0),
                    SURFACE,
                    1.0,
                ),
            ],
        );

        let label_offset =
            self.row_height.saturating_sub((LABEL_HEIGHT_LOGICAL * scale).round() as u32) / 2;
        let inset = f64::from(self.padding.width);
        for (index, row) in self.rows() {
            if self.selected == Some(index) {
                // Inset by the border so the highlight sits inside the panel rather than over it.
                paint_rects(
                    scene,
                    [RenderRect::new(
                        row.x as f32 + offset.0 + edge,
                        row.y as f32 + offset.1,
                        (row.width as f32 - edge * 2.0).max(0.0),
                        row.height as f32,
                        ACTIVE,
                        1.0,
                    )],
                );
            }
            let Some(entry) = self.entries.get(index) else { continue };
            let Some(clip) = label_clip(row, self.padding.width, offset) else { continue };
            scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
            text.paint_text(
                scene,
                &ellipsize(&entry.label, clip.width(), scale),
                (
                    row.x as f32 + offset.0 + inset as f32,
                    row.y as f32 + offset.1 + label_offset as f32,
                ),
                TEXT,
                false,
            );
            scene.pop_layer();
        }
    }

    /// The rectangle of one row, or `None` when the panel was clamped short of it.
    fn row(&self, index: usize) -> Option<PhysicalRect> {
        let offset = self
            .row_height
            .checked_mul(u32::try_from(index).ok()?)?
            .checked_add(self.padding.height)?;
        let bottom = offset.checked_add(self.row_height)?;
        if bottom.saturating_add(self.padding.height) > self.rect.height {
            return None;
        }
        Some(PhysicalRect {
            x: self.rect.x,
            y: self.rect.y.saturating_add(i32::try_from(offset).ok()?),
            width: self.rect.width,
            height: self.row_height,
        })
    }
}

fn label_clip(row: PhysicalRect, padding: u32, offset: (f32, f32)) -> Option<Rect> {
    let width = row.width.checked_sub(padding.saturating_mul(2))?;
    (width > 0).then(|| {
        let x = f64::from(row.x) + f64::from(offset.0) + f64::from(padding);
        let y = f64::from(row.y) + f64::from(offset.1);
        Rect::new(x, y, x + f64::from(width), y + f64::from(row.height))
    })
}

fn rect(
    rectangle: PhysicalRect,
    color: crate::display::color::Rgb,
    offset: (f32, f32),
) -> RenderRect {
    RenderRect::new(
        rectangle.x as f32 + offset.0,
        rectangle.y as f32 + offset.1,
        rectangle.width as f32,
        rectangle.height as f32,
        color,
        1.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::font::Font;

    fn entries(count: usize) -> Vec<LaunchEntry> {
        (0..count)
            .map(|index| LaunchEntry {
                label: format!("entry {index}"),
                action: LaunchAction::NewTab(None),
            })
            .collect()
    }

    fn menu(count: usize, anchor: PhysicalRect, bounds: PhysicalSize<u32>) -> NewTabMenu {
        let mut text = TextSystem::new(Font::default());
        NewTabMenu::new(entries(count), anchor, bounds, &mut text, 1.0).unwrap()
    }

    fn anchor(x: i32) -> PhysicalRect {
        PhysicalRect { x, y: 0, width: 36, height: 35 }
    }

    #[test]
    fn the_panel_hangs_under_the_button_and_right_aligns_with_it() {
        let menu = menu(2, anchor(400), PhysicalSize::new(800, 600));

        assert_eq!(menu.rect().y, 35);
        assert_eq!(menu.rect().right(), 436);
        assert_eq!(menu.rect().height, ROW_LOGICAL as u32 * 2 + PADDING_Y_LOGICAL as u32 * 2);
        assert!(menu.rect().width >= MIN_WIDTH_LOGICAL as u32);
    }

    #[test]
    fn the_panel_is_at_least_as_wide_as_its_widest_label() {
        let mut text = TextSystem::new(Font::default());
        let long = vec![LaunchEntry {
            label: "WSL: a-very-long-distribution-name".to_owned(),
            action: LaunchAction::NewWindow,
        }];
        let widest = text.measure_text(&long[0].label, false).ceil() as u32;
        let menu = NewTabMenu::new(long, anchor(700), PhysicalSize::new(1200, 600), &mut text, 1.0)
            .unwrap();

        assert!(menu.rect().width >= widest);
        assert!(menu.rect().width <= (MAX_WIDTH_LOGICAL as u32).max(widest));
    }

    #[test]
    fn a_panel_which_would_overhang_slides_back_inside_the_window() {
        let overhanging = menu(2, anchor(770), PhysicalSize::new(800, 600));

        assert!(overhanging.rect().x >= 0);
        assert_eq!(overhanging.rect().right(), 800);

        let narrow = menu(2, anchor(90), PhysicalSize::new(100, 600));
        assert_eq!(narrow.rect().x, 0);
        assert_eq!(narrow.rect().width, 100);
    }

    #[test]
    fn hit_testing_maps_points_to_rows_and_rejects_the_outside() {
        let menu = menu(3, anchor(400), PhysicalSize::new(800, 600));
        let x = f64::from(menu.rect().x) + 4.0;

        assert_eq!(menu.hit(x, 40.0), Some(0));
        assert_eq!(menu.hit(x, 66.0), Some(1));
        assert_eq!(menu.hit(x, 92.0), Some(2));
        // The padding above the first row and below the last belongs to no entry.
        assert_eq!(menu.hit(x, 36.0), None);
        assert_eq!(menu.hit(x, 119.0), None);
        assert_eq!(menu.hit(f64::from(menu.rect().x) - 1.0, 40.0), None);
        assert_eq!(menu.hit(x, 20.0), None);
    }

    #[test]
    fn a_panel_clamped_by_the_window_drops_the_rows_which_do_not_fit() {
        let menu = menu(6, anchor(400), PhysicalSize::new(800, 100));

        assert_eq!(menu.rect().height, 65);
        assert_eq!(menu.rows().count(), 2);
        assert_eq!(menu.hit(f64::from(menu.rect().x) + 4.0, 95.0), None);
    }

    #[test]
    fn a_window_with_no_room_below_the_button_opens_no_menu() {
        let mut text = TextSystem::new(Font::default());

        assert!(
            NewTabMenu::new(entries(2), anchor(400), PhysicalSize::new(800, 35), &mut text, 1.0)
                .is_none()
        );
        assert!(
            NewTabMenu::new(Vec::new(), anchor(400), PhysicalSize::new(800, 600), &mut text, 1.0)
                .is_none()
        );
    }

    #[test]
    fn hover_reports_only_real_changes() {
        let mut menu = menu(2, anchor(400), PhysicalSize::new(800, 600));
        let x = f64::from(menu.rect().x) + 4.0;

        assert!(menu.hover(x, 40.0));
        assert!(!menu.hover(x, 42.0));
        assert!(menu.hover(x, 66.0));
        assert!(menu.hover(0.0, 0.0));
        assert_eq!(menu.selected(), None);
    }

    #[test]
    fn keyboard_motion_enters_at_the_near_end_and_wraps() {
        let mut downwards = menu(3, anchor(400), PhysicalSize::new(800, 600));
        assert!(downwards.move_selection(1));
        assert_eq!(downwards.selected, Some(0));
        downwards.move_selection(-1);
        assert_eq!(downwards.selected, Some(2));
        downwards.move_selection(1);
        assert_eq!(downwards.selected, Some(0));

        let mut upwards = menu(3, anchor(400), PhysicalSize::new(800, 600));
        upwards.move_selection(-1);
        assert_eq!(upwards.selected, Some(2));
    }
}
