//! Authenticated pane-window control. The renderer and input lane share this owner-scoped state.
//!
//! Native terminal targets install viewport geometry before offering the profile bundle.
//! Keeping dispatch here avoids embedding window policy in the terminal loop.

use crate::display::{text::TextSystem, vector::CompiledScene};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Weak;
use vello::peniko::{Blob, ImageAlphaType, ImageData, ImageFormat};

use vivid_protocol::overlay::wire::{Action, Query, SetWindow, Status, Viewport, WindowAddress};
use vivid_protocol::overlay::wire::{
    PresentationOutcome, Submission, SubmissionOutcome, ViewportChanged,
};
use vivid_protocol::overlay::{DismissReason, Windows};
use vivid_protocol::vector::Scalar;

use super::*;
#[cfg(test)]
use crate::display::vector::compile;

#[path = "overlay_text.rs"]
pub(super) mod text;

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
    pub(super) actors: HashMap<SessionIdentity, Weak<SessionRuntime>>,
    lanes: HashMap<SessionIdentity, (u64, Instant)>,
    font: crate::config::font::Font,
    compiled_scenes: u64,
    presented_scenes: u64,
    superseded_scenes: u64,
    window_updates: u64,
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
        let consumed = self.windows.keyboard(event, escape);
        self.editor_rect();
        consumed
    }
    pub(super) fn set_pane_focus(&mut self, focused: bool) {
        if !focused {
            self.editor = None;
        }
        self.windows.set_pane_focus(focused);
    }

    pub(super) fn pointer(
        &mut self,
        _scene: &SharedScene,
        x: f64,
        y: f64,
        button: Option<(u16, bool)>,
        modifiers: u32,
    ) -> bool {
        let Some(viewport) = self.viewport else {
            return false;
        };
        let scale = f64::from(viewport.scale_numerator) / f64::from(viewport.scale_denominator);
        let Ok(point) = vivid_protocol::vector::Point::new(x / scale, y / scale) else {
            return false;
        };
        let drawings: Vec<_> = self.displayed.values().cloned().collect();
        let consumed = self.windows.pointer(point, button, modifiers, |id, point| {
            drawings.iter().find(|d| d.window == id).and_then(|d| d.compiled.hit(point))
        });
        self.editor_rect();
        consumed
    }

    pub(super) fn wheel(
        &mut self,
        _scene: &SharedScene,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        modifiers: u32,
    ) -> bool {
        let Some(viewport) = self.viewport else {
            return false;
        };
        let scale = f64::from(viewport.scale_numerator) / f64::from(viewport.scale_denominator);
        let (Ok(point), Ok(dx), Ok(dy)) = (
            vivid_protocol::vector::Point::new(x / scale, y / scale),
            Scalar::new(dx / scale),
            Scalar::new(dy / scale),
        ) else {
            return false;
        };
        let drawings: Vec<_> = self.displayed.values().cloned().collect();
        self.windows.wheel(point, dx, dy, modifiers, |id, point| {
            drawings.iter().find(|d| d.window == id).and_then(|d| d.compiled.hit(point))
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
            request.expected_revision = if request.expected_revision == 0 {
                if host.surfaces.contains(&identity) {
                    return Err(ControlError::state(
                        "an overlay surface cannot be reopened after dismissal",
                    ));
                }
                host.windows
                    .create(identity, request.address.generation, request.options.clone())
                    .map(|()| 1)
            } else {
                host.window_updates = host.window_updates.saturating_add(1);
                host.windows.update(
                    identity,
                    request.address.generation,
                    request.expected_revision,
                    request.options.clone(),
                )
            }
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
            host.windows
                .apply_action(session.identity, action, viewport)
                .map_err(|e| ControlError::state(e.0))?;
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
                    host.windows
                        .capture_pointer(
                            id,
                            vivid_protocol::vector::Point::new(0., 0.).map_err(|e| e.0)?,
                        )
                        .map_err(|e| e.0)?;
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
}
