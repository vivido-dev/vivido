//! Overlay scrollbar for the terminal scrollback.
//!
//! The scrollbar is host chrome drawn into the same Vello scene as the message bar and the
//! visual bell. It is a pure projection of the terminal grid: [`ScrollbarState::observe`]
//! samples a [`ScrollbarModel`] once per frame and treats any `display_offset` change as a
//! reason to show itself, then lingers and fades like an overlay scroller. Nothing scrolls
//! from inside this module; drags ask the terminal to move its viewport, and the next
//! frame's projection moves the thumb.
//!
//! The alternate screen has no history, so it never produces an applicable model: full-screen
//! programs like vim never see the scrollbar.

use std::time::{Duration, Instant};

use crate::display::SizeInfo;

/// Width of the scrollbar gutter, in logical pixels.
pub const GUTTER_WIDTH: f32 = 8.;
/// Width reserved inside the terminal's right edge for scrollbar interaction.
///
/// The hit surface extends through any right padding to the window edge, while the painted gutter
/// remains [`GUTTER_WIDTH`] wide.
const INTERACTION_WIDTH: f32 = 16.;
/// Gap between the gutter edge and the thumb, in logical pixels.
const TRACK_INSET: f32 = 1.;
/// Shortest the thumb may get, in logical pixels.
pub const MIN_THUMB_HEIGHT: f32 = 28.;
/// How long the scrollbar stays fully visible after its last interaction.
const VISIBLE_LINGER: Duration = Duration::from_millis(700);
/// How long fading from full visibility to hidden takes after the linger.
const FADE_DURATION: Duration = Duration::from_millis(300);
/// Thumb color: a neutral gray that reads over both light and dark terminals.
pub const THUMB_COLOR: (u8, u8, u8) = (140, 140, 140);
/// Thumb alpha at full visibility.
pub const THUMB_ALPHA: f32 = 0.55;

/// Everything the scrollbar needs to know about the terminal, sampled once per frame.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ScrollbarModel {
    /// Whether the scrollbar may show at all: primary screen with history, config enabled.
    pub applicable: bool,
    /// Viewport offset into the history; 0 is the bottom, `history` the very top.
    pub display_offset: usize,
    /// Lines of scrollback above the viewport.
    pub history: usize,
    /// Lines visible in the viewport.
    pub screen_lines: usize,
}

/// Scrollbar geometry in physical pixels.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ScrollbarGeometry {
    /// The narrow gutter which contains the painted thumb.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// The wider, invisible hover and drag surface extending to the window's right edge.
    pub interaction_x: f32,
    pub interaction_width: f32,
    /// The thumb inside the gutter; `None` when there is nothing to scroll.
    pub thumb: Option<ThumbGeometry>,
}

/// Thumb position in physical pixels.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ThumbGeometry {
    pub x: f32,
    pub width: f32,
    pub top: f32,
    pub height: f32,
}

impl ScrollbarGeometry {
    /// Whether a physical-pixel point lies over the invisible interaction surface.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.interaction_x
            && x <= self.interaction_x + self.interaction_width
            && y >= self.y
            && y <= self.y + self.height
    }
}

/// Overlay scrollbar state: visibility timing, hover, and an in-flight drag.
pub struct ScrollbarState {
    model: ScrollbarModel,
    hovering: bool,
    dragging: bool,
    /// Distance from the pointer to the thumb top, captured when a drag starts.
    drag_anchor: Option<f32>,
    /// When the current show started; `None` while hidden.
    visible_since: Option<Instant>,
    /// The `(intensity, top, height)` visual baked into the cached scene, if it shows one.
    drawn: Option<(f32, f32, f32)>,
}

impl ScrollbarState {
    pub fn new() -> Self {
        Self {
            model: ScrollbarModel::default(),
            hovering: false,
            dragging: false,
            drag_anchor: None,
            visible_since: None,
            drawn: None,
        }
    }

    /// Sample the terminal projection for this frame.
    ///
    /// Any display-offset change — wheel, keyboard, search jumps, this scrollbar's own drags
    /// — restarts visibility. Losing applicability (alternate screen, emptied history,
    /// config off) hides immediately, without fading.
    pub fn observe(&mut self, now: Instant, model: ScrollbarModel) {
        if !model.applicable {
            self.hide();
        } else if model.display_offset != self.model.display_offset {
            self.visible_since = Some(now);
        }
        self.model = model;
    }

    /// Hide immediately: no linger, no fade.
    pub fn hide(&mut self) {
        self.hovering = false;
        self.dragging = false;
        self.drag_anchor = None;
        self.visible_since = None;
    }

    /// Visibility from 0. (hidden) to 1. (fully visible).
    pub fn intensity_at(&self, now: Instant) -> f32 {
        if self.dragging || self.hovering {
            return 1.;
        }
        let Some(shown_at) = self.visible_since else {
            return 0.;
        };
        let elapsed = now.duration_since(shown_at);
        if elapsed <= VISIBLE_LINGER {
            1.
        } else if elapsed <= VISIBLE_LINGER + FADE_DURATION {
            1. - (elapsed - VISIBLE_LINGER).as_secs_f32() / FADE_DURATION.as_secs_f32()
        } else {
            0.
        }
    }

    /// Whether the scrollbar will look different later, so redraws keep coming.
    pub fn is_animating_at(&self, now: Instant) -> bool {
        if self.dragging || self.hovering {
            // Pinned at full visibility by an interaction; the next change comes from input.
            return false;
        }
        self.visible_since
            .is_some_and(|shown_at| now.duration_since(shown_at) < VISIBLE_LINGER + FADE_DURATION)
    }

    /// Compute the scrollbar geometry in physical pixels.
    pub fn geometry(&self, size_info: &SizeInfo, scale_factor: f32) -> ScrollbarGeometry {
        let gutter = GUTTER_WIDTH * scale_factor;
        let terminal_right = size_info.width() - size_info.padding_x();
        let interaction_x = (terminal_right - INTERACTION_WIDTH * scale_factor).max(0.);
        ScrollbarGeometry {
            x: terminal_right - gutter,
            y: size_info.padding_y(),
            width: gutter,
            height: size_info.cell_height() * self.model.screen_lines as f32,
            interaction_x,
            interaction_width: size_info.width() - interaction_x,
            thumb: self.thumb_geometry(size_info, scale_factor),
        }
    }

    fn thumb_geometry(&self, size_info: &SizeInfo, scale_factor: f32) -> Option<ThumbGeometry> {
        let model = self.model;
        if !model.applicable || model.history == 0 || model.screen_lines == 0 {
            return None;
        }
        let track_top = size_info.padding_y();
        let track_height = size_info.cell_height() * model.screen_lines as f32;
        if track_height <= 0. {
            return None;
        }

        // The thumb covers the share of the document the viewport shows, never shorter than
        // the minimum when the track can hold it, and never taller than the track. Windows can
        // briefly report a one-row surface while restoring a minimized window.
        let total = (model.history + model.screen_lines) as f32;
        let proportional = model.screen_lines as f32 / total * track_height;
        let minimum = (MIN_THUMB_HEIGHT * scale_factor).min(track_height);
        let height = proportional.clamp(minimum, track_height);

        // display_offset counts up from the bottom, so the thumb rides it inverted.
        let scrollable = track_height - height;
        let from_top = model.history - model.display_offset.min(model.history);
        let top = track_top + from_top as f32 / model.history as f32 * scrollable;

        let inset = TRACK_INSET * scale_factor;
        let gutter = GUTTER_WIDTH * scale_factor;
        Some(ThumbGeometry {
            x: size_info.width() - size_info.padding_x() - gutter + inset,
            width: (gutter - 2. * inset).max(1.),
            top,
            height,
        })
    }

    /// The scrollbar's visual for a frame: `(intensity, thumb_top, thumb_height)`, or `None`
    /// when nothing should be drawn.
    pub fn visual_at(
        &self,
        now: Instant,
        size_info: &SizeInfo,
        scale_factor: f32,
    ) -> Option<(f32, f32, f32)> {
        let intensity = self.intensity_at(now);
        let thumb = self.thumb_geometry(size_info, scale_factor)?;
        (intensity > 0.).then_some((intensity, thumb.top, thumb.height))
    }

    /// The visual baked into the cached scene, for scene-reuse comparison.
    pub fn drawn_visual(&self) -> Option<(f32, f32, f32)> {
        self.drawn
    }

    /// Record the visual the just-built scene contains.
    pub fn record_drawn(&mut self, visual: Option<(f32, f32, f32)>) {
        self.drawn = visual;
    }

    /// Whether the scrollbar may show at all right now.
    pub fn applicable(&self) -> bool {
        self.model.applicable
    }

    /// Whether a drag owns the pointer.
    pub fn dragging(&self) -> bool {
        self.dragging
    }

    /// Whether a physical-pixel point is over the scrollbar interaction surface.
    pub fn contains_point(&self, size_info: &SizeInfo, scale_factor: f32, x: f32, y: f32) -> bool {
        self.model.applicable && self.geometry(size_info, scale_factor).contains(x, y)
    }

    /// Update hover from a pointer move; returns whether visibility changed.
    ///
    /// Both edges restart the linger: entering reveals, and leaving fades back out even if
    /// the previous show had already expired.
    pub fn set_hover(&mut self, now: Instant, inside: bool) -> bool {
        if !self.model.applicable {
            self.hovering = false;
            return false;
        }
        if inside == self.hovering {
            return false;
        }
        self.hovering = inside;
        self.visible_since = Some(now);
        true
    }

    /// Start dragging at physical-pixel `y`. Returns `false` when there is no thumb.
    pub fn begin_drag(
        &mut self,
        now: Instant,
        size_info: &SizeInfo,
        scale_factor: f32,
        y: f32,
    ) -> bool {
        let Some(thumb) = self.thumb_geometry(size_info, scale_factor) else {
            return false;
        };
        self.drag_anchor = Some(y - thumb.top);
        self.dragging = true;
        self.visible_since = Some(now);
        true
    }

    /// Map a physical-pixel pointer `y` to the display offset a drag should land on.
    ///
    /// The result is line-granular (rounded to whole lines). `None` means no drag is in
    /// flight or there is nothing scrollable.
    pub fn drag_target(&self, size_info: &SizeInfo, scale_factor: f32, y: f32) -> Option<usize> {
        let model = self.model;
        if !model.applicable || model.history == 0 {
            return None;
        }
        let anchor = self.drag_anchor?;
        let thumb = self.thumb_geometry(size_info, scale_factor)?;
        let scrollable = size_info.cell_height() * model.screen_lines as f32 - thumb.height;
        if scrollable <= 0. {
            return None;
        }

        // Keep the grab point at its captured distance from the thumb top, clamped to the
        // track; the thumb top's fraction of the scrollable range is the scrolled fraction.
        let track_top = size_info.padding_y();
        let thumb_top = (y - anchor - track_top).clamp(0., scrollable);
        let fraction = thumb_top / scrollable;
        let lines = (fraction * model.history as f32).round() as usize;
        Some(model.history - lines.min(model.history))
    }

    /// End a drag, leaving the scrollbar visible to linger out.
    pub fn end_drag(&mut self, now: Instant) {
        self.dragging = false;
        self.drag_anchor = None;
        self.visible_since = Some(now);
    }
}

impl Default for ScrollbarState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 30 rows of 10x20 cells inside 4px padding: a 600px track starting at y=4.
    fn size_info() -> SizeInfo {
        SizeInfo::new(400., 608., 10., 20., 4., 4., false)
    }

    fn model(history: usize, offset: usize) -> ScrollbarModel {
        ScrollbarModel {
            applicable: history > 0,
            display_offset: offset,
            history,
            screen_lines: 30,
        }
    }

    fn thumb(state: &ScrollbarState) -> ThumbGeometry {
        state.geometry(&size_info(), 1.).thumb.expect("applicable model has a thumb")
    }

    #[test]
    fn thumb_height_is_proportional() {
        let mut state = ScrollbarState::new();
        state.observe(Instant::now(), model(90, 0));
        // Viewport covers 30 of 120 lines: a quarter of the 600px track.
        assert!((thumb(&state).height - 150.).abs() < 1e-4);
    }

    #[test]
    fn thumb_height_never_drops_below_minimum() {
        let mut state = ScrollbarState::new();
        state.observe(Instant::now(), model(10_000, 0));
        assert!((thumb(&state).height - MIN_THUMB_HEIGHT).abs() < 1e-4);
    }

    #[test]
    fn thumb_height_never_exceeds_track() {
        let mut state = ScrollbarState::new();
        state.observe(Instant::now(), model(1, 0));
        assert!(thumb(&state).height <= 600.);
    }

    #[test]
    fn thumb_shrinks_to_fit_restore_sized_track() {
        let size = SizeInfo::new(400., 24., 10., 24., 0., 0., false);
        let mut state = ScrollbarState::new();
        state.observe(
            Instant::now(),
            ScrollbarModel { applicable: true, display_offset: 0, history: 100, screen_lines: 1 },
        );

        let thumb = state
            .geometry(&size, 1.25)
            .thumb
            .expect("a one-row track with history still has a thumb");
        assert!((thumb.height - 24.).abs() < 1e-4);
    }

    #[test]
    fn thumb_sits_at_bottom_and_top_for_extreme_offsets() {
        let mut state = ScrollbarState::new();
        state.observe(Instant::now(), model(100, 0));
        let at_bottom = thumb(&state);
        assert!((at_bottom.top + at_bottom.height - 604.).abs() < 1e-4); // track_top + track

        state.observe(Instant::now(), model(100, 100));
        assert!((thumb(&state).top - 4.).abs() < 1e-4); // track_top
    }

    #[test]
    fn thumb_moves_up_as_offset_grows() {
        let mut state = ScrollbarState::new();
        state.observe(Instant::now(), model(100, 0));
        let bottom = thumb(&state).top;
        state.observe(Instant::now(), model(100, 50));
        assert!(thumb(&state).top < bottom);
        state.observe(Instant::now(), model(100, 100));
        assert!(thumb(&state).top < bottom);
    }

    #[test]
    fn drag_round_trips_thumb_position() {
        let size = size_info();
        let mut state = ScrollbarState::new();
        for offset in [0, 1, 25, 50, 99, 100] {
            state.observe(Instant::now(), model(100, offset));
            let t = thumb(&state);
            // Grab the thumb top, then drag to that same place: no movement expected.
            assert!(state.begin_drag(Instant::now(), &size, 1., t.top));
            assert_eq!(state.drag_target(&size, 1., t.top), Some(offset));
            // Dragging to the track top and bottom lands on the extreme offsets.
            assert_eq!(state.drag_target(&size, 1., size.padding_y()), Some(100));
            assert_eq!(state.drag_target(&size, 1., size.padding_y() + 600.), Some(0));
            state.end_drag(Instant::now());
        }
    }

    #[test]
    fn drag_target_is_line_granular() {
        let size = size_info();
        let mut state = ScrollbarState::new();
        state.observe(Instant::now(), model(100, 50));
        let t = thumb(&state);
        state.begin_drag(Instant::now(), &size, 1., t.top);

        // Small pointer moves inside one line's share of the track round to the same offset.
        let nearby = t.top + 0.5;
        assert_eq!(state.drag_target(&size, 1., nearby), state.drag_target(&size, 1., t.top));

        // Monotonic: dragging up never scrolls down.
        let mut last = state.drag_target(&size, 1., size.padding_y() + 600.).unwrap();
        for step in 1..=20 {
            let y = size.padding_y() + 600. - step as f32 * 30.;
            let target = state.drag_target(&size, 1., y).unwrap();
            assert!(target >= last);
            last = target;
        }
    }

    #[test]
    fn no_thumb_without_history() {
        let mut state = ScrollbarState::new();
        state.observe(Instant::now(), model(0, 0));
        assert_eq!(state.geometry(&size_info(), 1.).thumb, None);
        assert_eq!(state.visual_at(Instant::now(), &size_info(), 1.), None);
    }

    #[test]
    fn gutter_hit_test_requires_applicability() {
        let size = size_info();
        let mut state = ScrollbarState::new();
        state.observe(Instant::now(), model(100, 0));
        let geometry = state.geometry(&size, 1.);
        assert!(state.contains_point(&size, 1., geometry.x + 1., geometry.y + 10.));

        state.observe(Instant::now(), ScrollbarModel { applicable: false, ..model(100, 0) });
        assert!(!state.contains_point(&size, 1., geometry.x + 1., geometry.y + 10.));
    }

    #[test]
    fn interaction_surface_is_wider_than_the_painted_gutter() {
        let size = size_info();
        let mut state = ScrollbarState::new();
        state.observe(Instant::now(), model(100, 0));
        let geometry = state.geometry(&size, 1.);

        assert_eq!(geometry.width, GUTTER_WIDTH);
        assert_eq!(geometry.interaction_x, 380.);
        assert_eq!(geometry.interaction_width, 20.);
        assert!(geometry.interaction_x < geometry.x);
        assert!(geometry.contains(geometry.x - 1., geometry.y + 10.));
        assert!(geometry.contains(size.width() - 1., geometry.y + 10.));
        assert!(!geometry.contains(geometry.interaction_x - 1., geometry.y + 10.));
    }

    #[test]
    fn hidden_until_scrolled_or_hovered() {
        let now = Instant::now();
        let mut state = ScrollbarState::new();
        state.observe(now, model(100, 0));
        // Staying at the bottom is not scrolling: no show.
        assert_eq!(state.intensity_at(now + FADE_DURATION * 10), 0.);
        // History growing alone does not wake it either.
        state.observe(now, model(200, 0));
        assert_eq!(state.intensity_at(now + FADE_DURATION * 10), 0.);
    }

    #[test]
    fn scrolling_wakes_then_lingers_then_fades() {
        let now = Instant::now();
        let mut state = ScrollbarState::new();
        state.observe(now, model(100, 0));
        state.observe(now, model(100, 40));
        assert_eq!(state.intensity_at(now), 1.);
        assert_eq!(state.intensity_at(now + Duration::from_millis(600)), 1.);
        assert!((state.intensity_at(now + Duration::from_millis(850)) - 0.5).abs() < 1e-3);
        assert_eq!(state.intensity_at(now + Duration::from_millis(1000)), 0.);
        assert!(!state.is_animating_at(now + Duration::from_millis(1001)));
        assert!(state.is_animating_at(now + Duration::from_millis(999)));
    }

    #[test]
    fn hover_reveals_and_pins_visibility() {
        let now = Instant::now();
        let mut state = ScrollbarState::new();
        state.observe(now, model(100, 0));
        assert!(state.set_hover(now, true));
        assert_eq!(state.intensity_at(now + FADE_DURATION * 10), 1.);
        assert!(!state.is_animating_at(now + FADE_DURATION * 10));
        // Leaving restarts the linger instead of vanishing.
        assert!(state.set_hover(now + FADE_DURATION * 10, false));
        assert_eq!(state.intensity_at(now + FADE_DURATION * 10 + VISIBLE_LINGER / 2), 1.);
    }

    #[test]
    fn drag_pins_visibility_until_released() {
        let size = size_info();
        let now = Instant::now();
        let mut state = ScrollbarState::new();
        state.observe(now, model(100, 0));
        let t = thumb(&state);
        assert!(state.begin_drag(now, &size, 1., t.top));
        assert_eq!(state.intensity_at(now + FADE_DURATION * 10), 1.);
        state.end_drag(now);
        assert!(!state.dragging());
        assert_eq!(state.intensity_at(now + VISIBLE_LINGER / 2), 1.);
    }

    #[test]
    fn inapplicable_hides_without_fading() {
        let now = Instant::now();
        let mut state = ScrollbarState::new();
        state.observe(now, model(100, 0));
        state.observe(now, model(100, 40));
        assert_eq!(state.intensity_at(now), 1.);
        // The alternate screen takes over mid-linger.
        state.observe(now + Duration::from_millis(100), model(0, 0));
        assert_eq!(state.intensity_at(now + Duration::from_millis(101)), 0.);
        assert!(!state.is_animating_at(now + Duration::from_millis(101)));
    }

    #[test]
    fn recorded_visual_drives_scene_reuse() {
        let size = size_info();
        let now = Instant::now();
        let mut state = ScrollbarState::new();
        assert_eq!(state.drawn_visual(), None);

        state.observe(now, model(100, 20));
        state.set_hover(now, true);
        let visual = state.visual_at(now + Duration::from_millis(10), &size, 1.);
        assert_eq!(
            visual.map(|(_, top, height)| (top, height)),
            Some((thumb(&state).top, thumb(&state).height))
        );
        // A cached scene without the scrollbar cannot be reused once it should show.
        assert_ne!(state.drawn_visual(), visual);
        state.record_drawn(visual);
        // While hover pins the intensity, the same visual keeps matching.
        assert_eq!(
            state.drawn_visual(),
            state.visual_at(now + Duration::from_millis(20), &size, 1.)
        );
    }
}
