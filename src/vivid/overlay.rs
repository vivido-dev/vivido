//! Authenticated pane-window control. The renderer and input lane share this owner-scoped state.
//!
//! Native terminal targets install viewport geometry before offering the profile bundle.
//! Keeping dispatch here avoids embedding window policy in the terminal loop.

use crate::display::{
    text::TextSystem,
    vector::{CompiledScene, compile},
};
use std::collections::{BTreeMap, HashSet};
use vello::peniko::{Blob, ImageAlphaType, ImageData, ImageFormat};

use vivid_protocol::overlay::wire::{Action, Query, SetWindow, Status, Viewport, WindowAddress};
use vivid_protocol::overlay::{DismissReason, Windows};
use vivid_protocol::vector::Scalar;

use super::*;

#[derive(Default)]
pub(crate) struct Host {
    windows: Windows,
    viewport: Option<Viewport>,
    surfaces: HashSet<SurfaceIdentity>,
    scenes: HashMap<TrackIdentity, (ChannelGeneration, u64, Arc<CompiledScene>)>,
    assets: HashMap<(TrackIdentity, ChannelGeneration), (usize, usize)>,
    lanes: HashMap<SessionIdentity, (u64, Instant)>,
    font: crate::config::font::Font,
}

pub(crate) struct Drawing {
    pub window: SurfaceIdentity,
    pub window_revision: u64,
    pub revision: u64,
    pub bounds: vivid_protocol::vector::Rect,
    pub scale: f64,
    pub compiled: Arc<CompiledScene>,
}

impl Host {
    pub(super) fn has_viewport(&self) -> bool {
        self.viewport.is_some()
    }
    pub(super) fn capturing(&self) -> bool {
        self.windows.has_pointer_capture()
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
        self.viewport = Some(viewport);
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
        })
    }

    pub fn remove_surface(&mut self, identity: SurfaceIdentity) {
        self.windows.close(identity, DismissReason::Closed);
        self.surfaces.remove(&identity);
        self.scenes.retain(|id, _| id.surface != identity);
        self.assets.retain(|(id, _), _| id.surface != identity);
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
        let ids = self.surfaces.iter().copied().filter(|id| id.context.session == owner).collect();
        self.windows.revoke_owner(owner);
        self.surfaces.retain(|id| id.context.session != owner);
        self.scenes.retain(|id, _| id.surface.context.session != owner);
        self.assets.retain(|(id, _), _| id.surface.context.session != owner);
        self.lanes.remove(&owner);
        ids
    }

    pub(super) fn lane_open(&mut self, owner: SessionIdentity, generation: u64) {
        self.lanes.insert(owner, (generation, Instant::now() + Duration::from_secs(5)));
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
        self.windows.keyboard(event, escape)
    }
    pub(super) fn set_pane_focus(&mut self, focused: bool) {
        self.windows.set_pane_focus(focused);
    }

    pub(super) fn pointer(
        &mut self,
        scene: &SharedScene,
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
        let drawings = self.drawing(scene);
        self.windows.pointer(point, button, modifiers, |id, point| {
            drawings.iter().find(|d| d.window == id).and_then(|d| d.compiled.hit(point))
        })
    }

    pub(super) fn wheel(
        &mut self,
        scene: &SharedScene,
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
        let drawings = self.drawing(scene);
        self.windows.wheel(point, dx, dy, modifiers, |id, point| {
            drawings.iter().find(|d| d.window == id).and_then(|d| d.compiled.hit(point))
        })
    }

    pub fn remove_track(&mut self, identity: TrackIdentity) {
        self.scenes.remove(&identity);
        self.assets.retain(|(id, _), _| *id != identity);
    }

    /// A bounded snapshot of immutable scenes. Moving windows only changes placement transforms.
    pub fn drawing(&self, scene: &SharedScene) -> Vec<Drawing> {
        let Some(viewport) = self.viewport else { return Vec::new() };
        self.windows
            .visible()
            .filter_map(|window| {
                let active = scene.active_track(window.identity, vivid_sdk::SLOT_VECTOR)?;
                let active = scene.track_status(active)?;
                let (generation, revision, compiled) = self.scenes.get(&active.identity)?;
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
                })
            })
            .collect()
    }

    pub(super) fn sync_revision(&mut self, scene: &SharedScene, surface: SurfaceIdentity) {
        let Some(active) = scene.active_track(surface, vivid_sdk::SLOT_VECTOR) else { return };
        let Some(active) = scene.track_status(active) else { return };
        let Some((generation, revision, _)) = self.scenes.get(&active.identity) else { return };
        if *generation != active.state.channel_generation {
            return;
        }
        if let Some(window) = self.windows.get(surface)
            && *revision > window.scene_revision
        {
            self.windows
                .publish_scene(surface, window.generation, *revision)
                .expect("validated scene revision under the overlay lock");
        }
    }

    /// Scene revisions are window-scoped, including across track replacement. Validate before
    /// changing either the active slot or its compiled content so input never refers to old hits.
    fn validate_revision(
        &self,
        surface: SurfaceIdentity,
        revision: u64,
    ) -> Result<(), &'static str> {
        let window = self.windows.get(surface).ok_or("overlay window is absent")?;
        if revision == 0 || revision <= window.scene_revision {
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
    text: TextSystem,
    images: BTreeMap<u64, ImageData>,
}
impl VectorWorker {
    pub fn new(font: crate::config::font::Font) -> Self {
        Self { text: TextSystem::new(font), images: BTreeMap::new() }
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
            if self.images.contains_key(&asset.id) {
                return Err("retained image ID already exists");
            }
            let mut host = lock(&shared.overlays);
            let (count, bytes) =
                host.assets.get(&(identity, generation)).copied().unwrap_or_default();
            let next_bytes =
                bytes.checked_add(asset.rgba.len()).ok_or("retained image bytes overflow")?;
            let (owner_count, owner_bytes) = host
                .assets
                .iter()
                .filter(|((track, _), _)| {
                    track.surface.context.session == identity.surface.context.session
                })
                .try_fold((0_usize, 0_usize), |(count, bytes), (_, (n, b))| {
                    Some((count.checked_add(*n)?, bytes.checked_add(*b)?))
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
            host.assets.insert((identity, generation), (count + 1, next_bytes));
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
            let compiled =
                Arc::new(compile(&frame.canvas, &mut self.text, &self.images).map_err(|e| e.0)?);
            let mut host = lock(&shared.overlays);
            if !host.surfaces.contains(&identity.surface) {
                return Err("vector surface has no overlay window");
            }
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
            host.scenes.insert(identity, (generation, frame.revision, compiled));
            shared.scene.mark_output_ready(identity, generation)?;
            host.sync_revision(&shared.scene, identity.surface);
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
                host.windows
                    .create(identity, request.address.generation, request.options.clone())
                    .map(|()| 1)
            } else {
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
    if !session.supports(registry::OVERLAY_INPUT) {
        return;
    }
    let mut host = lock(&shared.overlays);
    let expired =
        host.lanes.get(&session.identity).is_some_and(|(_, deadline)| Instant::now() >= *deadline);
    let mut failed = expired || host.windows.take_overflow(session.identity);
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
        for (width, height, scale) in [(0., 600., 1.), (800., 600., 0.), (800., 600., f64::NAN)] {
            assert!(host.update_viewport(width, height, scale).is_err());
            assert_eq!(host.viewport, previous);
        }
    }
}
