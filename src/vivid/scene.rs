//! Owner-scoped Vivid 1.5 surface, track, and retained-scene state.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};

use vivid_protocol::identity::{
    ContextIdentity, NodeIdentity, SessionIdentity, SurfaceIdentity, TrackIdentity,
};
use vivid_protocol::revision::{
    ChannelGeneration, SceneRevision, SurfaceGeneration, SurfaceRevision, TargetGeneration,
    TrackRevision,
};
use vivid_protocol::scene::SceneNode;
use vivid_protocol::surface::SurfaceDefinition;
use vivid_protocol::track::{
    KindConfiguration, MILESTONE_BUFFERED_ENDED, MILESTONE_CHANNEL_ACCEPTED,
    MILESTONE_CHANNEL_DETACHED, MILESTONE_CLOCK_STARTED, MILESTONE_OUTPUT_READY,
    MILESTONE_PRESENTED, MILESTONE_TRACK_LOST, TrackConfiguration, TrackState,
};

pub type SurfaceKey = SurfaceIdentity;
pub type TrackKey = TrackIdentity;

pub const SLOT_PRIMARY_VIDEO: u64 = 1;
pub const SLOT_AUDIO: u64 = 2;
pub const SLOT_RASTER: u64 = 3;
pub const SLOT_POSTER: u64 = 4;
pub const ALPHA_STRAIGHT: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RasterDamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub frame_id: u64,
    pub pts_us: i64,
    pub width: u32,
    pub height: u32,
    pub sar_num: u32,
    pub sar_den: u32,
    pub alpha_mode: u64,
    pub rgba: Arc<[u8]>,
    pub damage: Option<Arc<[RasterDamageRect]>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipRect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

#[derive(Debug, Clone)]
pub struct RenderItem {
    pub track_key: TrackKey,
    pub surface_key: SurfaceKey,
    pub surface_generation: SurfaceGeneration,
    pub channel_generation: ChannelGeneration,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub text_anchored: bool,
    pub text_layer: u64,
    pub z_index: i64,
    pub clip: Option<ClipRect>,
    pub frame: Arc<Frame>,
    pub capture_policy: u64,
}

#[derive(Debug, Clone)]
pub struct SurfaceStatus {
    pub identity: SurfaceIdentity,
    pub revision: SurfaceRevision,
    pub generation: SurfaceGeneration,
    pub definition: SurfaceDefinition,
    pub active_slots: BTreeMap<u64, u64>,
    pub lifecycle: u64,
}

#[derive(Debug, Clone)]
pub struct TrackStatus {
    pub identity: TrackIdentity,
    pub configuration: TrackConfiguration,
    pub state: TrackState,
    pub lifecycle: u64,
    pub last_decoded_pts_us: Option<i64>,
    pub last_presented_pts_us: Option<i64>,
    pub last_presentation_id: u64,
    pub maximum_channel_bytes: u64,
    pub maximum_channel_records: u64,
}

#[derive(Debug, Clone)]
pub struct SceneNodeStatus {
    pub identity: NodeIdentity,
    pub node: SceneNode,
}

#[derive(Debug, Clone)]
pub struct SceneStatus {
    pub session: SessionIdentity,
    pub revision: SceneRevision,
    pub target_generation: TargetGeneration,
    pub nodes: Vec<SceneNodeStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackWaitSatisfied {
    pub revision: TrackRevision,
    pub channel_generation: ChannelGeneration,
    pub observed_value: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackWaitEvaluation {
    Satisfied(TrackWaitSatisfied),
    Pending,
    NotVisible,
    NotFound,
    StaleGeneration,
}

#[derive(Debug, Clone)]
struct Surface {
    definition: SurfaceDefinition,
    revision: SurfaceRevision,
    generation: SurfaceGeneration,
    active_slots: BTreeMap<u64, u64>,
    lifecycle: u64,
}

#[derive(Debug, Clone)]
struct Track {
    configuration: TrackConfiguration,
    state: TrackState,
    lifecycle: u64,
    frame: Option<Arc<Frame>>,
    last_decoded_pts_us: Option<i64>,
    last_presented_pts_us: Option<i64>,
    last_presentation_id: u64,
    maximum_channel_bytes: u64,
    maximum_channel_records: u64,
}

#[derive(Debug, Clone)]
struct SessionScene {
    revision: SceneRevision,
    target_generation: TargetGeneration,
    nodes: BTreeMap<NodeIdentity, SceneNode>,
    pending: BTreeMap<(ContextIdentity, u64), Vec<NodeMutation>>,
}

#[derive(Debug, Clone)]
enum NodeMutation {
    Create(SceneNode),
    Update(SceneNode),
    Delete(NodeIdentity),
}

#[derive(Default)]
struct State {
    surfaces: HashMap<SurfaceIdentity, Surface>,
    tracks: HashMap<TrackIdentity, Track>,
    scenes: HashMap<SessionIdentity, SessionScene>,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Clone)]
pub struct SharedScene(Arc<Inner>);

impl Default for SharedScene {
    fn default() -> Self {
        Self(Arc::new(Inner { state: Mutex::new(State::default()), changed: Condvar::new() }))
    }
}

impl SharedScene {
    pub fn register_session(
        &self,
        session: SessionIdentity,
        target_generation: TargetGeneration,
    ) -> Result<(), &'static str> {
        target_generation.require_nonzero().map_err(|_| "target generation must be nonzero")?;
        let mut state = self.lock();
        if state.scenes.contains_key(&session) {
            return Err("session already exists");
        }
        state.scenes.insert(
            session,
            SessionScene {
                revision: SceneRevision::ZERO,
                target_generation,
                nodes: BTreeMap::new(),
                pending: BTreeMap::new(),
            },
        );
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn remove_session(&self, session: SessionIdentity) {
        let mut state = self.lock();
        state.surfaces.retain(|identity, _| identity.context.session != session);
        state.tracks.retain(|identity, _| identity.surface.context.session != session);
        state.scenes.remove(&session);
        self.0.changed.notify_all();
    }

    pub fn remove_contexts(&self, session: SessionIdentity, contexts: &HashSet<u64>) {
        let mut state = self.lock();
        state.surfaces.retain(|identity, _| {
            identity.context.session != session || !contexts.contains(&identity.context.context_id)
        });
        state.tracks.retain(|identity, _| {
            identity.surface.context.session != session
                || !contexts.contains(&identity.surface.context.context_id)
        });
        if let Some(scene) = state.scenes.get_mut(&session) {
            let before = scene.nodes.len();
            scene.nodes.retain(|identity, node| {
                !contexts.contains(&identity.context.context_id)
                    && !contexts.contains(&node.surface_context_id)
            });
            scene.pending.retain(|(identity, _), _| !contexts.contains(&identity.context_id));
            if scene.nodes.len() != before
                && let Ok(next) = scene.revision.advance()
            {
                scene.revision = next;
            }
        }
        self.0.changed.notify_all();
    }

    pub fn create_surface(
        &self,
        identity: SurfaceIdentity,
        definition: SurfaceDefinition,
    ) -> Result<SurfaceStatus, &'static str> {
        definition.validate().map_err(|_| "invalid surface definition")?;
        if definition.context_id != identity.context.context_id
            || definition.surface_id != identity.surface_id
        {
            return Err("surface owner does not match complete identity");
        }
        let mut state = self.lock();
        if !state.scenes.contains_key(&identity.context.session) {
            return Err("owning session does not exist");
        }
        if state.surfaces.contains_key(&identity) {
            return Err("surface already exists");
        }
        let surface = Surface {
            definition,
            revision: SurfaceRevision::ONE,
            generation: SurfaceGeneration::ONE,
            active_slots: BTreeMap::new(),
            lifecycle: 1,
        };
        let status = surface_status(identity, &surface);
        state.surfaces.insert(identity, surface);
        self.0.changed.notify_all();
        Ok(status)
    }

    pub fn update_surface(
        &self,
        identity: SurfaceIdentity,
        expected_revision: SurfaceRevision,
        expected_generation: SurfaceGeneration,
        replacement: SurfaceDefinition,
    ) -> Result<SurfaceStatus, &'static str> {
        replacement.validate().map_err(|_| "invalid surface replacement")?;
        let mut state = self.lock();
        let surface = state.surfaces.get_mut(&identity).ok_or("surface does not exist")?;
        if surface.revision != expected_revision || surface.generation != expected_generation {
            return Err("stale surface revision or generation");
        }
        if replacement.context_id != identity.context.context_id
            || replacement.surface_id != identity.surface_id
            || replacement.semantic_profile != surface.definition.semantic_profile
            || replacement.coordinate_model != surface.definition.coordinate_model
        {
            return Err("surface update changes immutable identity or profile");
        }
        if replacement.policy & surface.definition.policy != surface.definition.policy {
            return Err("surface policy update is not monotonic");
        }
        let mapping_changed = replacement.logical_width != surface.definition.logical_width
            || replacement.logical_height != surface.definition.logical_height
            || replacement.scale_numerator != surface.definition.scale_numerator
            || replacement.scale_denominator != surface.definition.scale_denominator
            || replacement.rotation != surface.definition.rotation
            || replacement.profile_parameters != surface.definition.profile_parameters;
        surface.revision = surface.revision.advance().map_err(|_| "surface revision exhausted")?;
        if mapping_changed {
            surface.generation =
                surface.generation.advance().map_err(|_| "surface generation exhausted")?;
        }
        surface.definition = replacement;
        let result = surface_status(identity, surface);
        self.0.changed.notify_all();
        Ok(result)
    }

    pub fn destroy_surface(&self, identity: SurfaceIdentity) -> Result<(), &'static str> {
        let mut state = self.lock();
        if state.surfaces.remove(&identity).is_none() {
            return Err("surface does not exist");
        }
        state.tracks.retain(|key, _| key.surface != identity);
        if let Some(scene) = state.scenes.get_mut(&identity.context.session) {
            let before = scene.nodes.len();
            scene.nodes.retain(|_, node| {
                node.surface_context_id != identity.context.context_id
                    || node.surface_id != identity.surface_id
            });
            if scene.nodes.len() != before {
                scene.revision =
                    scene.revision.advance().map_err(|_| "scene revision exhausted")?;
            }
        }
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn surface_status(&self, identity: SurfaceIdentity) -> Option<SurfaceStatus> {
        self.lock().surfaces.get(&identity).map(|surface| surface_status(identity, surface))
    }

    pub fn surface_keys(&self) -> Vec<SurfaceIdentity> {
        let mut keys = self.lock().surfaces.keys().copied().collect::<Vec<_>>();
        keys.sort();
        keys
    }

    pub fn create_track(
        &self,
        identity: TrackIdentity,
        configuration: TrackConfiguration,
    ) -> Result<TrackStatus, &'static str> {
        configuration.validate(false).map_err(|_| "invalid track configuration")?;
        if configuration.context_id != identity.surface.context.context_id
            || configuration.surface_id != identity.surface.surface_id
            || configuration.track_id != identity.track_id
        {
            return Err("track owner does not match complete identity");
        }
        if configuration.slot > SLOT_POSTER && configuration.slot < 32 {
            return Err("reserved surface slot");
        }
        if configuration.slot >= 32 {
            return Err("auxiliary slots are unsupported by the terminal target");
        }
        let mut state = self.lock();
        if !state.surfaces.contains_key(&identity.surface) {
            return Err("owning surface does not exist");
        }
        if state.tracks.contains_key(&identity) {
            return Err("track already exists");
        }
        let track = Track {
            configuration,
            state: TrackState::new(),
            lifecycle: 1,
            frame: None,
            last_decoded_pts_us: None,
            last_presented_pts_us: None,
            last_presentation_id: 0,
            maximum_channel_bytes: 0,
            maximum_channel_records: 0,
        };
        let status = track_status(identity, &track);
        state.tracks.insert(identity, track);
        self.0.changed.notify_all();
        Ok(status)
    }

    pub fn destroy_track(&self, identity: TrackIdentity) -> Result<(), &'static str> {
        let mut state = self.lock();
        if state.tracks.remove(&identity).is_none() {
            return Err("track does not exist");
        }
        if let Some(surface) = state.surfaces.get_mut(&identity.surface) {
            surface.active_slots.retain(|_, track_id| *track_id != identity.track_id);
            surface.revision =
                surface.revision.advance().map_err(|_| "surface revision exhausted")?;
        }
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn activate_tracks(
        &self,
        surface_identity: SurfaceIdentity,
        expected_revision: SurfaceRevision,
        bindings: &[(u64, u64, ChannelGeneration, u64)],
    ) -> Result<SurfaceStatus, &'static str> {
        let mut state = self.lock();
        let surface = state.surfaces.get(&surface_identity).ok_or("surface does not exist")?;
        if surface.revision != expected_revision {
            return Err("stale surface revision");
        }
        let mut candidate = surface.active_slots.clone();
        for &(slot, track_id, expected_generation, required_milestone) in bindings {
            if slot > SLOT_POSTER || slot == 0 {
                return Err("unsupported surface slot");
            }
            if track_id == 0 {
                if expected_generation != ChannelGeneration::ZERO || required_milestone != 0 {
                    return Err("cleared slot has a generation or milestone");
                }
                candidate.remove(&slot);
                continue;
            }
            let track_identity = TrackIdentity { surface: surface_identity, track_id };
            let track = state.tracks.get(&track_identity).ok_or("track does not exist")?;
            if track.configuration.slot != slot
                || track.state.channel_generation != expected_generation
                || required_milestone.count_ones() != 1
                || track.state.milestones & required_milestone == 0
                || track.lifecycle != 1
            {
                return Err("track is not eligible for the requested slot");
            }
            candidate.insert(slot, track_id);
        }
        let surface = state.surfaces.get_mut(&surface_identity).unwrap();
        surface.active_slots = candidate;
        surface.revision = surface.revision.advance().map_err(|_| "surface revision exhausted")?;
        let status = surface_status(surface_identity, surface);
        self.0.changed.notify_all();
        Ok(status)
    }

    pub fn track_status(&self, identity: TrackIdentity) -> Option<TrackStatus> {
        self.lock().tracks.get(&identity).map(|track| track_status(identity, track))
    }

    pub fn latest_frame(&self, identity: TrackIdentity) -> Option<Arc<Frame>> {
        self.lock().tracks.get(&identity).and_then(|track| track.frame.clone())
    }

    pub fn track_keys(&self) -> Vec<TrackIdentity> {
        let mut keys = self.lock().tracks.keys().copied().collect::<Vec<_>>();
        keys.sort();
        keys
    }

    pub fn accept_channel(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
        maximum_bytes: u64,
        maximum_records: u64,
    ) -> Result<TrackStatus, &'static str> {
        if maximum_bytes == 0 || maximum_records == 0 {
            return Err("channel flow maxima must be positive");
        }
        let mut state = self.lock();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        if track.state.channel_generation != generation || track.lifecycle != 1 {
            return Err("stale channel generation");
        }
        track
            .state
            .accept_channel(
                generation,
                maximum_bytes,
                maximum_records,
                track.configuration.maximum_record_body,
            )
            .map_err(|_| "channel flow is not admissible")?;
        track.maximum_channel_bytes = maximum_bytes;
        track.maximum_channel_records = maximum_records;
        let result = track_status(identity, track);
        self.0.changed.notify_all();
        Ok(result)
    }

    pub fn advance_channel(&self, identity: TrackIdentity) -> Result<TrackStatus, &'static str> {
        let mut state = self.lock();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        let current = track.state.channel_generation;
        let next = current.advance().map_err(|_| "channel generation exhausted")?;
        track
            .state
            .advance_channel(current, next)
            .map_err(|_| "channel generation could not advance")?;
        track.maximum_channel_bytes = 0;
        track.maximum_channel_records = 0;
        track.frame = None;
        let result = track_status(identity, track);
        self.0.changed.notify_all();
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn publish_frame(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
        body_length: u32,
        media_epoch: u32,
        media_id: u64,
        random_access: bool,
        frame: Frame,
    ) -> Result<(), &'static str> {
        let expected_len = usize::try_from(
            u64::from(frame.width)
                .checked_mul(u64::from(frame.height))
                .and_then(|value| value.checked_mul(4))
                .ok_or("decoded frame dimensions overflow")?,
        )
        .map_err(|_| "decoded frame is too large")?;
        if frame.width == 0
            || frame.height == 0
            || frame.rgba.len() != expected_len
            || frame.sar_num == 0
            || frame.sar_den == 0
        {
            return Err("invalid decoded frame");
        }
        self.admit_media(identity, generation, body_length, media_epoch, media_id, random_access)?;
        self.publish_decoded_frame(identity, generation, frame)
    }

    pub fn detach_channel(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        if track.state.channel_generation != generation || track.lifecycle != 1 {
            return Err("stale channel generation");
        }
        track.state.detach().map_err(|_| "track revision exhausted")?;
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn admit_media(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
        body_length: u32,
        media_epoch: u32,
        media_id: u64,
        random_access: bool,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        if track.state.channel_generation != generation || track.lifecycle != 1 {
            return Err("stale channel generation");
        }
        track
            .state
            .admit_media(generation, body_length, media_epoch, media_id, random_access)
            .map_err(|_| "stale media epoch, media ID, or flow allowance")?;
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn publish_decoded_frame(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
        frame: Frame,
    ) -> Result<(), &'static str> {
        let expected_len = usize::try_from(
            u64::from(frame.width)
                .checked_mul(u64::from(frame.height))
                .and_then(|value| value.checked_mul(4))
                .ok_or("decoded frame dimensions overflow")?,
        )
        .map_err(|_| "decoded frame is too large")?;
        if frame.width == 0
            || frame.height == 0
            || frame.rgba.len() != expected_len
            || frame.sar_num == 0
            || frame.sar_den == 0
        {
            return Err("invalid decoded frame");
        }
        let mut state = self.lock();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        if track.state.channel_generation != generation || track.lifecycle != 1 {
            return Err("stale channel generation");
        }
        track.last_decoded_pts_us = Some(frame.pts_us);
        track.frame = Some(Arc::new(frame));
        track.state.milestones |= MILESTONE_OUTPUT_READY;
        track.state.revision =
            track.state.revision.advance().map_err(|_| "track revision exhausted")?;
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn lose_track(&self, identity: TrackIdentity) -> Result<(), &'static str> {
        let mut state = self.lock();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        track.lifecycle = 2;
        track.state.lose().map_err(|_| "track revision exhausted")?;
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn mark_output_ready(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        if track.state.channel_generation != generation || track.lifecycle != 1 {
            return Err("stale channel generation");
        }
        if track.state.milestones & MILESTONE_OUTPUT_READY == 0 {
            track.state.milestones |= MILESTONE_OUTPUT_READY;
            track.state.revision =
                track.state.revision.advance().map_err(|_| "track revision exhausted")?;
            self.0.changed.notify_all();
        }
        Ok(())
    }

    pub fn return_channel_capacity(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
        body_bytes: u64,
        records: u64,
    ) -> Result<(u64, u64), &'static str> {
        let mut state = self.lock();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        if track.state.channel_generation != generation || track.lifecycle != 1 {
            return Err("stale channel generation");
        }
        let maximum_bytes = track
            .state
            .flow
            .maximum_body_bytes
            .checked_add(body_bytes)
            .ok_or("channel byte maximum exhausted")?;
        let maximum_records = track
            .state
            .flow
            .maximum_media_records
            .checked_add(records)
            .ok_or("channel record maximum exhausted")?;
        track.state.flow.raise_maxima(maximum_bytes, maximum_records);
        track.maximum_channel_bytes = maximum_bytes;
        track.maximum_channel_records = maximum_records;
        Ok((maximum_bytes, maximum_records))
    }

    pub fn begin_transaction(
        &self,
        context: ContextIdentity,
        transaction_id: u64,
    ) -> Result<(), &'static str> {
        if transaction_id == 0 {
            return Err("transaction ID must be nonzero");
        }
        let mut state = self.lock();
        let scene = state.scenes.get_mut(&context.session).ok_or("session does not exist")?;
        if scene.pending.insert((context, transaction_id), Vec::new()).is_some() {
            return Err("transaction already exists");
        }
        Ok(())
    }

    pub fn queue_node_create(
        &self,
        context: ContextIdentity,
        transaction_id: u64,
        node: SceneNode,
    ) -> Result<(), &'static str> {
        self.queue_mutation(context, transaction_id, NodeMutation::Create(node))
    }

    pub fn queue_node_update(
        &self,
        context: ContextIdentity,
        transaction_id: u64,
        node: SceneNode,
    ) -> Result<(), &'static str> {
        self.queue_mutation(context, transaction_id, NodeMutation::Update(node))
    }

    pub fn queue_node_delete(
        &self,
        context: ContextIdentity,
        transaction_id: u64,
        node: NodeIdentity,
    ) -> Result<(), &'static str> {
        self.queue_mutation(context, transaction_id, NodeMutation::Delete(node))
    }

    pub fn abort_transaction(&self, context: ContextIdentity, transaction_id: u64) -> bool {
        self.lock()
            .scenes
            .get_mut(&context.session)
            .and_then(|scene| scene.pending.remove(&(context, transaction_id)))
            .is_some()
    }

    pub fn commit_transaction(
        &self,
        context: ContextIdentity,
        transaction_id: u64,
        expected_target: TargetGeneration,
        expected_revision: Option<SceneRevision>,
    ) -> Result<SceneRevision, &'static str> {
        let mut state = self.lock();
        let session = context.session;
        let mutations = state
            .scenes
            .get(&session)
            .and_then(|scene| scene.pending.get(&(context, transaction_id)))
            .cloned()
            .ok_or("transaction does not exist")?;
        {
            let scene = state.scenes.get(&session).unwrap();
            if scene.target_generation != expected_target {
                return Err("stale target generation");
            }
            if expected_revision.is_some_and(|revision| revision != scene.revision) {
                return Err("stale scene revision");
            }
        }
        let mut candidate = state.scenes.get(&session).unwrap().nodes.clone();
        for mutation in mutations {
            match mutation {
                NodeMutation::Create(node) => {
                    validate_terminal_node(&node)?;
                    let identity = context.node(node.node_id).map_err(|_| "invalid node ID")?;
                    if node.owning_context_id != context.context_id
                        || candidate.contains_key(&identity)
                    {
                        return Err("duplicate or misowned node");
                    }
                    let surface_identity = context
                        .session
                        .context(node.surface_context_id)
                        .map_err(|_| "invalid surface context")?
                        .surface(node.surface_id)
                        .map_err(|_| "invalid surface ID")?;
                    if !state.surfaces.contains_key(&surface_identity) {
                        return Err("node references a missing surface");
                    }
                    candidate.insert(identity, node);
                },
                NodeMutation::Update(node) => {
                    validate_terminal_node(&node)?;
                    let identity = context.node(node.node_id).map_err(|_| "invalid node ID")?;
                    if node.owning_context_id != context.context_id
                        || !candidate.contains_key(&identity)
                    {
                        return Err("missing or misowned node");
                    }
                    candidate.insert(identity, node);
                },
                NodeMutation::Delete(identity) => {
                    if identity.context != context || candidate.remove(&identity).is_none() {
                        return Err("missing or misowned node");
                    }
                },
            }
        }
        let scene = state.scenes.get_mut(&session).unwrap();
        scene.revision = scene.revision.advance().map_err(|_| "scene revision exhausted")?;
        scene.nodes = candidate;
        scene.pending.remove(&(context, transaction_id));
        let revision = scene.revision;
        self.0.changed.notify_all();
        Ok(revision)
    }

    pub fn scene_status(&self, session: SessionIdentity, maximum_nodes: usize) -> SceneStatus {
        let state = self.lock();
        let Some(scene) = state.scenes.get(&session) else {
            return SceneStatus {
                session,
                revision: SceneRevision::ZERO,
                target_generation: TargetGeneration::ZERO,
                nodes: Vec::new(),
            };
        };
        SceneStatus {
            session,
            revision: scene.revision,
            target_generation: scene.target_generation,
            nodes: scene
                .nodes
                .iter()
                .take(maximum_nodes)
                .map(|(identity, node)| SceneNodeStatus { identity: *identity, node: node.clone() })
                .collect(),
        }
    }

    pub fn snapshot(&self) -> (u64, Vec<RenderItem>) {
        let state = self.lock();
        let revision = state
            .scenes
            .values()
            .fold(0_u64, |value, scene| value.wrapping_add(scene.revision.get()));
        let mut items = Vec::new();
        for (session_identity, scene) in &state.scenes {
            for node in scene.nodes.values().filter(|node| node.visible) {
                let Some((x, y, width, height, text_layer, text_anchored)) =
                    terminal_geometry(node)
                else {
                    continue;
                };
                let context = session_identity.context(node.surface_context_id).ok();
                let Some(surface_key) = context.and_then(|context| {
                    context
                        .surface(node.surface_id)
                        .ok()
                        .filter(|key| state.surfaces.contains_key(key))
                }) else {
                    continue;
                };
                let surface = &state.surfaces[&surface_key];
                let selected =
                    [SLOT_PRIMARY_VIDEO, SLOT_RASTER, SLOT_POSTER].into_iter().find_map(|slot| {
                        let track_id = *surface.active_slots.get(&slot)?;
                        let key = TrackIdentity { surface: surface_key, track_id };
                        let track = state.tracks.get(&key)?;
                        (track.lifecycle == 1)
                            .then_some(track.frame.as_ref().map(|frame| (key, track, frame)))
                            .flatten()
                    });
                let Some((track_key, track, frame)) = selected else {
                    continue;
                };
                items.push(RenderItem {
                    track_key,
                    surface_key,
                    surface_generation: surface.generation,
                    channel_generation: track.state.channel_generation,
                    x,
                    y,
                    width,
                    height,
                    text_anchored,
                    text_layer,
                    z_index: node.z_index,
                    clip: terminal_clip(node),
                    frame: frame.clone(),
                    capture_policy: surface.definition.policy,
                });
            }
        }
        items.sort_by_key(|item| (item.text_layer, item.z_index));
        (revision, items)
    }

    pub fn mark_presented(
        &self,
        identity: TrackIdentity,
        channel_generation: ChannelGeneration,
        surface_generation: SurfaceGeneration,
        frame_id: u64,
        pts_us: i64,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let surface = state.surfaces.get(&identity.surface).ok_or("surface does not exist")?;
        if surface.generation != surface_generation {
            return Err("stale surface generation");
        }
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        if track.state.channel_generation != channel_generation
            || track.frame.as_ref().is_none_or(|frame| frame.frame_id != frame_id)
        {
            return Err("stale frame presentation");
        }
        track.last_presented_pts_us = Some(pts_us);
        track.last_presentation_id =
            track.last_presentation_id.checked_add(1).ok_or("presentation ID exhausted")?;
        track.state.milestones |= MILESTONE_PRESENTED;
        track.state.revision =
            track.state.revision.advance().map_err(|_| "track revision exhausted")?;
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn evaluate_track_wait(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
        condition: u64,
        value: Option<u64>,
    ) -> TrackWaitEvaluation {
        let state = self.lock();
        let Some(track) = state.tracks.get(&identity) else {
            return TrackWaitEvaluation::NotFound;
        };
        if track.state.channel_generation != generation {
            return TrackWaitEvaluation::StaleGeneration;
        }
        if matches!(condition, 3 | 4)
            && !state.scenes.get(&identity.surface.context.session).is_some_and(|scene| {
                scene.nodes.values().any(|node| {
                    node.visible
                        && node.surface_context_id == identity.surface.context.context_id
                        && node.surface_id == identity.surface.surface_id
                })
            })
        {
            return TrackWaitEvaluation::NotVisible;
        }
        let observed = match condition {
            1 => track.state.revision.get(),
            2 => track.state.milestones,
            3 => track.last_presentation_id,
            4 => track.last_presented_pts_us.and_then(|pts| u64::try_from(pts).ok()).unwrap_or(0),
            5 => u64::from(track.state.milestones & MILESTONE_CLOCK_STARTED != 0),
            6 => u64::from(track.state.milestones & MILESTONE_BUFFERED_ENDED != 0),
            7 => u64::from(track.state.milestones & MILESTONE_CHANNEL_ACCEPTED != 0),
            8 => u64::from(track.state.milestones & MILESTONE_CHANNEL_DETACHED != 0),
            9 => u64::from(track.state.milestones & MILESTONE_TRACK_LOST != 0),
            _ => return TrackWaitEvaluation::Pending,
        };
        let satisfied = match condition {
            1 | 3 | 4 => value.is_some_and(|minimum| observed > minimum),
            2 => value.is_some_and(|mask| observed & mask == mask),
            5..=9 => observed == 1,
            _ => false,
        };
        if satisfied {
            TrackWaitEvaluation::Satisfied(TrackWaitSatisfied {
                revision: track.state.revision,
                channel_generation: track.state.channel_generation,
                observed_value: observed,
            })
        } else {
            TrackWaitEvaluation::Pending
        }
    }

    pub fn session_ids(&self) -> Vec<SessionIdentity> {
        let mut sessions = self.lock().scenes.keys().copied().collect::<Vec<_>>();
        sessions.sort();
        sessions
    }

    fn queue_mutation(
        &self,
        context: ContextIdentity,
        transaction_id: u64,
        mutation: NodeMutation,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        state
            .scenes
            .get_mut(&context.session)
            .and_then(|scene| scene.pending.get_mut(&(context, transaction_id)))
            .ok_or("transaction does not exist")?
            .push(mutation);
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn surface_status(identity: SurfaceIdentity, surface: &Surface) -> SurfaceStatus {
    SurfaceStatus {
        identity,
        revision: surface.revision,
        generation: surface.generation,
        definition: surface.definition.clone(),
        active_slots: surface.active_slots.clone(),
        lifecycle: surface.lifecycle,
    }
}

fn track_status(identity: TrackIdentity, track: &Track) -> TrackStatus {
    TrackStatus {
        identity,
        configuration: track.configuration.clone(),
        state: track.state.clone(),
        lifecycle: track.lifecycle,
        last_decoded_pts_us: track.last_decoded_pts_us,
        last_presented_pts_us: track.last_presented_pts_us,
        last_presentation_id: track.last_presentation_id,
        maximum_channel_bytes: track.maximum_channel_bytes,
        maximum_channel_records: track.maximum_channel_records,
    }
}

fn validate_terminal_node(node: &SceneNode) -> Result<(), &'static str> {
    node.validate().map_err(|_| "invalid scene node")?;
    terminal_geometry(node).ok_or("invalid terminal node geometry")?;
    if terminal_clip(node).is_none() && node.clip.is_some() {
        return Err("invalid terminal node clip");
    }
    Ok(())
}

fn terminal_geometry(node: &SceneNode) -> Option<(i64, i64, i64, i64, u64, bool)> {
    let value = |key| node.geometry.iter().find(|entry| entry.0 == key).map(|entry| &entry.1);
    let coordinate_space = value(0)?.as_u64()?;
    let x = value(1)?.as_i64()?;
    let y = value(2)?.as_i64()?;
    let width = value(3)?.as_i64()?;
    let height = value(4)?.as_i64()?;
    let layer = value(5)?.as_u64()?;
    if width <= 0
        || height <= 0
        || layer > 2
        || !matches!(coordinate_space, 1 | 2)
        || (coordinate_space == 1 && node.geometry.len() != 6)
        || (coordinate_space == 2
            && (node.geometry.len() != 8 || value(6)?.as_u64()? == 0 || value(7)?.as_u64()? == 0))
    {
        return None;
    }
    Some((x, y, width, height, layer, coordinate_space == 2))
}

fn terminal_clip(node: &SceneNode) -> Option<ClipRect> {
    let clip = node.clip.as_ref()?;
    if clip.len() != 4 {
        return None;
    }
    let value = |key| clip.iter().find(|entry| entry.0 == key).map(|entry| &entry.1);
    let result = ClipRect {
        x: value(0)?.as_i64()?,
        y: value(1)?.as_i64()?,
        width: value(2)?.as_i64()?,
        height: value(3)?.as_i64()?,
    };
    (result.width > 0 && result.height > 0).then_some(result)
}

pub fn track_kind_name(configuration: &TrackConfiguration) -> &'static str {
    match configuration.kind {
        KindConfiguration::Video(_) => "video",
        KindConfiguration::Audio(_) => "audio",
        KindConfiguration::Raster(_) => "raster",
        KindConfiguration::EncodedImage(_) => "encoded-image",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivid_protocol::cbor::Value;
    use vivid_protocol::identity::PresenterInstanceId;
    use vivid_protocol::surface::{CoordinateModel, SurfaceDescriptor, SurfaceRole};

    fn session(presenter: u8, id: u64) -> SessionIdentity {
        SessionIdentity::new(PresenterInstanceId([presenter; 16]), id).unwrap()
    }

    fn definition(context_id: u64, surface_id: u64) -> SurfaceDefinition {
        SurfaceDefinition {
            context_id,
            surface_id,
            semantic_profile: vivid_protocol::registry::TERMINAL_CONTENT.into(),
            coordinate_model: CoordinateModel::TerminalContentCells,
            logical_width: 10,
            logical_height: 10,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
            descriptor: SurfaceDescriptor {
                role: SurfaceRole::Figure,
                title: String::new(),
                semantic_content_revision: 0,
                semantic_availability: 0,
                locator_hint: String::new(),
            },
            policy: 0,
            profile_parameters: vec![],
        }
    }

    #[test]
    fn reused_local_ids_are_isolated_by_complete_owner() {
        let scene = SharedScene::default();
        let first = session(1, 1);
        let second = session(1, 2);
        scene.register_session(first, TargetGeneration::ONE).unwrap();
        scene.register_session(second, TargetGeneration::ONE).unwrap();
        let first_surface = first.context(1).unwrap().surface(1).unwrap();
        let second_surface = second.context(1).unwrap().surface(1).unwrap();
        scene.create_surface(first_surface, definition(1, 1)).unwrap();
        scene.create_surface(second_surface, definition(1, 1)).unwrap();
        scene.destroy_surface(first_surface).unwrap();
        assert!(scene.surface_status(first_surface).is_none());
        assert!(scene.surface_status(second_surface).is_some());
    }

    #[test]
    fn scene_commit_is_atomic_on_a_missing_surface() {
        let scene = SharedScene::default();
        let session = session(1, 1);
        let context = session.context(1).unwrap();
        scene.register_session(session, TargetGeneration::ONE).unwrap();
        scene.begin_transaction(context, 1).unwrap();
        scene
            .queue_node_create(
                context,
                1,
                SceneNode {
                    owning_context_id: 1,
                    node_id: 1,
                    surface_context_id: 1,
                    surface_id: 9,
                    geometry: vec![
                        (0, Value::Unsigned(1)),
                        (1, Value::Unsigned(0)),
                        (2, Value::Unsigned(0)),
                        (3, Value::Unsigned(1_u64 << 32)),
                        (4, Value::Unsigned(1_u64 << 32)),
                        (5, Value::Unsigned(1)),
                    ],
                    fit: vivid_protocol::scene::Fit::Contain,
                    linear_sampling: true,
                    z_index: 0,
                    visible: true,
                    opacity: u16::MAX,
                    clip: None,
                },
            )
            .unwrap();
        assert!(
            scene
                .commit_transaction(context, 1, TargetGeneration::ONE, Some(SceneRevision::ZERO))
                .is_err()
        );
        assert_eq!(scene.scene_status(session, 10).revision, SceneRevision::ZERO);
    }
}
