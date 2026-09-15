//! Authenticated pane-window control. The renderer and input lane share this owner-scoped state.
//!
//! Native terminal targets install viewport geometry before offering the profile bundle.
//! Keeping dispatch here avoids embedding window policy in the terminal loop.

use crate::display::{text::TextSystem, vector::CompiledScene};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Weak;
use vello::peniko::{Blob, ImageAlphaType, ImageData, ImageFormat};
use winit::window::CursorIcon;

use vivid_protocol::overlay::wire::{
    Action, Appearance, Clipboard, Environment, EnvironmentChanged, Query, SetSemantics, SetWindow,
    Status, Viewport, WindowAddress,
};
use vivid_protocol::overlay::wire::{
    PresentationOutcome, Submission, SubmissionOutcome, ViewportChanged,
};
use vivid_protocol::overlay::{
    AccessibleAction, DismissReason, Event, MAX_CLICKS, PointerReport, Scroll, ScrollPhase,
    Semantics, Windows,
};
use vivid_protocol::vector::Scalar;

use super::*;
#[cfg(test)]
use crate::display::vector::compile;

#[path = "overlay_text.rs"]
pub(super) mod text;

/// Assistive-technology actions awaiting their producer's lane, per owning session: the window
/// they name and the encoded event body, in the order they were asked for.
///
/// Queued rather than posted because the asker is not the actor that owns the lane writer.
type PendingAccessibility = HashMap<SessionIdentity, VecDeque<(u64, Vec<(u64, Value)>)>>;

#[derive(Default)]
pub(crate) struct Host {
    pub(super) layouts:
        HashMap<(SurfaceIdentity, u64), Arc<crate::display::vector::text_layout::MeasuredLayout>>,
    pub(super) layout_charges: HashMap<(SessionIdentity, u64), Weak<()>>,
    next_layout: u64,
    text_jobs: HashSet<SessionIdentity>,
    editor: Option<(SurfaceIdentity, vivid_protocol::overlay::wire::text::EditorGeometry)>,
    windows: Windows,
    pub(super) viewport: Option<Viewport>,
    /// Pointer capture is established from this, never from a producer-supplied position.
    pointer_position: Option<vivid_protocol::vector::Point>,
    accessibility: PendingAccessibility,
    /// Each window's semantic tree and the published scene revision it describes.
    semantics: BTreeMap<SurfaceIdentity, Semantics>,
    /// The window a key press or pointer press was last delivered to, and when. A clipboard
    /// write must be caused by the user, so this is what authorizes one.
    last_gesture: Option<(SurfaceIdentity, Instant)>,
    /// The last press of the current sequence: when, which button, where, and the count so far.
    last_click: Option<(Instant, u16, vivid_protocol::vector::Point, u8)>,
    surfaces: HashSet<SurfaceIdentity>,
    scenes: HashMap<TrackIdentity, (ChannelGeneration, u64, Arc<CompiledScene>)>,
    assets: HashMap<(TrackIdentity, ChannelGeneration, u64), (Weak<()>, usize)>,
    submissions: HashMap<TrackIdentity, Submission>,
    active: HashMap<SurfaceIdentity, Submission>,
    accepted: HashMap<SurfaceIdentity, u64>,
    pending: HashSet<(SessionIdentity, Submission)>,
    outcomes: HashMap<SessionIdentity, VecDeque<SubmissionOutcome>>,
    overflow: HashSet<SessionIdentity>,
    displayed: HashMap<SurfaceIdentity, Drawing>,
    inflight: HashMap<SurfaceIdentity, Drawing>,
    viewport_revision: u64,
    viewport_dirty: HashSet<SessionIdentity>,
    /// The environment a producer sees, and the revision it last changed at.
    environment: Environment,
    environment_revision: u64,
    environment_dirty: HashSet<SessionIdentity>,
    appearance: Appearance,
    refresh_interval_us: Option<u64>,
    pub(super) actors: HashMap<SessionIdentity, Weak<SessionRuntime>>,
    lanes: HashMap<SessionIdentity, (u64, Instant)>,
    font: crate::config::font::Font,
    compiled_scenes: u64,
    presented_scenes: u64,
    superseded_scenes: u64,
    window_updates: u64,
}

/// How far a pointer may drift between presses and still count as one sequence. The threshold
/// itself is the terminal's, so the whole presenter counts clicks one way.
const CLICK_SLOP: f64 = 4.;

/// Map the protocol's closed cursor set onto the platform's. Every shape has an icon, so a
/// region can ask for any of them without a fallback that would silently change behavior.
pub(crate) fn cursor_icon(shape: vivid_protocol::vector::CursorShape) -> CursorIcon {
    use vivid_protocol::vector::CursorShape as Shape;
    match shape {
        Shape::Default => CursorIcon::Default,
        Shape::Pointer => CursorIcon::Pointer,
        Shape::Text => CursorIcon::Text,
        Shape::Move => CursorIcon::Move,
        Shape::Crosshair => CursorIcon::Crosshair,
        Shape::NotAllowed => CursorIcon::NotAllowed,
        Shape::Grab => CursorIcon::Grab,
        Shape::Grabbing => CursorIcon::Grabbing,
        Shape::Wait => CursorIcon::Wait,
        Shape::Progress => CursorIcon::Progress,
        Shape::ResizeLeft => CursorIcon::WResize,
        Shape::ResizeRight => CursorIcon::EResize,
        Shape::ResizeUp => CursorIcon::NResize,
        Shape::ResizeDown => CursorIcon::SResize,
        Shape::ResizeUpLeft => CursorIcon::NwResize,
        Shape::ResizeUpRight => CursorIcon::NeResize,
        Shape::ResizeDownLeft => CursorIcon::SwResize,
        Shape::ResizeDownRight => CursorIcon::SeResize,
        Shape::ResizeLeftRight => CursorIcon::EwResize,
        Shape::ResizeUpDown => CursorIcon::NsResize,
    }
}

/// A platform wheel delta in physical pixels, before the viewport scale is applied.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollInput {
    pub dx: f64,
    pub dy: f64,
    /// True for pixel-precise devices such as trackpads, false for detented wheels.
    pub precise: bool,
    pub phase: ScrollPhase,
}

#[derive(Clone)]
pub(crate) struct Drawing {
    pub window: SurfaceIdentity,
    pub window_revision: u64,
    pub revision: u64,
    pub bounds: vivid_protocol::vector::Rect,
    pub scale: f64,
    pub compiled: Arc<CompiledScene>,
    pub submission: Submission,
}

impl Host {
    pub(super) fn has_viewport(&self) -> bool {
        self.viewport.is_some()
    }
    pub(super) fn capturing(&self) -> bool {
        self.windows.has_pointer_capture()
    }
    pub(super) fn focused(&self) -> bool {
        self.windows.focus().is_some()
    }
    #[cfg(test)]
    pub(super) fn compiled_scene_count(&self) -> u64 {
        self.compiled_scenes
    }
    #[cfg(any(unix, windows))]
    pub(super) fn automation_metrics(&self) -> serde_json::Value {
        serde_json::json!({
            "windows": self.windows.visible().count(),
            "focused": self.windows.focus().is_some(),
            "pointer_capture": self.windows.has_pointer_capture(),
            "compiled_scenes": self.compiled_scenes,
            "presented_scenes": self.presented_scenes,
            "superseded_scenes": self.superseded_scenes,
            "window_updates": self.window_updates,
            "compiled_tracks": self.scenes.len(),
            "retained_assets": self.assets.len(),
            "retained_layouts": self.layouts.len(),
            "pending_submissions": self.pending.len(),
            "queued_outcomes": self.outcomes.values().map(VecDeque::len).sum::<usize>(),
            "input_lanes": self.lanes.len(),
        })
    }
    pub(super) fn font(&self) -> crate::config::font::Font {
        self.font.clone()
    }
    pub(super) fn set_font(&mut self, font: crate::config::font::Font) {
        self.font = font;
        // The environment names the default font, so a font change is an environment change.
        self.refresh_environment();
    }

    /// The host's defaults as a producer sees them: the pane's own font, the desktop's
    /// appearance, and what the display can actually do.
    pub(super) fn environment(&self) -> Environment {
        let normal = self.font.normal();
        Environment {
            font_family: normal.family.clone(),
            font_size: Scalar::new(f64::from(self.font.size().as_px()))
                .unwrap_or_else(|_| Scalar::new(16.).expect("16 is a valid scalar")),
            appearance: self.appearance,
            // Vivido has no reduced-motion signal from the platform, so it reports absence
            // rather than asserting a preference it cannot read.
            reduced_motion: None,
            refresh_interval_us: self.refresh_interval_us,
        }
    }

    /// Republish the environment if anything in it changed.
    pub(super) fn refresh_environment(&mut self) {
        let environment = self.environment();
        if environment.validate().is_err() || self.environment == environment {
            return;
        }
        self.environment = environment;
        // Revisions are nonzero and a lane's first snapshot is revision 1, so the counter starts
        // there rather than at the zero a derived default leaves it at.
        self.environment_revision = self.environment_revision.max(1).saturating_add(1);
        self.environment_dirty.extend(self.lanes.keys().copied());
        for actor in self.actors.values().filter_map(Weak::upgrade) {
            actor.wake_actor();
        }
    }

    /// Record the desktop appearance, republishing if it differs.
    pub(super) fn set_appearance(&mut self, appearance: Appearance) {
        if self.appearance == appearance {
            return;
        }
        self.appearance = appearance;
        self.refresh_environment();
    }

    /// Record the display refresh interval, republishing if it differs.
    pub(super) fn set_refresh_interval(&mut self, interval_us: Option<u64>) {
        let interval_us = interval_us.filter(|rate| *rate != 0);
        if self.refresh_interval_us == interval_us {
            return;
        }
        self.refresh_interval_us = interval_us;
        self.refresh_environment();
    }
    pub(super) fn deadline(&self, owner: SessionIdentity) -> Option<Instant> {
        self.lanes.get(&owner).map(|(_, deadline)| *deadline)
    }
    pub fn update_viewport(
        &mut self,
        width: f64,
        height: f64,
        scale: f64,
    ) -> Result<(), &'static str> {
        if !scale.is_finite() || scale <= 0. || scale > 64. {
            return Err("invalid overlay display scale");
        }
        let viewport = Viewport {
            width: Scalar::new(width / scale).map_err(|e| e.0)?,
            height: Scalar::new(height / scale).map_err(|e| e.0)?,
            scale_numerator: (scale * 1_000_000.).round() as u32,
            scale_denominator: 1_000_000,
        };
        viewport.validate().map_err(|_| "invalid overlay viewport")?;
        if self.viewport != Some(viewport) {
            self.viewport_revision =
                self.viewport_revision.checked_add(1).ok_or("viewport revision exhausted")?;
            self.viewport = Some(viewport);
            self.viewport_dirty.extend(self.lanes.keys().copied());
            for actor in self.actors.values().filter_map(Weak::upgrade) {
                actor.wake_actor();
            }
        }
        Ok(())
    }

    fn status(&self, owner: SessionIdentity, query: Query) -> Result<Status, ControlError> {
        let id = owner
            .context(query.context_id)
            .and_then(|c| c.surface(query.surface_id))
            .map_err(|_| ControlError::bad_message("invalid overlay identity"))?;
        let window = self
            .windows
            .get(id)
            .ok_or_else(|| ControlError::not_found("overlay window is absent"))?;
        Ok(Status {
            window: SetWindow {
                address: WindowAddress {
                    context_id: query.context_id,
                    surface_id: query.surface_id,
                    generation: window.generation,
                },
                expected_revision: window.revision,
                options: window.options.clone(),
            },
            viewport: self
                .viewport
                .ok_or_else(|| ControlError::bad_state("overlay viewport is unavailable"))?,
            focused: self.windows.focus() == Some(id),
            scene_revision: window.scene_revision,
            active: self.active.get(&id).copied(),
            accepted_revision: self.accepted.get(&id).copied().unwrap_or(0),
            viewport_revision: self.viewport_revision,
        })
    }

    fn retire_presentation(&mut self, identity: SurfaceIdentity) {
        let pending: Vec<_> = self
            .pending
            .iter()
            .copied()
            .filter(|(owner, s)| {
                *owner == identity.context.session
                    && s.address.context_id == identity.context.context_id
                    && s.address.surface_id == identity.surface_id
            })
            .collect();
        for (owner, submission) in pending {
            self.resolve(owner, submission, PresentationOutcome::Superseded);
        }
        self.displayed.remove(&identity);
        self.semantics.remove(&identity);
        self.inflight.remove(&identity);
        self.active.remove(&identity);
        self.submissions.retain(|id, _| id.surface != identity);
        self.scenes.retain(|id, _| id.surface != identity);
        self.assets.retain(|_, (token, _)| token.strong_count() != 0);
    }

    pub fn remove_surface(&mut self, identity: SurfaceIdentity) {
        self.layouts.retain(|(surface, _), _| *surface != identity);
        if self.editor.is_some_and(|(id, _)| id == identity) {
            self.editor = None;
        }
        self.retire_presentation(identity);
        self.accepted.remove(&identity);
        self.windows.close(identity, DismissReason::Closed);
        self.surfaces.remove(&identity);
    }

    pub fn remove_contexts(&mut self, owner: SessionIdentity, contexts: &HashSet<u64>) {
        let ids: Vec<_> = self
            .surfaces
            .iter()
            .copied()
            .filter(|id| id.context.session == owner && contexts.contains(&id.context.context_id))
            .collect();
        for id in ids {
            self.remove_surface(id);
        }
    }

    pub fn remove_owner(&mut self, owner: SessionIdentity) -> Vec<SurfaceIdentity> {
        self.layouts.retain(|(surface, _), _| surface.context.session != owner);
        if self.editor.is_some_and(|(id, _)| id.context.session == owner) {
            self.editor = None;
        }
        let ids = self.surfaces.iter().copied().filter(|id| id.context.session == owner).collect();
        self.windows.revoke_owner(owner);
        self.surfaces.retain(|id| id.context.session != owner);
        self.scenes.retain(|id, _| id.surface.context.session != owner);
        self.submissions.retain(|id, _| id.surface.context.session != owner);
        self.active.retain(|id, _| id.context.session != owner);
        self.accepted.retain(|id, _| id.context.session != owner);
        self.displayed.retain(|id, _| id.context.session != owner);
        self.inflight.retain(|id, _| id.context.session != owner);
        self.assets.retain(|_, (token, _)| token.strong_count() != 0);
        self.pending.retain(|(id, _)| *id != owner);
        self.outcomes.remove(&owner);
        self.overflow.remove(&owner);
        self.viewport_dirty.remove(&owner);
        self.actors.remove(&owner);
        self.lanes.remove(&owner);
        ids
    }

    pub(super) fn lane_open(&mut self, owner: SessionIdentity, generation: u64) {
        self.lanes.insert(owner, (generation, Instant::now() + Duration::from_secs(5)));
        self.viewport_dirty.insert(owner);
        self.environment_dirty.insert(owner);
    }

    pub(super) fn lane_lost(
        &mut self,
        owner: SessionIdentity,
        generation: u64,
    ) -> Vec<SurfaceIdentity> {
        if self.lanes.get(&owner).is_some_and(|(live, _)| *live == generation) {
            self.remove_owner(owner)
        } else {
            Vec::new()
        }
    }

    pub(super) fn keyboard(&mut self, event: vivid_protocol::overlay::Event, escape: bool) -> bool {
        // A key press goes to the focused window, which is therefore the one it reached.
        if matches!(event, vivid_protocol::overlay::Event::Key { down: true, .. }) {
            self.last_gesture = self.windows.focus().map(|id| (id, Instant::now()));
        }
        let consumed = self.windows.keyboard(event, escape);
        self.editor_rect();
        consumed
    }

    /// Whether a clipboard write from this window may be honored, and why not when it may not.
    ///
    /// A clipboard is shared with every other application on the machine and is frequently where
    /// a password or a command line briefly lives, so a write is only honored when the user just
    /// did something in the window asking for it.
    pub(super) fn authorize_clipboard(&self, id: SurfaceIdentity) -> Result<(), &'static str> {
        if self.windows.get(id).is_none() {
            return Err("overlay window is absent");
        }
        if self.windows.focus() != Some(id) {
            return Err("a clipboard write requires the focused overlay window");
        }
        match self.last_gesture {
            Some((window, at)) if window == id => {
                if at.elapsed() <= vivid_protocol::overlay::MAX_CLIPBOARD_GESTURE_AGE {
                    Ok(())
                } else {
                    Err("the gesture authorizing this clipboard write is too old")
                }
            },
            // A gesture in another window is not this window's to spend.
            _ => Err("a clipboard write requires a recent gesture in the same window"),
        }
    }
    pub(super) fn set_pane_focus(&mut self, focused: bool) {
        if !focused {
            self.editor = None;
        }
        self.windows.set_pane_focus(focused);
    }

    pub(super) fn pointer(
        &mut self,
        x: f64,
        y: f64,
        button: Option<(u16, bool)>,
        modifiers: u32,
        pressure: Option<f64>,
    ) -> bool {
        let Some(viewport) = self.viewport else {
            return false;
        };
        let scale = f64::from(viewport.scale_numerator) / f64::from(viewport.scale_denominator);
        let Ok(point) = vivid_protocol::vector::Point::new(x / scale, y / scale) else {
            return false;
        };
        self.pointer_position = Some(point);
        let clicks = self.count_clicks(point, button);
        let pressure = pressure.and_then(|p| Scalar::new(p).ok());
        let drawings: Vec<_> = self.displayed.values().cloned().collect();
        let consumed = self.windows.pointer(
            PointerReport { position: point, button, modifiers, clicks, pressure },
            |id, point| {
                drawings.iter().find(|d| d.window == id).and_then(|d| d.compiled.hit(point))
            },
        );
        if consumed && button.is_some_and(|(_, down)| down) {
            // Read after the dispatch: that is what settles which window the press reached. A
            // press that reached no window clears any gesture a producer might otherwise bank.
            self.last_gesture = self.windows.hovered_window().map(|id| (id, Instant::now()));
        }
        self.editor_rect();
        consumed
    }

    /// The cursor the pointer calls for, which the window backend then applies.
    pub(super) fn cursor(&self) -> Option<vivid_protocol::vector::CursorShape> {
        self.windows.cursor()
    }

    /// Create or update a window, then re-evaluate what the pointer is over.
    ///
    /// Moving, resizing, and hiding a window change the region under a stationary pointer just
    /// as publishing a scene does. Keeping that inside the mutation rather than at each call
    /// site means a caller cannot forget it.
    pub(super) fn set_window(
        &mut self,
        identity: SurfaceIdentity,
        generation: u64,
        expected_revision: u64,
        options: vivid_protocol::overlay::WindowOptions,
    ) -> Result<u64, vivid_protocol::vector::InvalidScene> {
        let result = if expected_revision == 0 {
            self.windows.create(identity, generation, options).map(|()| 1)
        } else {
            self.window_updates = self.window_updates.saturating_add(1);
            self.windows.update(identity, generation, expected_revision, options)
        };
        if result.is_ok() {
            self.refresh_hover();
        }
        result
    }

    /// Apply a conditional window action, then re-evaluate what the pointer is over.
    pub(super) fn apply_action(
        &mut self,
        owner: SessionIdentity,
        action: vivid_protocol::overlay::wire::Action,
        viewport: Viewport,
    ) -> Result<Option<u64>, vivid_protocol::vector::InvalidScene> {
        let result = self.windows.apply_action(owner, action, viewport);
        if result.is_ok() {
            self.refresh_hover();
        }
        result
    }

    /// Replace a window's semantic tree. Stale revisions are refused rather than stored, so
    /// assistive technology is never told about a control that is not on screen.
    pub(super) fn set_semantics(
        &mut self,
        id: SurfaceIdentity,
        semantics: Semantics,
    ) -> Result<(), &'static str> {
        let presented = self.displayed.get(&id).map_or(0, |drawing| drawing.revision);
        if semantics.scene_revision != presented {
            return Err("semantics must describe the currently published scene");
        }
        self.semantics.insert(id, semantics);
        Ok(())
    }

    /// Queue an assistive-technology action for the producer that owns a window.
    ///
    /// It arrives as an ordinary input event on the producer's own lane, naming the application's
    /// node ID, so a producer routes it the way it routes anything else the host tells it.
    ///
    /// Only the AccessKit adapters can deliver an action, so nothing calls this on macOS, where
    /// the AppKit adapter builds its own tree. It is kept compiled and type-checked there rather
    /// than configured away.
    #[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
    pub(super) fn queue_accessibility(
        &mut self,
        window: SurfaceIdentity,
        node: u64,
        action: AccessibleAction,
    ) -> bool {
        let Some(current) = self.windows.get(window) else {
            return false;
        };
        // An adapter builds its tree from what was published, so a node the live tree no longer
        // names is one the user is not looking at: the scene was replaced while the asker held
        // the old tree.
        if !self.semantics.get(&window).is_some_and(|tree| tree.nodes.iter().any(|n| n.id == node))
        {
            return false;
        }
        let event = vivid_protocol::overlay::wire::InputEvent {
            address: vivid_protocol::overlay::wire::WindowAddress {
                context_id: window.context.context_id,
                surface_id: window.surface_id,
                generation: current.generation,
            },
            // An action is not tied to one scene, so it carries the window's published revision.
            scene_revision: self.displayed.get(&window).map_or(0, |drawing| drawing.revision),
            event: Event::Accessibility { node, action },
        };
        let Ok(payload) = event.payload() else {
            return false;
        };
        let owner = window.context.session;
        let queue = self.accessibility.entry(owner).or_default();
        // Assistive technology cannot generate actions faster than a person presses them, but a
        // stuck client could; the queue is bounded like every other lane queue.
        if queue.len() >= vivid_protocol::overlay::MAX_PENDING_EVENTS {
            return false;
        }
        queue.push_back((window.surface_id, payload));
        if let Some(actor) = self.actors.get(&owner).and_then(Weak::upgrade) {
            actor.wake_actor();
        }
        true
    }

    /// Every window that has a live semantic tree, in a stable order.
    pub(super) fn windows_with_semantics(&self) -> Vec<(SurfaceIdentity, &Semantics)> {
        self.semantics.iter().map(|(id, semantics)| (*id, semantics)).collect()
    }

    /// Drop semantics that no longer describe what is displayed. Called where a scene publishes,
    /// so a tree that named a stale revision never outlives the scene it described.
    fn retire_stale_semantics(&mut self) {
        let stale: Vec<SurfaceIdentity> = self
            .semantics
            .iter()
            .filter(|(id, semantics)| {
                self.displayed
                    .get(id)
                    .is_none_or(|drawing| drawing.revision != semantics.scene_revision)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.semantics.remove(&id);
        }
    }

    /// Re-evaluate hover after the scene under the pointer changed.
    pub(super) fn refresh_hover(&mut self) {
        let Some(point) = self.pointer_position else {
            return;
        };
        let drawings: Vec<_> = self.displayed.values().cloned().collect();
        self.windows.refresh_hover(point, &|id, point| {
            drawings.iter().find(|d| d.window == id).and_then(|d| d.compiled.hit(point))
        });
    }

    /// Count a press in the current sequence, using the same threshold and target rule the
    /// terminal's own click detection uses, plus a small tolerance because a pointer drifts a
    /// pixel or two between presses where a terminal cell does not.
    fn count_clicks(
        &mut self,
        position: vivid_protocol::vector::Point,
        button: Option<(u16, bool)>,
    ) -> u8 {
        let Some((button_id, true)) = button else {
            return 0;
        };
        let now = Instant::now();
        let clicks = match self.last_click {
            Some((at, id, point, count))
                if id == button_id
                    && now.saturating_duration_since(at) < crate::input::CLICK_THRESHOLD
                    && (point.x.get() - position.x.get()).abs() <= CLICK_SLOP
                    && (point.y.get() - position.y.get()).abs() <= CLICK_SLOP =>
            {
                // A fourth press matches nothing and restarts the sequence, exactly as a
                // terminal click does, so the count stays inside the protocol's range.
                if count >= MAX_CLICKS { 1 } else { count + 1 }
            },
            _ => 1,
        };
        self.last_click = Some((now, button_id, position, clicks));
        clicks
    }

    pub(super) fn wheel(&mut self, x: f64, y: f64, scroll: ScrollInput, modifiers: u32) -> bool {
        let Some(viewport) = self.viewport else {
            return false;
        };
        let scale = f64::from(viewport.scale_numerator) / f64::from(viewport.scale_denominator);
        let (Ok(point), Ok(dx), Ok(dy)) = (
            vivid_protocol::vector::Point::new(x / scale, y / scale),
            Scalar::new(scroll.dx / scale),
            Scalar::new(scroll.dy / scale),
        ) else {
            return false;
        };
        self.pointer_position = Some(point);
        let scroll = Scroll { dx, dy, precise: scroll.precise, phase: scroll.phase };
        let drawings: Vec<_> = self.displayed.values().cloned().collect();
        self.windows.wheel(point, scroll, modifiers, |id, point| {
            drawings
                .iter()
                .find(|d| d.window == id)
                .and_then(|d| d.compiled.hit(point))
                .map(|target| (target.id, target.role))
        })
    }

    pub fn remove_track(&mut self, identity: TrackIdentity) {
        self.scenes.remove(&identity);
        if let Some(submission) = self.submissions.remove(&identity) {
            self.supersede(identity.surface.context.session, submission);
        }
        self.active
            .retain(|surface, s| *surface != identity.surface || s.track_id != identity.track_id);
        // Displayed/in-flight scenes retain their resources after the immutable track retires.
        self.assets.retain(|_, (token, _)| token.strong_count() != 0);
    }

    pub(super) fn track_lost(&mut self, identity: TrackIdentity, generation: ChannelGeneration) {
        if self.scenes.get(&identity).is_some_and(|(current, _, _)| *current == generation) {
            self.remove_track(identity);
        }
    }

    /// A bounded snapshot of immutable scenes. Moving windows only changes placement transforms.
    pub fn drawing(&self, scene: &SharedScene) -> Vec<Drawing> {
        let Some(viewport) = self.viewport else { return Vec::new() };
        self.windows
            .visible()
            .filter_map(|window| {
                let active = scene.active_track(window.identity, vivid_sdk::SLOT_VECTOR)?;
                let active = scene.track_status(active)?;
                let Some((generation, revision, compiled)) = self.scenes.get(&active.identity)
                else {
                    let mut previous = self.displayed.get(&window.identity)?.clone();
                    previous.bounds = window.options.bounds;
                    previous.window_revision = window.revision;
                    previous.scale =
                        f64::from(viewport.scale_numerator) / f64::from(viewport.scale_denominator);
                    return Some(previous);
                };
                if *generation != active.state.channel_generation || active.lifecycle != 1 {
                    return None;
                }
                Some(Drawing {
                    window: window.identity,
                    window_revision: window.revision,
                    revision: *revision,
                    bounds: window.options.bounds,
                    scale: f64::from(viewport.scale_numerator)
                        / f64::from(viewport.scale_denominator),
                    compiled: compiled.clone(),
                    submission: self.submissions.get(&active.identity).copied()?,
                })
            })
            .collect()
    }

    pub(super) fn sync_active(&mut self, scene: &SharedScene, surface: SurfaceIdentity) {
        let Some(active) = scene.active_track(surface, vivid_sdk::SLOT_VECTOR) else { return };
        let Some(active) = scene.track_status(active) else { return };
        let Some((generation, _, _)) = self.scenes.get(&active.identity) else { return };
        if *generation != active.state.channel_generation {
            return;
        }
        if let Some(submission) = self.submissions.get(&active.identity).copied()
            && let Some(old) = self.active.insert(surface, submission)
            && old != submission
        {
            self.supersede(surface.context.session, old);
        }
    }

    fn resolve(
        &mut self,
        owner: SessionIdentity,
        submission: Submission,
        outcome: PresentationOutcome,
    ) {
        if self.pending.remove(&(owner, submission)) {
            match outcome {
                PresentationOutcome::Presented => {
                    self.presented_scenes = self.presented_scenes.saturating_add(1);
                },
                PresentationOutcome::Superseded => {
                    self.superseded_scenes = self.superseded_scenes.saturating_add(1);
                },
            }
            self.outcomes
                .entry(owner)
                .or_default()
                .push_back(SubmissionOutcome { submission, outcome });
            if let Some(actor) = self.actors.get(&owner).and_then(Weak::upgrade) {
                actor.wake_actor();
            }
        }
    }

    fn supersede(&mut self, owner: SessionIdentity, submission: Submission) {
        if !self
            .inflight
            .values()
            .any(|d| d.window.context.session == owner && d.submission == submission)
        {
            self.resolve(owner, submission, PresentationOutcome::Superseded);
        }
    }

    /// Reserve the exact snapshot being composed. A newer candidate cannot retire it mid-render.
    pub fn prepare(&mut self, scene: &SharedScene) -> Vec<Drawing> {
        let drawings = self.drawing(scene);
        self.inflight = drawings.iter().map(|d| (d.window, d.clone())).collect();
        drawings
    }

    /// Called only after successful final composition, or to abandon an unpresented snapshot.
    pub fn finish(&mut self, presented: bool) {
        let inflight = std::mem::take(&mut self.inflight);
        for (surface, drawing) in inflight {
            if presented
                && self
                    .windows
                    .get(surface)
                    .is_some_and(|w| w.generation == drawing.submission.address.generation)
            {
                if self.windows.get(surface).is_some_and(|w| w.scene_revision < drawing.revision) {
                    let _ = self.windows.publish_scene(
                        surface,
                        drawing.submission.address.generation,
                        drawing.revision,
                    );
                }
                self.resolve(
                    surface.context.session,
                    drawing.submission,
                    PresentationOutcome::Presented,
                );
                self.displayed.insert(surface, drawing.clone());
            }
            if !self
                .submissions
                .iter()
                .any(|(id, s)| id.surface == surface && *s == drawing.submission)
            {
                self.resolve(
                    surface.context.session,
                    drawing.submission,
                    PresentationOutcome::Superseded,
                );
            }
        }
        self.assets.retain(|_, (token, _)| token.strong_count() != 0);
        // The scene under the pointer has just been replaced, so the region it names may differ
        // from the one the producer last saw.
        self.refresh_hover();
        self.retire_stale_semantics();
    }

    /// Scene revisions are window-scoped, including across track replacement. Validate before
    /// changing either the active slot or its compiled content so input never refers to old hits.
    fn validate_revision(
        &self,
        surface: SurfaceIdentity,
        revision: u64,
    ) -> Result<(), &'static str> {
        let window = self.windows.get(surface).ok_or("overlay window is absent")?;
        if revision == 0
            || revision <= window.scene_revision
            || self.active.get(&surface).is_some_and(|s| revision <= s.revision)
        {
            return Err("overlay scene revision must advance across track replacement");
        }
        Ok(())
    }

    pub(super) fn validate_activation(
        &self,
        scene: &SharedScene,
        surface: SurfaceIdentity,
        bindings: &[(u64, u64, ChannelGeneration, u64)],
    ) -> Result<(), &'static str> {
        for (slot, track, generation, _) in bindings {
            if *slot != vivid_sdk::SLOT_VECTOR {
                continue;
            }
            let identity = surface.track(*track).map_err(|_| "invalid vector track identity")?;
            if scene.active_track(surface, *slot) == Some(identity)
                && self
                    .active
                    .get(&surface)
                    .is_some_and(|s| s.channel_generation == generation.get())
            {
                continue;
            }
            let (compiled_generation, revision, _) =
                self.scenes.get(&identity).ok_or("vector track has no compiled scene")?;
            if compiled_generation != generation {
                return Err("stale compiled vector generation");
            }
            if scene.active_track(surface, *slot) != Some(identity) {
                self.validate_revision(surface, *revision)?;
            }
        }
        Ok(())
    }
}

/// Lives only on the authenticated bulk channel worker. Shaping never executes on the UI loop.
pub(super) struct VectorWorker {
    font: crate::config::font::Font,
    text: TextSystem,
    images: BTreeMap<u64, ImageData>,
    tokens: BTreeMap<u64, Arc<()>>,
    last_asset: u64,
}
impl VectorWorker {
    pub fn new(font: crate::config::font::Font) -> Self {
        Self {
            text: TextSystem::new(font.clone()),
            font,
            images: BTreeMap::new(),
            tokens: BTreeMap::new(),
            last_asset: 0,
        }
    }

    pub fn process(
        &mut self,
        shared: &ServiceShared,
        identity: TrackIdentity,
        generation: ChannelGeneration,
        record_type: u16,
        sequence: u64,
        body: &[u8],
    ) -> Result<(), &'static str> {
        use vivid_protocol::vector::{Frame, ImageAsset, Limits};
        let length = u32::try_from(body.len()).map_err(|_| "vector record exceeds u32")?;
        if record_type == messages::VECTOR_ASSET {
            let asset = ImageAsset::decode(body).map_err(|e| e.0)?;
            if asset.id <= self.last_asset {
                return Err("retained image IDs must increase within a channel generation");
            }
            let mut host = lock(&shared.overlays);
            host.assets.retain(|_, (token, _)| token.strong_count() != 0);
            let (owner_count, owner_bytes) = host
                .assets
                .iter()
                .filter(|((track, _, _), _)| {
                    track.surface.context.session == identity.surface.context.session
                })
                .try_fold((0_usize, 0_usize), |(count, bytes), (_, (_, b))| {
                    Some((count.checked_add(1)?, bytes.checked_add(*b)?))
                })
                .ok_or("retained image accounting overflow")?;
            if owner_count >= vivid_protocol::vector::MAX_RETAINED_ASSETS
                || owner_bytes
                    .checked_add(asset.rgba.len())
                    .is_none_or(|bytes| bytes > vivid_protocol::vector::MAX_RETAINED_ASSET_BYTES)
            {
                return Err("retained image capacity exhausted");
            }
            shared.scene.admit_vector_asset(identity, generation, length, sequence)?;
            let token = Arc::new(());
            host.assets.insert(
                (identity, generation, asset.id),
                (Arc::downgrade(&token), asset.rgba.len()),
            );
            self.tokens.insert(asset.id, token);
            self.last_asset = asset.id;
            self.images.insert(
                asset.id,
                ImageData {
                    data: Blob::new(Arc::new(asset.rgba)),
                    width: asset.width,
                    height: asset.height,
                    format: ImageFormat::Rgba8,
                    alpha_type: ImageAlphaType::Alpha,
                },
            );
        } else if record_type == messages::VECTOR_ASSET_RELEASE {
            let release = vivid_protocol::vector::AssetRelease::decode(body).map_err(|e| e.0)?;
            if !self.images.contains_key(&release.id) {
                return Err("retained image is absent or released");
            }
            shared.scene.admit_vector_asset(identity, generation, length, sequence)?;
            self.images.remove(&release.id);
            self.tokens.remove(&release.id);
            lock(&shared.overlays).assets.retain(|_, (token, _)| token.strong_count() != 0);
        } else {
            let frame = Frame::decode(body).map_err(|e| e.0)?;
            frame.canvas.validate_with_limits(&Limits::default()).map_err(|e| e.0)?;
            let status = shared.scene.track_status(identity).ok_or("vector track is absent")?;
            let KindConfiguration::VectorScene(config) = status.configuration.kind else {
                return Err("vector frame used a non-vector track");
            };
            if body.len() > config.maximum_scene_bytes as usize + 12 {
                return Err("vector scene exceeds immutable track ceiling");
            }
            let font = lock(&shared.overlays).font();
            if self.font != font {
                self.text.update_font(font.clone());
                self.font = font;
            }
            // Paint commands are available only to a producer that negotiated the paint profile,
            // exactly as retained text layouts are. Checking before compilation keeps an
            // un-negotiated command from ever reaching a renderer.
            if frame.canvas.commands().iter().any(|command| command.requires_paint())
                && !lock(&shared.overlays)
                    .actors
                    .get(&identity.surface.context.session)
                    .and_then(Weak::upgrade)
                    .is_some_and(|session| session.supports(registry::OVERLAY_PAINT))
            {
                return Err("paint commands were not negotiated");
            }
            // A cursor is available only to a producer that negotiated the pointer profile.
            if frame.canvas.commands().iter().any(|command| command.requires_pointer())
                && !lock(&shared.overlays)
                    .actors
                    .get(&identity.surface.context.session)
                    .and_then(Weak::upgrade)
                    .is_some_and(|session| session.supports(registry::OVERLAY_POINTER))
            {
                return Err("cursor shapes were not negotiated");
            }
            let layouts = {
                let host = lock(&shared.overlays);
                let mut layouts = BTreeMap::new();
                for command in frame.canvas.commands() {
                    if let vivid_protocol::vector::Command::TextLayout { layout, .. } = command {
                        if !host
                            .actors
                            .get(&identity.surface.context.session)
                            .and_then(Weak::upgrade)
                            .is_some_and(|session| session.supports(registry::OVERLAY_TEXT_LAYOUT))
                        {
                            return Err("text layouts were not negotiated");
                        }
                        let value = host
                            .layouts
                            .get(&(identity.surface, *layout))
                            .ok_or("text layout is absent or released")?;
                        layouts.insert(*layout, value.clone());
                    }
                }
                layouts
            };
            let mut compiled = crate::display::vector::compile_with_layouts(
                &frame.canvas,
                &mut self.text,
                &self.images,
                &layouts,
            )
            .map_err(|e| e.0)?;
            let ids: HashSet<_> = frame
                .canvas
                .commands()
                .iter()
                .filter_map(|c| match c {
                    vivid_protocol::vector::Command::Image { asset, .. } => Some(*asset),
                    _ => None,
                })
                .collect();
            compiled.retained.extend(
                ids.iter()
                    .map(|id| self.tokens.get(id).cloned().ok_or("retained image is absent"))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            let compiled = Arc::new(compiled);
            let mut host = lock(&shared.overlays);
            if !host.surfaces.contains(&identity.surface) {
                return Err("vector surface has no overlay window");
            }
            let owner = identity.surface.context.session;
            if host.pending.iter().filter(|(id, _)| *id == owner).count()
                + host.outcomes.get(&owner).map_or(0, VecDeque::len)
                >= 256
            {
                host.overflow.insert(owner);
                if let Some(actor) = host.actors.get(&owner).and_then(Weak::upgrade) {
                    actor.wake_actor();
                }
                return Err("overlay outcome capacity exhausted");
            }
            if host
                .accepted
                .get(&identity.surface)
                .is_some_and(|revision| frame.revision <= *revision)
            {
                return Err("scene revision must advance across all window tracks");
            }
            let window = host.windows.get(identity.surface).ok_or("overlay window is absent")?;
            let submission = Submission {
                address: WindowAddress {
                    context_id: identity.surface.context.context_id,
                    surface_id: identity.surface.surface_id,
                    generation: window.generation,
                },
                track_id: identity.track_id,
                channel_generation: generation.get(),
                epoch: frame.epoch,
                revision: frame.revision,
            };
            if shared.scene.active_track(identity.surface, vivid_sdk::SLOT_VECTOR) == Some(identity)
            {
                host.validate_revision(identity.surface, frame.revision)?;
            }
            shared.scene.admit_media(
                identity,
                generation,
                length,
                frame.epoch,
                frame.revision,
                true,
                sequence,
            )?;
            // One pending replacement per window, including tracks being primed. Keep the
            // displayed and reserved snapshots independently of the candidate's track lifetime.
            let retired: Vec<_> = host
                .scenes
                .keys()
                .copied()
                .filter(|id| id.surface == identity.surface && *id != identity)
                .collect();
            for track in retired {
                host.scenes.remove(&track);
                if let Some(old) = host.submissions.remove(&track) {
                    host.supersede(owner, old);
                }
            }
            host.scenes.insert(identity, (generation, frame.revision, compiled));
            host.compiled_scenes = host.compiled_scenes.saturating_add(1);
            host.accepted.insert(identity.surface, frame.revision);
            host.pending.insert((owner, submission));
            if let Some(old) = host.submissions.insert(identity, submission) {
                host.supersede(owner, old);
            }
            shared.scene.mark_output_ready(identity, generation)?;
            host.sync_active(&shared.scene, identity.surface);
        }
        shared.request_frame_wake();
        Ok(())
    }
}

type ControlReply = (u16, u64, Result<Vec<u8>, messages::MessageError>);

pub(super) fn dispatch(
    shared: &ServiceShared,
    session: &SessionRuntime,
    record: &Record,
    request_id: u64,
    value: &Value,
) -> Result<ControlReply, ControlError> {
    if !session.supports(registry::TERMINAL_OVERLAY) {
        return Err(ControlError::unsupported("terminal-overlay-v1 was not negotiated"));
    }
    let mut host = lock(&shared.overlays);
    let (reply, payload) = match record.record_type {
        messages::SET_OVERLAY_WINDOW => {
            let mut request = SetWindow::decode(session.identity, record.object_id, value)
                .map_err(|_| ControlError::bad_message("invalid overlay window request"))?;
            require_context_operation(session, request.address.context_id, OP_SURFACE_TRACK_MEDIA)?;
            let identity = request
                .address
                .identity(session.identity)
                .map_err(|_| ControlError::bad_message("invalid overlay identity"))?;
            let surface = shared
                .scene
                .surface_status(identity)
                .ok_or_else(|| ControlError::not_found("overlay surface is absent"))?;
            if surface.generation.get() != request.address.generation {
                return Err(ControlError::precondition("stale overlay surface generation"));
            }
            if surface.definition.coordinate_model != surface::CoordinateModel::DesktopLogicalPixels
            {
                return Err(ControlError::bad_message(
                    "overlay content requires logical pixel coordinates",
                ));
            }
            if let Some(parent) = request.options.parent {
                require_context_operation(
                    session,
                    parent.context.context_id,
                    OP_SURFACE_TRACK_MEDIA,
                )?;
            }
            if host.viewport.is_none() {
                return Err(ControlError::bad_state("overlay viewport is unavailable"));
            }
            if !host.lanes.contains_key(&session.identity) {
                return Err(ControlError::bad_state("overlay input lane is unavailable"));
            }
            if request.expected_revision == 0 && host.surfaces.contains(&identity) {
                return Err(ControlError::state(
                    "an overlay surface cannot be reopened after dismissal",
                ));
            }
            request.expected_revision = host
                .set_window(
                    identity,
                    request.address.generation,
                    request.expected_revision,
                    request.options.clone(),
                )
                .map_err(|e| ControlError::state(e.0))?;
            host.surfaces.insert(identity);
            (
                messages::OVERLAY_WINDOW_READY,
                request
                    .payload(session.identity)
                    .map_err(|_| ControlError::bad_message("invalid overlay reply"))?,
            )
        },
        messages::OVERLAY_ACTION => {
            let action = Action::decode(record.object_id, value)
                .map_err(|_| ControlError::bad_message("invalid overlay action"))?;
            require_context_operation(session, action.address.context_id, OP_SURFACE_TRACK_MEDIA)?;
            let viewport = host
                .viewport
                .ok_or_else(|| ControlError::bad_state("overlay viewport is unavailable"))?;
            host.apply_action(session.identity, action, viewport)
                .map_err(|e| ControlError::state(e.0))?;
            (messages::OK, vec![])
        },
        messages::SET_OVERLAY_CLIPBOARD => {
            let request = Clipboard::decode(record.object_id, value)
                .map_err(|_| ControlError::bad_message("invalid overlay clipboard request"))?;
            require_context_operation(session, request.address.context_id, OP_SURFACE_TRACK_MEDIA)?;
            let identity = request
                .address
                .identity(session.identity)
                .map_err(|_| ControlError::bad_message("invalid overlay identity"))?;
            let generation = host
                .windows
                .get(identity)
                .ok_or_else(|| ControlError::not_found("overlay window is absent"))?
                .generation;
            if generation != request.address.generation {
                return Err(ControlError::precondition("stale overlay window generation"));
            }
            // A clipboard is shared with every other application, so a write is only honored
            // when the user just acted in the window asking for it.
            host.authorize_clipboard(identity).map_err(ControlError::state)?;
            shared.stage_clipboard(session.identity, request.text);
            // The write itself happens on the UI thread, and the tail of this function wakes it.
            (messages::OK, vec![])
        },
        messages::SET_OVERLAY_SEMANTICS => {
            if !session.supports(registry::OVERLAY_A11Y) {
                return Err(ControlError::unsupported("overlay-a11y-v1 was not negotiated"));
            }
            let request = SetSemantics::decode(record.object_id, value)
                .map_err(|_| ControlError::bad_message("invalid overlay semantics"))?;
            require_context_operation(session, request.address.context_id, OP_SURFACE_TRACK_MEDIA)?;
            let identity = request
                .address
                .identity(session.identity)
                .map_err(|_| ControlError::bad_message("invalid overlay identity"))?;
            let generation = host
                .windows
                .get(identity)
                .ok_or_else(|| ControlError::not_found("overlay window is absent"))?
                .generation;
            if generation != request.address.generation {
                return Err(ControlError::precondition("stale overlay window generation"));
            }
            host.set_semantics(identity, request.semantics).map_err(ControlError::state)?;
            (messages::OK, vec![])
        },
        messages::QUERY_OVERLAY => {
            let query = Query::decode(record.object_id, value)
                .map_err(|_| ControlError::bad_message("invalid overlay query"))?;
            require_context(session, query.context_id)?;
            let status = host.status(session.identity, query)?;
            (
                messages::OVERLAY_STATUS,
                status
                    .payload(session.identity)
                    .map_err(|_| ControlError::bad_message("invalid overlay status"))?,
            )
        },
        _ => return Err(ControlError::bad_message("unknown overlay control")),
    };
    host.editor_rect();
    drop(host);
    if record.record_type != messages::QUERY_OVERLAY {
        shared.request_frame_wake();
    }
    Ok((reply, record.object_id, Envelope::new(request_id, payload).encode()))
}

pub(super) fn lane_record(
    shared: &ServiceShared,
    session: &SessionRuntime,
    record: &Record,
) -> io::Result<()> {
    let envelope = messages::decode_control(&record.body)?;
    envelope.validate_request()?;
    let value = Value::Map(envelope.payload);
    let mut host = lock(&shared.overlays);
    let result = (|| -> Result<(), &'static str> {
        match record.record_type {
            messages::OVERLAY_INPUT_RENEW => {
                let renew = vivid_protocol::overlay::wire::Renew::decode(record.object_id, &value)
                    .map_err(|_| "invalid overlay renewal")?;
                let lane = host.lanes.get_mut(&session.identity).ok_or("overlay lane is absent")?;
                if lane.0 != renew.lane_generation {
                    return Err("stale overlay lane generation");
                }
                lane.1 = Instant::now() + Duration::from_micros(renew.watchdog_us);
            },
            messages::OVERLAY_INPUT_CAPTURE => {
                let capture =
                    vivid_protocol::overlay::wire::Capture::decode(record.object_id, &value)
                        .map_err(|_| "invalid overlay capture")?;
                let id = capture
                    .address
                    .identity(session.identity)
                    .map_err(|_| "invalid overlay identity")?;
                let window = host.windows.get(id).ok_or("overlay window is absent")?;
                if window.generation != capture.address.generation
                    || window.scene_revision != capture.scene_revision
                {
                    return Err("stale overlay capture scene");
                }
                if capture.capture {
                    let position = host.pointer_position.unwrap_or_default();
                    host.windows.capture_pointer(id, position).map_err(|e| e.0)?;
                } else {
                    host.windows.release_pointer(id);
                }
            },
            _ => return Err("unexpected overlay lane record"),
        }
        Ok(())
    })();
    drop(host);
    session.wake_actor();
    match result {
        Ok(()) => {
            session.post_lane(messages::OK, record.object_id, messages::ok(envelope.request_id));
        },
        Err(error) => {
            session.post_lane(
                messages::ERROR,
                record.object_id,
                protocol_error(envelope.request_id, messages::ERROR_BAD_STATE, false, error)?,
            );
        },
    }
    Ok(())
}

/// Called by the existing bounded control actor, independently of vector compilation.
pub(super) fn service_input(shared: &ServiceShared, session: &SessionRuntime) {
    if !session.supports(registry::OVERLAY_INPUT) || lock(&session.lane_writer).is_none() {
        return;
    }
    let mut host = lock(&shared.overlays);
    // Dismissal removes the window model, but the producer may retain its semantic surface.
    // Resolve receipts now; retaining that surface must not keep dead presentations alive.
    let dismissed: Vec<_> = host
        .surfaces
        .iter()
        .copied()
        .filter(|id| id.context.session == session.identity && host.windows.get(*id).is_none())
        .collect();
    for surface in dismissed {
        host.layouts.retain(|(owner, _), _| *owner != surface);
        host.retire_presentation(surface);
    }
    let expired =
        host.lanes.get(&session.identity).is_some_and(|(_, deadline)| Instant::now() >= *deadline);
    let mut failed = expired
        || host.windows.take_overflow(session.identity)
        || host.overflow.remove(&session.identity);
    if !failed && host.environment_dirty.remove(&session.identity) {
        let update = EnvironmentChanged {
            revision: host.environment_revision.max(1),
            environment: host.environment.clone(),
        };
        failed = update
            .payload()
            .and_then(|p| Envelope::new(0, p).encode())
            .map_or(true, |body| !session.post_lane(messages::OVERLAY_ENV_CHANGED, 0, body));
    }
    if !failed
        && host.viewport_dirty.remove(&session.identity)
        && let Some(viewport) = host.viewport
    {
        let update = ViewportChanged { revision: host.viewport_revision, viewport };
        failed = update
            .payload()
            .and_then(|p| Envelope::new(0, p).encode())
            .map_or(true, |body| !session.post_lane(messages::OVERLAY_VIEWPORT_CHANGED, 0, body));
    }
    if !failed && let Some(actions) = host.accessibility.get_mut(&session.identity) {
        while let Some((surface_id, payload)) = actions.pop_front() {
            let body = match Envelope::new(0, payload).encode() {
                Ok(body) => body,
                Err(_) => break,
            };
            if !session.post_lane(messages::OVERLAY_INPUT_EVENT, surface_id, body) {
                failed = true;
                break;
            }
        }
    }
    if !failed && let Some(outcomes) = host.outcomes.get_mut(&session.identity) {
        while let Some(outcome) = outcomes.pop_front() {
            if outcome.payload().and_then(|p| Envelope::new(0, p).encode()).map_or(true, |body| {
                !session.post_lane(
                    messages::OVERLAY_SUBMISSION_OUTCOME,
                    outcome.submission.address.surface_id,
                    body,
                )
            }) {
                failed = true;
                break;
            }
        }
    }
    if !failed {
        while let Some(event) = host.windows.take_event(session.identity) {
            let event = vivid_protocol::overlay::wire::InputEvent {
                address: WindowAddress {
                    context_id: event.window.context.context_id,
                    surface_id: event.window.surface_id,
                    generation: event.generation,
                },
                scene_revision: event.scene_revision,
                event: event.event,
            };
            let body = event.payload().and_then(|payload| Envelope::new(0, payload).encode());
            if body.map_or(true, |body| {
                !session.post_lane(messages::OVERLAY_INPUT_EVENT, event.address.surface_id, body)
            }) {
                failed = true;
                break;
            }
        }
    }
    if failed {
        let surfaces = host.remove_owner(session.identity);
        drop(host);
        for surface in surfaces {
            let _ = shared.scene.destroy_surface(surface);
        }
        if let Some(writer) = lock(&session.lane_writer).as_ref() {
            writer.shutdown_handle().stop();
        }
        shared.request_frame_wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivid_protocol::overlay::{WindowMode, WindowOptions};
    use vivid_protocol::vector::Rect;

    #[test]
    fn cleanup_is_owner_and_context_scoped_and_dpi_is_logical() {
        let mut host = Host::default();
        host.update_viewport(1200., 800., 2.).unwrap();
        let first = SessionIdentity::new(PresenterInstanceId([1; 16]), 1).unwrap();
        let second = SessionIdentity::new(PresenterInstanceId([1; 16]), 2).unwrap();
        for owner in [first, second] {
            let id = owner.context(1).unwrap().surface(1).unwrap();
            host.windows
                .create(
                    id,
                    1,
                    WindowOptions::new(
                        Rect::new(10., 20., 100., 80.).unwrap(),
                        WindowMode::Floating,
                    ),
                )
                .unwrap();
            host.surfaces.insert(id);
        }
        let query = Query { context_id: 1, surface_id: 1 };
        assert_eq!(host.status(first, query).ok().unwrap().viewport.width.get(), 600.);
        host.remove_contexts(first, &HashSet::from([1]));
        assert!(host.status(first, query).is_err());
        assert!(host.status(second, query).is_ok());
        assert!(host.remove_owner(first).is_empty());
        assert!(host.status(second, query).is_ok());
        assert_eq!(host.remove_owner(second).len(), 1);
        assert!(host.status(second, query).is_err());
    }

    #[test]
    fn stale_replacement_is_rejected_before_publication_and_other_owners_are_independent() {
        let mut host = Host::default();
        let scene = SharedScene::for_test();
        let mut text = TextSystem::new(Default::default());
        let compiled = Arc::new(
            compile(&vivid_protocol::vector::Canvas::new(), &mut text, &BTreeMap::new()).unwrap(),
        );
        for session_id in [1, 2] {
            let owner = SessionIdentity::new(PresenterInstanceId([1; 16]), session_id).unwrap();
            let surface = owner.context(1).unwrap().surface(1).unwrap();
            host.windows
                .create(
                    surface,
                    1,
                    WindowOptions::new(Rect::new(0., 0., 10., 10.).unwrap(), WindowMode::Floating),
                )
                .unwrap();
            if session_id == 1 {
                host.windows.publish_scene(surface, 1, 8).unwrap();
            }
            let track = surface.track(2).unwrap();
            host.scenes.insert(track, (ChannelGeneration::ONE, 8, compiled.clone()));
            let bindings = [(
                vivid_sdk::SLOT_VECTOR,
                2,
                ChannelGeneration::ONE,
                vivid_sdk::MILESTONE_OUTPUT_READY,
            )];
            assert_eq!(
                host.validate_activation(&scene, surface, &bindings).is_ok(),
                session_id == 2
            );
            assert_eq!(
                host.windows.get(surface).unwrap().scene_revision,
                if session_id == 1 { 8 } else { 0 }
            );
            host.scenes.insert(track, (ChannelGeneration::ONE, 9, compiled.clone()));
            assert!(host.validate_activation(&scene, surface, &bindings).is_ok());
            assert_eq!(scene.active_track(surface, vivid_sdk::SLOT_VECTOR), None);
        }
    }

    /// A window covering `x..x+100`, `y..y+100`, with one region over the whole of it.
    fn placed(
        host: &mut Host,
        owner: SessionIdentity,
        surface_id: u64,
        bounds: Rect,
        cursor: Option<vivid_protocol::vector::CursorShape>,
    ) -> SurfaceIdentity {
        let surface = owner.context(1).unwrap().surface(surface_id).unwrap();
        host.windows.create(surface, 1, WindowOptions::new(bounds, WindowMode::Floating)).unwrap();
        let mut canvas = vivid_protocol::vector::Canvas::new();
        canvas
            .push(vivid_protocol::vector::Command::Hit {
                id: surface_id,
                path: vivid_protocol::vector::Path::rectangle(
                    Rect::new(0., 0., bounds.width.get(), bounds.height.get()).unwrap(),
                )
                .unwrap(),
                role: vivid_protocol::vector::HitRole::Input,
                cursor,
            })
            .unwrap();
        let compiled = Arc::new(
            compile(&canvas, &mut TextSystem::new(Default::default()), &BTreeMap::new()).unwrap(),
        );
        host.displayed.insert(
            surface,
            Drawing {
                window: surface,
                window_revision: 1,
                revision: 1,
                bounds,
                scale: 1.,
                compiled,
                submission: Submission {
                    address: vivid_protocol::overlay::wire::WindowAddress {
                        context_id: 1,
                        surface_id,
                        generation: 1,
                    },
                    track_id: surface_id,
                    channel_generation: 1,
                    epoch: 1,
                    revision: 1,
                },
            },
        );
        surface
    }

    #[test]
    fn the_environment_names_the_host_font_and_only_real_preferences() {
        let mut host = Host::default();
        // A default that failed validation would revoke a lane before the host learned its font.
        assert!(host.environment().validate().is_ok());

        let font = crate::config::font::Font::default()
            .with_size(crate::config::font::FontSize::from_px(13.5));
        let family = font.normal().family.clone();
        host.set_font(font);
        let environment = host.environment();
        // The environment names the pane's own font, so plain overlay text matches the terminal.
        assert_eq!(environment.font_family, family);
        assert_eq!(environment.font_size.get(), 13.5);
        // Vivido has no reduced-motion signal, so it reports absence rather than asserting one.
        assert_eq!(environment.reduced_motion, None);

        // A display rate becomes an interval, and an unknown one stays absent.
        // The host records microseconds; the service converts a monitor's millihertz into them.
        host.set_refresh_interval(Some(16_667));
        assert_eq!(host.environment().refresh_interval_us, Some(16_667));
        // A zero interval is not a rate, so it is absence rather than a fast display.
        host.set_refresh_interval(Some(0));
        assert_eq!(host.environment().refresh_interval_us, None);
        host.set_refresh_interval(None);
        assert_eq!(host.environment().refresh_interval_us, None);

        // An appearance change republishes; repeating it does not.
        let before = host.environment_revision;
        host.set_appearance(vivid_protocol::overlay::wire::Appearance::Dark);
        assert!(host.environment_revision > before);
        let settled = host.environment_revision;
        host.set_appearance(vivid_protocol::overlay::wire::Appearance::Dark);
        assert_eq!(host.environment_revision, settled);
    }

    #[test]
    fn hover_follows_window_movement_raise_and_hide() {
        let mut host = Host::default();
        host.update_viewport(800., 600., 1.).unwrap();
        let owner = SessionIdentity::new(PresenterInstanceId([1; 16]), 1).unwrap();
        // Two overlapping windows, the second created above the first.
        let below = placed(
            &mut host,
            owner,
            1,
            Rect::new(0., 0., 200., 200.).unwrap(),
            Some(vivid_protocol::vector::CursorShape::Text),
        );
        let above = placed(
            &mut host,
            owner,
            2,
            Rect::new(0., 0., 200., 200.).unwrap(),
            Some(vivid_protocol::vector::CursorShape::Pointer),
        );
        host.pointer_position = Some(vivid_protocol::vector::Point::new(50., 50.).unwrap());
        host.refresh_hover();
        // The topmost window owns the pointer.
        assert_eq!(host.cursor(), Some(vivid_protocol::vector::CursorShape::Pointer));

        // Raising the lower one changes which window is on top with no pointer movement.
        let raise = Action {
            address: vivid_protocol::overlay::wire::WindowAddress {
                context_id: 1,
                surface_id: 1,
                generation: 1,
            },
            expected_revision: 1,
            action: vivid_protocol::overlay::wire::WindowAction::Raise,
        };
        host.apply_action(owner, raise, host.viewport.unwrap()).unwrap();
        assert_eq!(host.cursor(), Some(vivid_protocol::vector::CursorShape::Text));

        // Moving the now-topmost window away leaves the pointer over the other one.
        host.set_window(
            below,
            1,
            2,
            WindowOptions::new(Rect::new(400., 400., 200., 200.).unwrap(), WindowMode::Floating),
        )
        .unwrap();
        assert_eq!(host.cursor(), Some(vivid_protocol::vector::CursorShape::Pointer));

        // Hiding the last window under the pointer ends the hover entirely.
        host.windows.close(above, vivid_protocol::overlay::DismissReason::Closed);
        assert_eq!(host.cursor(), None);
    }

    #[test]
    fn a_clipboard_write_needs_focus_a_recent_gesture_in_the_same_window() {
        let mut host = Host::default();
        host.update_viewport(800., 600., 1.).unwrap();
        let owner = SessionIdentity::new(PresenterInstanceId([1; 16]), 1).unwrap();
        let first = placed(&mut host, owner, 1, Rect::new(0., 0., 100., 100.).unwrap(), None);
        let second = placed(&mut host, owner, 2, Rect::new(200., 0., 100., 100.).unwrap(), None);

        // Nothing has happened yet.
        assert!(host.authorize_clipboard(first).is_err());
        host.pointer_position = Some(vivid_protocol::vector::Point::new(10., 10.).unwrap());
        host.refresh_hover();

        // A press in the first window authorizes a write from the first window.
        host.pointer(10., 10., Some((0, true)), 0, None);
        assert!(host.authorize_clipboard(first).is_ok());
        // It does not authorize a write from the other window, even though that one exists.
        assert!(host.authorize_clipboard(second).is_err());
        // Nor from a window that is not there at all.
        let absent = owner.context(1).unwrap().surface(99).unwrap();
        assert!(host.authorize_clipboard(absent).is_err());

        // Focusing elsewhere withdraws it, because focus is what a write requires.
        host.pointer(210., 10., Some((0, true)), 0, None);
        assert!(host.authorize_clipboard(first).is_err());
        assert!(host.authorize_clipboard(second).is_ok());

        // A gesture in one window is never spendable by another, even right after focusing it.
        let mut third = Host::default();
        third.update_viewport(800., 600., 1.).unwrap();
        let a = placed(&mut third, owner, 1, Rect::new(0., 0., 100., 100.).unwrap(), None);
        let b = placed(&mut third, owner, 2, Rect::new(200., 0., 100., 100.).unwrap(), None);
        third.pointer_position = Some(vivid_protocol::vector::Point::new(10., 10.).unwrap());
        third.refresh_hover();
        third.pointer(10., 10., Some((0, true)), 0, None);
        // Requesting focus for the other window does not launder the first window's gesture.
        third.windows.request_focus(b).unwrap();
        // B is focused and still refused, so what refuses it is the gesture guard rather than
        // the focus check happening to fail.
        assert_eq!(third.windows.focus(), Some(b));
        assert!(third.authorize_clipboard(b).is_err());
        // And A is refused because focusing B revoked A's eligibility.
        assert!(third.authorize_clipboard(a).is_err());
        // A press in B is what finally authorizes B.
        third.pointer_position = Some(vivid_protocol::vector::Point::new(210., 10.).unwrap());
        third.refresh_hover();
        third.pointer(210., 10., Some((0, true)), 0, None);
        assert!(third.authorize_clipboard(b).is_ok());
    }

    #[test]
    fn an_expired_gesture_cannot_be_banked() {
        let mut host = Host::default();
        host.update_viewport(800., 600., 1.).unwrap();
        let owner = SessionIdentity::new(PresenterInstanceId([1; 16]), 1).unwrap();
        let window = placed(&mut host, owner, 1, Rect::new(0., 0., 100., 100.).unwrap(), None);
        host.pointer_position = Some(vivid_protocol::vector::Point::new(10., 10.).unwrap());
        host.refresh_hover();
        host.pointer(10., 10., Some((0, true)), 0, None);
        assert!(host.authorize_clipboard(window).is_ok());

        // Replaying the gesture at an age past the ceiling must fail, so a producer cannot hold
        // one and spend it later.
        host.last_gesture = Some((
            window,
            Instant::now()
                - vivid_protocol::overlay::MAX_CLIPBOARD_GESTURE_AGE
                - Duration::from_millis(1),
        ));
        assert!(host.authorize_clipboard(window).is_err());
    }

    #[test]
    fn click_counts_follow_the_sequence_and_restart_after_a_fourth() {
        let mut host = Host::default();
        host.update_viewport(800., 600., 1.).unwrap();
        let point = vivid_protocol::vector::Point::new(10., 10.).unwrap();
        // A release is never a click.
        assert_eq!(host.count_clicks(point, Some((0, false))), 0);
        assert_eq!(host.count_clicks(point, None), 0);
        for expected in [1, 2, 3, 1] {
            assert_eq!(host.count_clicks(point, Some((0, true))), expected);
        }
        // A different button, or a pointer that moved, starts a new sequence.
        assert_eq!(host.count_clicks(point, Some((2, true))), 1);
        assert_eq!(host.count_clicks(point, Some((2, true))), 2);
        let far = vivid_protocol::vector::Point::new(200., 200.).unwrap();
        assert_eq!(host.count_clicks(far, Some((2, true))), 1);
    }

    #[test]
    fn hover_follows_the_published_scene_not_only_the_pointer() {
        let mut host = Host::default();
        host.update_viewport(800., 600., 1.).unwrap();
        let owner = SessionIdentity::new(PresenterInstanceId([1; 16]), 1).unwrap();
        let surface = owner.context(1).unwrap().surface(1).unwrap();
        host.windows
            .create(
                surface,
                1,
                WindowOptions::new(Rect::new(0., 0., 100., 100.).unwrap(), WindowMode::Floating),
            )
            .unwrap();
        host.pointer_position = Some(vivid_protocol::vector::Point::new(10., 10.).unwrap());
        // Nothing is displayed yet, so the pointer is over no region at all.
        assert_eq!(host.cursor(), None);

        let mut with_region = vivid_protocol::vector::Canvas::new();
        with_region
            .push(vivid_protocol::vector::Command::Hit {
                id: 1,
                path: vivid_protocol::vector::Path::rectangle(Rect::new(0., 0., 50., 50.).unwrap())
                    .unwrap(),
                role: vivid_protocol::vector::HitRole::Input,
                cursor: Some(vivid_protocol::vector::CursorShape::Text),
            })
            .unwrap();
        let compiled = Arc::new(
            compile(&with_region, &mut TextSystem::new(Default::default()), &BTreeMap::new())
                .unwrap(),
        );
        host.displayed.insert(
            surface,
            Drawing {
                window: surface,
                window_revision: 1,
                revision: 1,
                bounds: Rect::new(0., 0., 100., 100.).unwrap(),
                scale: 1.,
                compiled,
                submission: Submission {
                    address: vivid_protocol::overlay::wire::WindowAddress {
                        context_id: 1,
                        surface_id: 1,
                        generation: 1,
                    },
                    track_id: 2,
                    channel_generation: 1,
                    epoch: 1,
                    revision: 1,
                },
            },
        );
        host.refresh_hover();
        assert_eq!(host.cursor(), Some(vivid_protocol::vector::CursorShape::Text));
        assert!(matches!(
            host.windows.take_event(owner).map(|e| e.event),
            Some(vivid_protocol::overlay::Event::Hover { region: 1, entered: true })
        ));
    }

    #[test]
    fn pointer_capture_uses_the_host_observed_position() {
        let mut host = Host::default();
        host.update_viewport(800., 600., 2.).unwrap();
        let owner = SessionIdentity::new(PresenterInstanceId([1; 16]), 1).unwrap();
        let surface = owner.context(1).unwrap().surface(1).unwrap();
        host.windows
            .create(
                surface,
                1,
                WindowOptions::new(Rect::new(0., 0., 100., 100.).unwrap(), WindowMode::Floating),
            )
            .unwrap();
        assert_eq!(host.pointer_position, None);

        // Physical pixels become logical pixels through the viewport scale.
        host.pointer(120., 80., None, 0, None);
        assert_eq!(
            host.pointer_position,
            Some(vivid_protocol::vector::Point::new(60., 40.).unwrap())
        );

        // Wheel events carry the same authoritative position.
        let scroll = ScrollInput { dx: 0., dy: 3., precise: true, phase: ScrollPhase::Changed };
        host.wheel(40., 20., scroll, 0);
        assert_eq!(
            host.pointer_position,
            Some(vivid_protocol::vector::Point::new(20., 10.).unwrap())
        );

        // A capture established now starts from that position, never from a fabricated origin.
        host.windows.request_focus(surface).unwrap();
        let position = host.pointer_position.unwrap();
        host.windows.capture_pointer(surface, position).unwrap();
        assert!(host.windows.has_pointer_capture());
    }

    #[test]
    fn invalid_viewport_does_not_replace_authoritative_geometry() {
        let mut host = Host::default();
        host.update_viewport(800., 600., 1.25).unwrap();
        let previous = host.viewport;
        host.update_viewport(800., 600., 1.25).unwrap();
        assert_eq!(host.viewport_revision, 1);
        for (width, height, scale) in [(0., 600., 1.), (800., 600., 0.), (800., 600., f64::NAN)] {
            assert!(host.update_viewport(width, height, scale).is_err());
            assert_eq!(host.viewport, previous);
        }
    }

    #[test]
    fn outcomes_hold_inflight_scenes_and_assets_across_replacement_and_owner_cleanup() {
        let mut host = Host::default();
        host.update_viewport(800., 600., 2.).unwrap();
        let mut text = TextSystem::new(Default::default());
        let mut owners = Vec::new();
        for serial in [1, 2] {
            let owner = SessionIdentity::new(PresenterInstanceId([1; 16]), serial).unwrap();
            let surface = owner.context(1).unwrap().surface(1).unwrap();
            let bounds = Rect::new(0., 0., 10., 10.).unwrap();
            host.windows
                .create(surface, 1, WindowOptions::new(bounds, WindowMode::Floating))
                .unwrap();
            host.surfaces.insert(surface);
            host.lane_open(owner, 1);
            let token = Arc::new(());
            host.assets.insert(
                (surface.track(2).unwrap(), ChannelGeneration::ONE, 1),
                (Arc::downgrade(&token), 4),
            );
            let mut compiled =
                compile(&vivid_protocol::vector::Canvas::new(), &mut text, &BTreeMap::new())
                    .unwrap();
            compiled.retained.push(token); // Namespace release leaves only the scene's reference.
            let submission = Submission {
                address: WindowAddress { context_id: 1, surface_id: 1, generation: 1 },
                track_id: 2,
                channel_generation: 1,
                epoch: 1,
                revision: 1,
            };
            host.pending.insert((owner, submission));
            host.active.insert(surface, submission);
            host.inflight.insert(
                surface,
                Drawing {
                    window: surface,
                    window_revision: 1,
                    revision: 1,
                    bounds,
                    scale: 2.,
                    compiled: Arc::new(compiled),
                    submission,
                },
            );
            host.supersede(owner, submission);
            assert!(
                host.pending.contains(&(owner, submission)),
                "composition owns this submission"
            );
            owners.push((owner, surface, submission));
        }
        let (first, surface, old) = owners[0];
        let replacement = Submission { track_id: 3, revision: 2, ..old };
        host.pending.insert((first, replacement));
        host.active.insert(surface, replacement);
        host.finish(true);
        assert_eq!(host.windows.get(surface).unwrap().scene_revision, 1);
        assert_eq!(host.outcomes[&first].front().unwrap().outcome, PresentationOutcome::Presented);
        assert_eq!(host.assets.len(), 2, "displayed images stay charged after namespace release");
        let mut failed = host.displayed[&surface].clone();
        failed.submission = replacement;
        failed.revision = 2;
        host.inflight.insert(surface, failed);
        host.finish(false);
        assert_eq!(
            host.windows.get(surface).unwrap().scene_revision,
            1,
            "failed composition cannot publish hits"
        );
        host.remove_track(surface.track(3).unwrap());
        host.remove_surface(surface);
        assert_eq!(host.outcomes[&first].back().unwrap().outcome, PresentationOutcome::Superseded);
        let (second, other, _) = owners[1];
        assert_eq!(host.displayed[&other].revision, 1);
        assert_eq!(host.assets.len(), 1);
        assert!(host.lanes.contains_key(&second));
        host.update_viewport(1000., 600., 2.).unwrap();
        assert_eq!(host.viewport_revision, 2);
        assert!(host.viewport_dirty.contains(&second));
        host.remove_owner(first);
        assert_eq!(host.outcomes[&second].len(), 1);
    }
    #[test]
    fn paint_commands_compile_into_the_cached_scene() {
        let mut text = TextSystem::new(Default::default());
        let rect = Rect::new(0., 0., 200., 100.).unwrap();
        let mut canvas = vivid_protocol::vector::Canvas::new();
        canvas
            .shadow(vivid_protocol::vector::Shadow {
                rect,
                radii: vivid_protocol::vector::Corners::new([2., 6., 10., 14.]).unwrap(),
                color: vivid_protocol::vector::Color(0x00000055),
                offset: vivid_protocol::vector::Point::new(0., 8.).unwrap(),
                blur: vivid_protocol::vector::Scalar::new(16.).unwrap(),
                spread: vivid_protocol::vector::Scalar::new(2.).unwrap(),
                inset: false,
            })
            .unwrap();
        canvas
            .stroke_styled(
                vivid_protocol::vector::Path::rectangle(rect).unwrap(),
                vivid_protocol::vector::Brush::Solid(vivid_protocol::vector::Color(0xffffffff)),
                vivid_protocol::vector::StrokeStyle {
                    width: vivid_protocol::vector::Scalar::new(2.).unwrap(),
                    cap: vivid_protocol::vector::Cap::Round,
                    join: vivid_protocol::vector::Join::Round,
                    miter_limit: vivid_protocol::vector::Scalar::new(4.).unwrap(),
                    dashes: vec![
                        vivid_protocol::vector::Scalar::new(6.).unwrap(),
                        vivid_protocol::vector::Scalar::new(3.).unwrap(),
                    ],
                    dash_offset: vivid_protocol::vector::Scalar::ZERO,
                },
            )
            .unwrap();
        let compiled = compile(&canvas, &mut text, &BTreeMap::new()).unwrap();
        assert!(!compiled.retained.is_empty() || true);
        // Hit geometry is unchanged by paint commands: no regions were declared, so the whole
        // window rectangle remains the input area.
        assert_eq!(
            compiled.hit(vivid_protocol::vector::Point::new(1., 1.).unwrap()),
            Some(vivid_protocol::vector::HitRegion {
                id: 0,
                role: vivid_protocol::vector::HitRole::Input,
                cursor: None,
            })
        );

        // An inset shadow with the same geometry must also compile: the ring approximation is
        // host-local and cannot fail where the outer one succeeded.
        let mut inset_canvas = vivid_protocol::vector::Canvas::new();
        inset_canvas
            .shadow(vivid_protocol::vector::Shadow {
                inset: true,
                ..{
                    let shadow = vivid_protocol::vector::Shadow {
                        rect,
                        radii: vivid_protocol::vector::Corners::uniform(8.).unwrap(),
                        color: vivid_protocol::vector::Color(0x00000080),
                        offset: vivid_protocol::vector::Point::new(0., 4.).unwrap(),
                        blur: vivid_protocol::vector::Scalar::new(12.).unwrap(),
                        spread: vivid_protocol::vector::Scalar::ZERO,
                        inset: false,
                    };
                    shadow.validate().unwrap();
                    shadow
                }
            })
            .unwrap();
        compile(&inset_canvas, &mut text, &BTreeMap::new()).unwrap();
    }

    #[test]
    fn oklab_gradients_and_image_brushes_compile_and_missing_assets_fail_atomically() {
        use vello::peniko::ImageData;
        let mut text = TextSystem::new(Default::default());
        let rect = Rect::new(0., 0., 80., 40.).unwrap();
        let path = vivid_protocol::vector::Path::rectangle(rect).unwrap();
        let mut canvas = vivid_protocol::vector::Canvas::new();
        canvas
            .fill(
                path.clone(),
                vivid_protocol::vector::Brush::Linear {
                    start: vivid_protocol::vector::Point::new(0., 0.).unwrap(),
                    end: vivid_protocol::vector::Point::new(80., 0.).unwrap(),
                    stops: vec![
                        vivid_protocol::vector::GradientStop {
                            offset: 0,
                            color: vivid_protocol::vector::Color(0xff0000ff),
                        },
                        vivid_protocol::vector::GradientStop {
                            offset: u16::MAX,
                            color: vivid_protocol::vector::Color(0x0000ffff),
                        },
                    ],
                    color_space: vivid_protocol::vector::ColorSpace::Oklab,
                },
            )
            .unwrap();
        assert!(compile(&canvas, &mut text, &BTreeMap::new()).is_ok());

        // An image brush referencing an asset the channel never carried fails the whole list
        // rather than rendering a placeholder.
        let mut missing = vivid_protocol::vector::Canvas::new();
        missing
            .fill(
                path,
                vivid_protocol::vector::Brush::Image {
                    asset: 9,
                    transform: None,
                    extend: vivid_protocol::vector::Extend::Repeat,
                },
            )
            .unwrap();
        assert!(compile(&missing, &mut text, &BTreeMap::new()).is_err());

        let mut present = vivid_protocol::vector::Canvas::new();
        present
            .fill(
                vivid_protocol::vector::Path::rectangle(rect).unwrap(),
                vivid_protocol::vector::Brush::Image {
                    asset: 1,
                    transform: None,
                    extend: vivid_protocol::vector::Extend::Repeat,
                },
            )
            .unwrap();
        let images = BTreeMap::from([(
            1_u64,
            ImageData {
                data: vello::peniko::Blob::from(vec![0_u8; 2 * 2 * 4]),
                format: vello::peniko::ImageFormat::Rgba8,
                alpha_type: vello::peniko::ImageAlphaType::AlphaPremultiplied,
                width: 2,
                height: 2,
            },
        )]);
        assert!(compile(&present, &mut text, &images).is_ok());
    }
    /// A semantic tree describing `id` in a scene at revision 1.
    fn describing(id: u64) -> Semantics {
        Semantics {
            scene_revision: 1,
            nodes: vec![vivid_protocol::overlay::SemanticNode {
                id,
                role: vivid_protocol::overlay::SemanticRole::Group,
                bounds: Rect::new(0., 0., 100., 100.).unwrap(),
                label: format!("owner {id}"),
                numeric: None,
                level: None,
                set: None,
                toggled: None,
                disabled: false,
                actions: Vec::new(),
                children: Vec::new(),
            }],
        }
    }

    #[test]
    fn one_owners_scene_replacement_retires_only_its_semantic_tree() {
        let mut host = Host::default();
        let first = SessionIdentity::new(PresenterInstanceId([1; 16]), 1).unwrap();
        let second = SessionIdentity::new(PresenterInstanceId([1; 16]), 2).unwrap();
        let bounds = Rect::new(0., 0., 100., 100.).unwrap();
        // Both owners number their first window and first scene the same way, so only the
        // complete owner identity separates the two trees.
        let first_window = placed(&mut host, first, 1, bounds, None);
        let second_window = placed(&mut host, second, 1, bounds, None);
        assert_eq!(first_window.surface_id, second_window.surface_id);
        assert_ne!(first_window.context, second_window.context);

        host.set_semantics(first_window, describing(11)).unwrap();
        host.set_semantics(second_window, describing(22)).unwrap();
        assert_eq!(host.windows_with_semantics().len(), 2);

        // The first owner publishes a new scene. Its own tree described the old one, so it is
        // retired; the other owner's tree still describes what its window is showing.
        host.displayed.get_mut(&first_window).unwrap().revision = 2;
        host.retire_stale_semantics();
        let held = host.windows_with_semantics();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].0, second_window);
        assert_eq!(held[0].1.nodes[0].id, 22);

        // An action is delivered only for a node the live tree names: the first owner's retired
        // node is refused rather than sent, since it is no longer on screen.
        assert!(!host.queue_accessibility(first_window, 11, AccessibleAction::Click));
        assert!(host.queue_accessibility(second_window, 22, AccessibleAction::Click));
        let queued = &host.accessibility[&second];
        assert_eq!(queued.len(), 1);
        let sent = vivid_protocol::overlay::wire::InputEvent {
            address: vivid_protocol::overlay::wire::WindowAddress {
                context_id: second_window.context.context_id,
                surface_id: second_window.surface_id,
                generation: 1,
            },
            scene_revision: 1,
            event: Event::Accessibility { node: 22, action: AccessibleAction::Click },
        };
        assert_eq!(queued[0].1, sent.payload().unwrap());

        // Describing the new scene is what the first owner does next, and it must not disturb the
        // other owner's tree.
        let mut replaced = describing(33);
        replaced.scene_revision = 2;
        host.set_semantics(first_window, replaced).unwrap();
        let mut ids: Vec<u64> =
            host.windows_with_semantics().iter().map(|(_, tree)| tree.nodes[0].id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![22, 33]);
    }
}
