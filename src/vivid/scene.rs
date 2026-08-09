//! Owner-scoped Vivid 1.5 surface, track, and retained-scene state.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use vivid_protocol::cbor::Value;
use vivid_protocol::identity::{
    AnchorIdentity, ContextIdentity, NodeIdentity, SessionIdentity, SurfaceIdentity, TrackIdentity,
};
use vivid_protocol::revision::{
    ChannelGeneration, SceneRevision, SurfaceGeneration, SurfaceRevision, TargetGeneration,
    TrackRevision,
};
use vivid_protocol::scene::SceneNode;

use crate::vivid::target::{ClipRect, NodePlacement, PresentationTarget};
use vivid_protocol::surface::SurfaceDefinition;
use vivid_protocol::track::{
    KindConfiguration, MILESTONE_BUFFERED_ENDED, MILESTONE_CHANNEL_ACCEPTED,
    MILESTONE_CHANNEL_DETACHED, MILESTONE_CLOCK_STARTED, MILESTONE_EOS_ACCEPTED,
    MILESTONE_OUTPUT_READY, MILESTONE_PRESENTED, MILESTONE_TRACK_LOST, TrackConfiguration,
    TrackMode, TrackState,
};

pub type SurfaceKey = SurfaceIdentity;
pub type TrackKey = TrackIdentity;

pub const SLOT_PRIMARY_VIDEO: u64 = 1;
pub const SLOT_AUDIO: u64 = 2;
pub const SLOT_RASTER: u64 = 3;
pub const SLOT_POSTER: u64 = 4;
pub const ALPHA_STRAIGHT: u64 = 1;
const MAX_RETAINED_POSTER_PIXELS: u64 = 64 * 1024 * 1024;
/// Retained posters are also bounded by count, not only by pixels: a detached session holding many
/// tiny frames would otherwise stay within the pixel budget while growing the render list without
/// limit.
const MAX_RETAINED_POSTERS: usize = 256;

/// The most render items one [`SharedScene::snapshot`] can produce.
///
/// Every live session's visible nodes plus the retained posters of detached ones. The compositor
/// sizes its geometry against this, so it has to be the true ceiling rather than a guess.
pub const MAX_RENDER_ITEMS: usize =
    crate::vivid::MAX_SESSIONS * crate::vivid::MAX_SCENE_NODES + MAX_RETAINED_POSTERS;

// Every session may fill its node contract at once, and every retained poster is another item.
// Raising a contract without raising this would let the compositor be handed more geometry than it
// sized itself for, which is the overflow this constant exists to prevent.
const _: () = assert!(
    MAX_RENDER_ITEMS
        >= crate::vivid::MAX_SESSIONS * crate::vivid::MAX_SCENE_NODES + MAX_RETAINED_POSTERS
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RasterDamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Retained RGBA pixels with optional bounded return to their decoder's free-buffer pool.
#[derive(Debug)]
pub struct RgbaBuffer {
    bytes: Option<Vec<u8>>,
    decoder_pool: Option<Weak<Mutex<Vec<Vec<u8>>>>>,
}

impl RgbaBuffer {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes: Some(bytes), decoder_pool: None }
    }

    pub(crate) fn pooled(bytes: Vec<u8>, decoder_pool: Weak<Mutex<Vec<Vec<u8>>>>) -> Self {
        Self { bytes: Some(bytes), decoder_pool: Some(decoder_pool) }
    }
}

impl Deref for RgbaBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.bytes.as_deref().unwrap_or_default()
    }
}

impl AsRef<[u8]> for RgbaBuffer {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl Drop for RgbaBuffer {
    fn drop(&mut self) {
        let Some(pool) = self.decoder_pool.as_ref().and_then(Weak::upgrade) else {
            return;
        };
        let Some(bytes) = self.bytes.take() else {
            return;
        };
        let mut free = pool.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if free.len() < 2 {
            free.push(bytes);
        }
    }
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
    pub rgba: Arc<RgbaBuffer>,
    pub damage: Option<Arc<[RasterDamageRect]>>,
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
pub struct RenderSnapshot {
    pub topology_revision: u64,
    pub content_revision: u64,
    pub capture_revision: u64,
    pub items: Arc<Vec<RenderItem>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SceneOptimizationMetrics {
    pub snapshot_rebuilds: u64,
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
    pub last_media_record_sequence: u64,
    pub maximum_channel_bytes: u64,
    pub maximum_channel_records: u64,
    pub metrics: TrackMetrics,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackMetrics {
    pub decoded_frames: u64,
    pub discarded_late_frames: u64,
    pub latency_keyframe_requests: u64,
    pub audio_rebases: u64,
}

#[derive(Debug, Clone)]
pub struct SceneNodeStatus {
    pub identity: NodeIdentity,
    pub node: SceneNode,
}

#[derive(Debug, Clone)]
/// Bounded revision summaries plus their aggregate, for `SESSION_STATUS`.
pub struct SessionRevisionSummary {
    pub session_revision: u64,
    pub entries: Vec<Value>,
}

pub struct SceneStatus {
    pub session: SessionIdentity,
    pub revision: SceneRevision,
    pub target_generation: TargetGeneration,
    pub nodes: Vec<SceneNodeStatus>,
}

/// Why a composited frame did not advance its track's presentation record.
///
/// The two cases are not equally interesting. A resize replaces the surface mapping and the track
/// under a frame that is already in flight, so a superseded record is the expected outcome of
/// ordinary window movement: the frame really was composited, and the record it would have advanced
/// no longer describes what is on screen. A failure is a presenter anomaly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationRejection {
    Superseded(&'static str),
    Failed(&'static str),
}

impl std::fmt::Display for PresentationRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::Superseded(reason) | Self::Failed(reason)) = self;
        formatter.write_str(reason)
    }
}

/// Why a scene commit did not apply.
///
/// The three cases carry different registered error codes and different producer recoveries, so
/// they stay distinguishable instead of collapsing into one opaque state failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitRejection {
    /// The commit was planned against a target the presentation has already moved past.
    StaleTarget,
    /// The scene-revision precondition no longer holds.
    StaleRevision,
    /// The transaction, or one of the mutations in it, is not applicable.
    Failed(&'static str),
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
    Lost,
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
    last_media_record_sequence: u64,
    maximum_channel_bytes: u64,
    maximum_channel_records: u64,
    playback: Option<PlaybackClock>,
    metrics: TrackMetrics,
}

#[derive(Debug, Clone, Copy)]
struct PlaybackClock {
    start_pts_us: i64,
    started_at: Option<Instant>,
    played_before_pause: Duration,
    eos: bool,
}

impl PlaybackClock {
    fn started(start_pts_us: i64) -> Self {
        Self {
            start_pts_us,
            started_at: Some(Instant::now()),
            played_before_pause: Duration::ZERO,
            eos: false,
        }
    }

    fn current_pts_us(self) -> i64 {
        let elapsed = self
            .started_at
            .map(|started| started.elapsed())
            .unwrap_or_default()
            .saturating_add(self.played_before_pause);
        self.start_pts_us.saturating_add(i64::try_from(elapsed.as_micros()).unwrap_or(i64::MAX))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextAnchor {
    column: usize,
    line: i32,
    alternate: bool,
}

#[derive(Debug, Clone)]
struct RetainedPoster {
    anchor: AnchorIdentity,
    track_key: TrackIdentity,
    surface_generation: SurfaceGeneration,
    channel_generation: ChannelGeneration,
    x: i64,
    y: i64,
    width: i64,
    height: i64,
    text_layer: u64,
    z_index: i64,
    clip: Option<ClipRect>,
    frame: Arc<Frame>,
    capture_policy: u64,
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
    target_generation: TargetGeneration,
    surfaces: HashMap<SurfaceIdentity, Surface>,
    tracks: HashMap<TrackIdentity, Track>,
    scenes: HashMap<SessionIdentity, SessionScene>,
    anchors: HashMap<AnchorIdentity, TextAnchor>,
    gone_anchors: HashSet<AnchorIdentity>,
    detached_sessions: HashSet<SessionIdentity>,
    retained_posters: Vec<RetainedPoster>,
    alternate_screen: bool,
    topology_revision: u64,
    content_revision: u64,
    capture_revision: u64,
    render_cache: Option<Arc<Vec<RenderItem>>>,
}

impl State {
    fn invalidate_topology(&mut self) {
        self.topology_revision = self.topology_revision.wrapping_add(1);
        self.render_cache = None;
    }

    fn invalidate_capture(&mut self) {
        self.capture_revision = self.capture_revision.wrapping_add(1);
        self.invalidate_topology();
    }

    fn replace_cached_frame(&mut self, identity: TrackIdentity, frame: Arc<Frame>) {
        self.content_revision = self.content_revision.wrapping_add(1);
        let Some(items) = self.render_cache.as_mut() else {
            return;
        };
        for item in Arc::make_mut(items).iter_mut().filter(|item| item.track_key == identity) {
            item.frame = frame.clone();
        }
    }
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    target: Arc<dyn PresentationTarget>,
    snapshot_rebuilds: AtomicU64,
}

#[derive(Clone)]
pub struct SharedScene(Arc<Inner>);

impl SharedScene {
    /// Build a scene for one presentation target. The target decides what node geometry means,
    /// so it is fixed for the scene's lifetime exactly as it is for a session (core §1).
    pub fn new(target: Arc<dyn PresentationTarget>) -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            target,
            snapshot_rebuilds: AtomicU64::new(0),
        }))
    }

    pub fn target(&self) -> &Arc<dyn PresentationTarget> {
        &self.0.target
    }

    pub fn optimization_metrics(&self) -> SceneOptimizationMetrics {
        SceneOptimizationMetrics {
            snapshot_rebuilds: self.0.snapshot_rebuilds.load(Ordering::Relaxed),
        }
    }

    /// A scene on a default terminal target, for tests that do not care about placement.
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self::new(Arc::new(
            crate::vivid::target::TerminalTarget::new(crate::vivid::target::DisplayGeometry {
                viewport_width: 800,
                viewport_height: 600,
                columns: 80,
                rows: 24,
                cell_width: 10,
                cell_height: 25,
            })
            .expect("test geometry"),
        ))
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
        state.invalidate_topology();
        if state.scenes.contains_key(&session) {
            return Err("session already exists");
        }
        if target_generation > state.target_generation {
            state.target_generation = target_generation;
            for scene in state.scenes.values_mut() {
                scene.target_generation = target_generation;
            }
        }
        let target_generation = state.target_generation;
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

    /// Move every live session's scene onto a newly announced target generation.
    ///
    /// A scene commit names the target generation it was planned against, so the generation the
    /// scene validates commits against has to follow the presentation target. Leaving it behind
    /// rejects every commit that correctly names the new target as stale. The move is monotonic:
    /// an announcement can only carry the scene forward.
    pub fn advance_target_generation(&self, target_generation: TargetGeneration) {
        let mut state = self.lock();
        state.invalidate_topology();
        if state.target_generation < target_generation {
            state.target_generation = target_generation;
        }
        for scene in state.scenes.values_mut() {
            if scene.target_generation < target_generation {
                scene.target_generation = target_generation;
            }
        }
        self.0.changed.notify_all();
    }

    pub fn remove_session(&self, session: SessionIdentity) {
        let mut state = self.lock();
        state.invalidate_topology();
        state.surfaces.retain(|identity, _| identity.context.session != session);
        state.tracks.retain(|identity, _| identity.surface.context.session != session);
        state.scenes.remove(&session);
        state.anchors.retain(|identity, _| identity.context.session != session);
        state.gone_anchors.retain(|identity| identity.context.session != session);
        state.detached_sessions.remove(&session);
        state.retained_posters.retain(|poster| poster.anchor.context.session != session);
        self.0.changed.notify_all();
    }

    /// End protocol authority while retaining only policy-permitted anchored visual posters.
    ///
    /// The retained objects are no longer addressable protocol state. They exist solely as
    /// terminal presentation snapshots and are reclaimed with their authenticated anchors.
    /// Suspend a session's state instead of destroying it.
    ///
    /// Security §7.1: unclean loss under cleanup policy one retains surface and track metadata,
    /// scene nodes, policies, descriptors, and the active-slot mapping, while marking every track
    /// channel detached and discarding decoded media. The retained state stays charged to the
    /// lease's reservation until resume or grace expiry, which is why nothing is released here.
    pub fn suspend_session(&self, session: SessionIdentity) {
        let mut state = self.lock();
        state.invalidate_topology();
        if !state.scenes.contains_key(&session) {
            return;
        }
        let owned = state
            .tracks
            .keys()
            .filter(|identity| identity.surface.context.session == session)
            .copied()
            .collect::<Vec<_>>();
        for identity in owned {
            let Some(track) = state.tracks.get_mut(&identity) else {
                continue;
            };
            // Every channel needs a new generation and a fresh key or full unit after resume.
            let _ = track.state.detach();
            // Decoded output and ingress do not survive; a policy-permitted poster may, and it
            // is the surface's retained frame rather than a live decoder.
            track.playback = None;
            track.last_decoded_pts_us = None;
        }
        self.0.changed.notify_all();
    }

    /// Has this surface's active primary-video track presented for the current surface generation?
    ///
    /// Desktop §5.1 makes milestone bit 5 on the track's *current* channel generation a
    /// precondition of enabling input, and media §7 resets those bits on every channel advance, so
    /// a sticky bit from an older generation must not answer this.
    pub fn surface_has_presented(&self, identity: SurfaceIdentity) -> bool {
        let state = self.lock();
        let Some(surface) = state.surfaces.get(&identity) else {
            return false;
        };
        let Some(track_id) = surface.active_slots.get(&SLOT_PRIMARY_VIDEO) else {
            return false;
        };
        let key = TrackIdentity { surface: identity, track_id: *track_id };
        state
            .tracks
            .get(&key)
            .is_some_and(|track| track.state.milestones & MILESTONE_PRESENTED != 0)
    }

    /// Whether a suspended session's retained state is still present.
    pub fn is_registered(&self, session: SessionIdentity) -> bool {
        self.lock().scenes.contains_key(&session)
    }

    pub fn detach_session(&self, session: SessionIdentity) {
        let mut state = self.lock();
        state.invalidate_topology();
        let Some(scene) = state.scenes.get(&session) else {
            return;
        };
        let mut posters = Vec::new();
        let mut retained_pixels = 0_u64;
        // This session's own earlier posters are replaced below, so only other sessions' posters
        // count against the ceiling.
        let posters_kept = state
            .retained_posters
            .iter()
            .filter(|poster| poster.anchor.context.session != session)
            .count();
        for node in scene.nodes.values() {
            if let Some((poster, pixels)) = retained_poster(&*self.0.target, &state, session, node)
                && retained_pixels.saturating_add(pixels) <= MAX_RETAINED_POSTER_PIXELS
                && posters_kept + posters.len() < MAX_RETAINED_POSTERS
            {
                retained_pixels = retained_pixels.saturating_add(pixels);
                posters.push(poster);
            }
        }

        let retained_anchors = posters.iter().map(|poster| poster.anchor).collect::<HashSet<_>>();
        state.surfaces.retain(|identity, _| identity.context.session != session);
        state.tracks.retain(|identity, _| identity.surface.context.session != session);
        state.scenes.remove(&session);
        state.anchors.retain(|identity, _| {
            identity.context.session != session || retained_anchors.contains(identity)
        });
        state.gone_anchors.retain(|identity| identity.context.session != session);
        state.retained_posters.retain(|poster| poster.anchor.context.session != session);
        state.retained_posters.extend(posters);
        if retained_anchors.is_empty() {
            state.detached_sessions.remove(&session);
        } else {
            state.detached_sessions.insert(session);
        }
        gc_detached_sessions(&mut state);
        self.0.changed.notify_all();
    }

    pub fn remove_contexts(&self, session: SessionIdentity, contexts: &HashSet<u64>) {
        let mut state = self.lock();
        state.invalidate_topology();
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
        state.anchors.retain(|identity, _| {
            identity.context.session != session || !contexts.contains(&identity.context.context_id)
        });
        state.gone_anchors.retain(|identity| {
            identity.context.session != session || !contexts.contains(&identity.context.context_id)
        });
        state.retained_posters.retain(|poster| {
            poster.anchor.context.session != session
                || !contexts.contains(&poster.anchor.context.context_id)
        });
        gc_detached_sessions(&mut state);
        self.0.changed.notify_all();
    }

    pub fn anchor_positions(&self) -> Vec<(AnchorIdentity, usize, i32, bool)> {
        let state = self.lock();
        state
            .anchors
            .iter()
            .map(|(&identity, anchor)| (identity, anchor.column, anchor.line, anchor.alternate))
            .collect()
    }

    pub fn add_anchor(
        &self,
        identity: AnchorIdentity,
        column: usize,
        line: i32,
        alternate: bool,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        state.invalidate_topology();
        if state.anchors.contains_key(&identity) || state.gone_anchors.contains(&identity) {
            return Err("anchor identity was already used");
        }
        state.anchors.insert(identity, TextAnchor { column, line, alternate });
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn anchor_status(&self, identity: AnchorIdentity) -> (u64, Option<(usize, i32, bool)>) {
        let state = self.lock();
        if let Some(anchor) = state.anchors.get(&identity) {
            (1, Some((anchor.column, anchor.line, anchor.alternate)))
        } else if state.gone_anchors.contains(&identity) {
            (2, None)
        } else {
            (0, None)
        }
    }

    pub fn apply_anchor_resize(
        &self,
        positions: impl IntoIterator<Item = (AnchorIdentity, Option<(usize, i32, bool)>)>,
    ) -> Vec<AnchorIdentity> {
        let mut state = self.lock();
        state.invalidate_topology();
        let mut removed = Vec::new();
        for (identity, position) in positions {
            match (state.anchors.get_mut(&identity), position) {
                (Some(anchor), Some((column, line, alternate))) => {
                    *anchor = TextAnchor { column, line, alternate };
                },
                (Some(_), None) => removed.push(identity),
                _ => {},
            }
        }
        remove_anchors(&mut state, &removed);
        self.0.changed.notify_all();
        removed
    }

    /// Move terminal-semantic anchors with a grid scroll. Positive `lines` moves content up.
    pub fn scroll_anchors(
        &self,
        origin: i32,
        end: i32,
        lines: i32,
        history_size: usize,
    ) -> Vec<AnchorIdentity> {
        if lines == 0 || origin >= end {
            return Vec::new();
        }
        let minimum_line = -(history_size.min(i32::MAX as usize) as i32);
        let mut state = self.lock();
        state.invalidate_topology();
        let mut removed = Vec::new();
        for (&identity, anchor) in &mut state.anchors {
            let old = anchor.line;
            let next = if lines > 0 {
                if origin == 0 && old < end {
                    Some(old.saturating_sub(lines))
                } else if (origin..end).contains(&old) {
                    (old >= origin.saturating_add(lines)).then_some(old.saturating_sub(lines))
                } else {
                    Some(old)
                }
            } else if (origin..end).contains(&old) {
                let amount = lines.saturating_abs();
                (old < end.saturating_sub(amount)).then_some(old.saturating_add(amount))
            } else {
                Some(old)
            };
            match next {
                Some(line) if line >= minimum_line => anchor.line = line,
                _ => removed.push(identity),
            }
        }
        remove_anchors(&mut state, &removed);
        self.0.changed.notify_all();
        removed
    }

    pub fn clear_terminal(&self) -> Vec<AnchorIdentity> {
        let mut state = self.lock();
        state.invalidate_topology();
        let removed = state.anchors.keys().copied().collect::<Vec<_>>();
        remove_anchors(&mut state, &removed);
        self.0.changed.notify_all();
        removed
    }

    pub fn set_alternate_screen(&self, alternate: bool) -> Vec<AnchorIdentity> {
        let mut state = self.lock();
        if state.alternate_screen == alternate {
            return Vec::new();
        }
        state.invalidate_topology();
        state.alternate_screen = alternate;
        let removed = if alternate {
            Vec::new()
        } else {
            state
                .anchors
                .iter()
                .filter_map(|(&identity, anchor)| anchor.alternate.then_some(identity))
                .collect::<Vec<_>>()
        };
        remove_anchors(&mut state, &removed);
        self.0.changed.notify_all();
        removed
    }

    pub fn create_surface(
        &self,
        identity: SurfaceIdentity,
        definition: SurfaceDefinition,
    ) -> Result<SurfaceStatus, &'static str> {
        definition.validate().map_err(|_| "invalid surface definition")?;
        self.0.target.validate_surface(&definition)?;
        if definition.context_id != identity.context.context_id
            || definition.surface_id != identity.surface_id
        {
            return Err("surface owner does not match complete identity");
        }
        let mut state = self.lock();
        state.invalidate_topology();
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
        self.0.target.validate_surface(&replacement)?;
        let mut state = self.lock();
        state.invalidate_capture();
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
        state.invalidate_topology();
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
        let state = self.lock();
        let mut keys = state
            .surfaces
            .keys()
            .filter(|identity| !state.detached_sessions.contains(&identity.context.session))
            .copied()
            .collect::<Vec<_>>();
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
        state.invalidate_topology();
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
            last_media_record_sequence: 0,
            maximum_channel_bytes: 0,
            maximum_channel_records: 0,
            playback: None,
            metrics: TrackMetrics::default(),
        };
        let status = track_status(identity, &track);
        state.tracks.insert(identity, track);
        self.0.changed.notify_all();
        Ok(status)
    }

    pub fn destroy_track(&self, identity: TrackIdentity) -> Result<(), &'static str> {
        let mut state = self.lock();
        state.invalidate_topology();
        if state.tracks.remove(&identity).is_none() {
            return Err("track does not exist");
        }
        if let Some(surface) = state.surfaces.get_mut(&identity.surface) {
            let slots = surface.active_slots.len();
            surface.active_slots.retain(|_, track_id| *track_id != identity.track_id);
            // Only a vacated slot mutates the surface. Advancing the revision for a track that
            // held no slot moves presenter truth away from the revision the producer holds, with
            // nothing said about it, and the producer's next surface update is rejected as stale.
            // Retiring a replaced track is exactly that case.
            if surface.active_slots.len() != slots {
                surface.revision =
                    surface.revision.advance().map_err(|_| "surface revision exhausted")?;
            }
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
        state.invalidate_topology();
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
            if track.lifecycle == 6 {
                return Err("track was lost before slot activation");
            }
            if track.configuration.slot != slot {
                return Err("track configuration does not permit the requested slot");
            }
            if track.state.channel_generation != expected_generation {
                return Err("slot activation names a stale channel generation");
            }
            if required_milestone.count_ones() != 1
                || track.state.milestones & required_milestone == 0
            {
                return Err("track has not reached the required activation milestone");
            }
            if track.lifecycle != 1 {
                return Err("track lifecycle is not eligible for slot activation");
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

    pub fn active_track(
        &self,
        surface_identity: SurfaceIdentity,
        slot: u64,
    ) -> Option<TrackIdentity> {
        let state = self.lock();
        let track_id = *state.surfaces.get(&surface_identity)?.active_slots.get(&slot)?;
        let identity = TrackIdentity { surface: surface_identity, track_id };
        state.tracks.get(&identity).is_some_and(|track| track.lifecycle == 1).then_some(identity)
    }

    pub fn track_keys(&self) -> Vec<TrackIdentity> {
        let state = self.lock();
        let mut keys = state
            .tracks
            .keys()
            .filter(|identity| !state.detached_sessions.contains(&identity.surface.context.session))
            .copied()
            .collect::<Vec<_>>();
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
        state.invalidate_topology();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        let current = track.state.channel_generation;
        let next = current.advance().map_err(|_| "channel generation exhausted")?;
        track
            .state
            .advance_channel(current, next)
            .map_err(|_| "channel generation could not advance")?;
        track.maximum_channel_bytes = 0;
        track.maximum_channel_records = 0;
        track.last_media_record_sequence = 0;
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
        record_sequence: u64,
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
        self.admit_media(
            identity,
            generation,
            body_length,
            media_epoch,
            media_id,
            random_access,
            record_sequence,
        )?;
        self.publish_decoded_frame(identity, generation, frame)
    }

    pub fn detach_channel(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        state.invalidate_topology();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        if track.state.channel_generation != generation || track.lifecycle != 1 {
            return Err("stale channel generation");
        }
        track.state.detach().map_err(|_| "track revision exhausted")?;
        self.0.changed.notify_all();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn admit_media(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
        body_length: u32,
        media_epoch: u32,
        media_id: u64,
        random_access: bool,
        record_sequence: u64,
    ) -> Result<(), &'static str> {
        if record_sequence <= 1 {
            return Err("media record sequence does not follow CHANNEL_OPEN");
        }
        let mut state = self.lock();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        if track.state.channel_generation != generation || track.lifecycle != 1 {
            return Err("stale channel generation");
        }
        track
            .state
            .admit_media(generation, body_length, media_epoch, media_id, random_access)
            .map_err(|_| "stale media epoch, media ID, or flow allowance")?;
        track.last_media_record_sequence = record_sequence;
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
        let frame = Arc::new(frame);
        let replacing = {
            let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
            if track.state.channel_generation != generation || track.lifecycle != 1 {
                return Err("stale channel generation");
            }
            track.last_decoded_pts_us = Some(frame.pts_us);
            track.metrics.decoded_frames = track.metrics.decoded_frames.saturating_add(1);
            let replacing = track.frame.is_some();
            track.frame = Some(frame.clone());
            track.state.milestones |= MILESTONE_OUTPUT_READY;
            track.state.revision =
                track.state.revision.advance().map_err(|_| "track revision exhausted")?;
            replacing
        };
        if replacing {
            state.replace_cached_frame(identity, frame);
        } else {
            state.content_revision = state.content_revision.wrapping_add(1);
            state.invalidate_topology();
        }
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn record_late_video_discard(
        &self,
        identity: TrackIdentity,
        frames: u64,
        requested_keyframe: bool,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        track.metrics.discarded_late_frames =
            track.metrics.discarded_late_frames.saturating_add(frames);
        if requested_keyframe {
            track.metrics.latency_keyframe_requests =
                track.metrics.latency_keyframe_requests.saturating_add(1);
        }
        Ok(())
    }

    pub fn record_audio_rebase(&self, identity: TrackIdentity) -> Result<(), &'static str> {
        let mut state = self.lock();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        track.metrics.audio_rebases = track.metrics.audio_rebases.saturating_add(1);
        Ok(())
    }

    pub fn lose_track(&self, identity: TrackIdentity) -> Result<(), &'static str> {
        let mut state = self.lock();
        state.invalidate_topology();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        track.lifecycle = 6;
        track.state.milestones = 0;
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

    pub fn start_playback(
        &self,
        identity: TrackIdentity,
        start_pts_us: i64,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let master = state.tracks.get(&identity).ok_or("track does not exist")?;
        if master.configuration.mode != TrackMode::Timed || master.lifecycle != 1 {
            return Err("PLAY requires a live timed track");
        }
        let active = state
            .surfaces
            .get(&identity.surface)
            .ok_or("surface does not exist")?
            .active_slots
            .values()
            .copied()
            .map(|track_id| TrackIdentity { surface: identity.surface, track_id })
            .collect::<Vec<_>>();
        let mut started = false;
        for candidate in active {
            let Some(track) = state.tracks.get_mut(&candidate) else {
                continue;
            };
            if track.configuration.mode != TrackMode::Timed || track.lifecycle != 1 {
                continue;
            }
            track.playback = Some(PlaybackClock::started(start_pts_us));
            track.state.milestones |= MILESTONE_CLOCK_STARTED;
            track.state.revision =
                track.state.revision.advance().map_err(|_| "track revision exhausted")?;
            started = true;
        }
        if !started {
            return Err("PLAY surface has no active timed tracks");
        }
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn pause_playback(&self, identity: TrackIdentity) -> Result<(), &'static str> {
        let mut state = self.lock();
        let surface = identity.surface;
        if !state.tracks.contains_key(&identity) {
            return Err("track does not exist");
        }
        for track in state
            .tracks
            .iter_mut()
            .filter_map(|(candidate, track)| (candidate.surface == surface).then_some(track))
        {
            if let Some(playback) = track.playback.as_mut()
                && let Some(started) = playback.started_at.take()
            {
                playback.played_before_pause =
                    playback.played_before_pause.saturating_add(started.elapsed());
                track.state.revision =
                    track.state.revision.advance().map_err(|_| "track revision exhausted")?;
            }
        }
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn flush_playback(
        &self,
        identity: TrackIdentity,
        new_epoch: u32,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        state.invalidate_topology();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        if track.configuration.mode != TrackMode::Timed || new_epoch <= track.state.media_epoch {
            return Err("FLUSH requires a greater epoch on a timed track");
        }
        track.playback = None;
        track.frame = None;
        track.state.media_epoch = new_epoch;
        track.state.milestones &= MILESTONE_CHANNEL_ACCEPTED;
        track.state.revision =
            track.state.revision.advance().map_err(|_| "track revision exhausted")?;
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn mark_eos(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
        media_epoch: u32,
        last_media_record_sequence: u64,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        if track.state.channel_generation != generation
            || track.state.media_epoch != media_epoch
            || track.last_media_record_sequence != last_media_record_sequence
        {
            return Err("CHANNEL_EOS does not match accepted channel progress");
        }
        if let Some(playback) = track.playback.as_mut() {
            playback.eos = true;
        }
        track.state.milestones |= MILESTONE_EOS_ACCEPTED;
        track.state.revision =
            track.state.revision.advance().map_err(|_| "track revision exhausted")?;
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn mark_buffered_ended(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let track = state.tracks.get_mut(&identity).ok_or("track does not exist")?;
        if track.state.channel_generation != generation {
            return Err("stale channel generation");
        }
        track.state.milestones |= MILESTONE_EOS_ACCEPTED | MILESTONE_BUFFERED_ENDED;
        track.state.revision =
            track.state.revision.advance().map_err(|_| "track revision exhausted")?;
        self.0.changed.notify_all();
        Ok(())
    }

    /// Pace decoded timed output against the surface-group PLAY clock.
    ///
    /// Every output released by the first output-bearing record is admitted immediately so the
    /// producer can finish that record, observe `MILESTONE_OUTPUT_READY`, and issue PLAY without
    /// a startup deadlock. Output from subsequent records is paced against the playback clock.
    pub fn wait_until_due(
        &self,
        identity: TrackIdentity,
        pts_us: i64,
        priming_record: bool,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        loop {
            let track = state.tracks.get(&identity).ok_or("track does not exist")?;
            if track.configuration.mode != TrackMode::Timed
                || track.frame.is_none()
                || pts_us == i64::MIN
                || priming_record
            {
                return Ok(());
            }
            let Some(playback) = track.playback else {
                // Once one decoded output has primed the track, hold subsequent output until PLAY.
                // Otherwise a fast channel can decode an entire file before the producer observes
                // OUTPUT_READY and atomically activates the timed slots.
                state = self.0.changed.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
                continue;
            };
            let remaining = pts_us.saturating_sub(playback.current_pts_us());
            if remaining <= 0 {
                return Ok(());
            }
            let wait = Duration::from_micros(u64::try_from(remaining).unwrap_or(u64::MAX))
                .min(Duration::from_millis(20));
            let result = self
                .0
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = result.0;
        }
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

    pub fn transaction_context(
        &self,
        session: SessionIdentity,
        transaction_id: u64,
    ) -> Result<ContextIdentity, &'static str> {
        let state = self.lock();
        let scene = state.scenes.get(&session).ok_or("session does not exist")?;
        let mut matches = scene
            .pending
            .keys()
            .filter_map(|(context, candidate)| (*candidate == transaction_id).then_some(*context));
        let context = matches.next().ok_or("transaction does not exist")?;
        if matches.next().is_some() {
            return Err("transaction ID is ambiguous within the session");
        }
        Ok(context)
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
    ) -> Result<SceneRevision, CommitRejection> {
        let mut state = self.lock();
        state.invalidate_topology();
        let session = context.session;
        let mutations = state
            .scenes
            .get(&session)
            .and_then(|scene| scene.pending.get(&(context, transaction_id)))
            .cloned()
            .ok_or(CommitRejection::Failed("transaction does not exist"))?;
        {
            let scene = state.scenes.get(&session).unwrap();
            if scene.target_generation != expected_target {
                return Err(CommitRejection::StaleTarget);
            }
            if expected_revision.is_some_and(|revision| revision != scene.revision) {
                return Err(CommitRejection::StaleRevision);
            }
        }
        let mut candidate = state.scenes.get(&session).unwrap().nodes.clone();
        for mutation in mutations {
            match mutation {
                NodeMutation::Create(node) => {
                    self.0.target.validate_node(&node).map_err(CommitRejection::Failed)?;
                    let identity = context
                        .node(node.node_id)
                        .map_err(|_| CommitRejection::Failed("invalid node ID"))?;
                    if node.owning_context_id != context.context_id
                        || candidate.contains_key(&identity)
                    {
                        return Err(CommitRejection::Failed("duplicate or misowned node"));
                    }
                    let surface_identity = context
                        .session
                        .context(node.surface_context_id)
                        .map_err(|_| CommitRejection::Failed("invalid surface context"))?
                        .surface(node.surface_id)
                        .map_err(|_| CommitRejection::Failed("invalid surface ID"))?;
                    if !state.surfaces.contains_key(&surface_identity) {
                        return Err(CommitRejection::Failed("node references a missing surface"));
                    }
                    candidate.insert(identity, node);
                },
                NodeMutation::Update(node) => {
                    self.0.target.validate_node(&node).map_err(CommitRejection::Failed)?;
                    let identity = context
                        .node(node.node_id)
                        .map_err(|_| CommitRejection::Failed("invalid node ID"))?;
                    if node.owning_context_id != context.context_id
                        || !candidate.contains_key(&identity)
                    {
                        return Err(CommitRejection::Failed("missing or misowned node"));
                    }
                    candidate.insert(identity, node);
                },
                NodeMutation::Delete(identity) => {
                    if identity.context != context || candidate.remove(&identity).is_none() {
                        return Err(CommitRejection::Failed("missing or misowned node"));
                    }
                },
            }
        }
        let scene = state.scenes.get_mut(&session).unwrap();
        scene.revision = scene
            .revision
            .advance()
            .map_err(|_| CommitRejection::Failed("scene revision exhausted"))?;
        scene.nodes = candidate;
        scene.pending.remove(&(context, transaction_id));
        let revision = scene.revision;
        self.0.changed.notify_all();
        Ok(revision)
    }

    /// Bounded per-object revision summaries for `SESSION_STATUS` key 6.
    ///
    /// Core §10 makes this a producer's reconciliation root after resume: it compares context,
    /// surface, track, and active-slot revisions against what it retained.
    pub fn session_revision_summary(
        &self,
        session: SessionIdentity,
        maximum: usize,
    ) -> SessionRevisionSummary {
        let state = self.lock();
        let mut entries = Vec::new();
        let mut session_revision = 0_u64;
        for (identity, surface) in &state.surfaces {
            if identity.context.session != session || entries.len() >= maximum {
                continue;
            }
            session_revision = session_revision.saturating_add(surface.revision.get());
            let tracks = state
                .tracks
                .iter()
                .filter(|(key, _)| key.surface == *identity)
                .map(|(key, track)| {
                    Value::Map(vec![
                        (0, Value::Unsigned(key.track_id)),
                        (1, Value::Unsigned(track.state.revision.get())),
                        (2, Value::Unsigned(track.state.channel_generation.get())),
                    ])
                })
                .collect::<Vec<_>>();
            entries.push(Value::Map(vec![
                (0, Value::Unsigned(identity.context.context_id)),
                (1, Value::Unsigned(identity.surface_id)),
                (2, Value::Unsigned(surface.revision.get())),
                (3, Value::Unsigned(surface.generation.get())),
                (4, Value::Unsigned(surface.lifecycle)),
                (5, Value::Array(tracks)),
            ]));
        }
        SessionRevisionSummary { session_revision, entries }
    }

    /// The session's current scene revision.
    pub fn scene_revision(&self, session: SessionIdentity) -> u64 {
        self.lock().scenes.get(&session).map(|scene| scene.revision.get()).unwrap_or(0)
    }

    /// Aggregate resource use for `SESSION_STATUS` key 8.
    pub fn resource_usage(&self, session: SessionIdentity) -> Value {
        let state = self.lock();
        let surfaces =
            state.surfaces.keys().filter(|key| key.context.session == session).count() as u64;
        let tracks =
            state.tracks.keys().filter(|key| key.surface.context.session == session).count() as u64;
        let nodes = state.scenes.get(&session).map(|scene| scene.nodes.len() as u64).unwrap_or(0);
        Value::Map(vec![
            (0, Value::Unsigned(surfaces)),
            (1, Value::Unsigned(tracks)),
            (2, Value::Unsigned(nodes)),
        ])
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

    pub fn snapshot(&self) -> RenderSnapshot {
        let mut state = self.lock();
        if let Some(items) = state.render_cache.clone() {
            return RenderSnapshot {
                topology_revision: state.topology_revision,
                content_revision: state.content_revision,
                capture_revision: state.capture_revision,
                items,
            };
        }
        self.0.snapshot_rebuilds.fetch_add(1, Ordering::Relaxed);
        let mut items = Vec::new();
        for (session_identity, scene) in &state.scenes {
            for node in scene.nodes.values().filter(|node| node.visible) {
                let Some(placement) = self.0.target.placement(node) else {
                    continue;
                };
                let NodePlacement { width, height, text_layer, text_anchored, .. } = placement;
                let (mut x, mut y) = (placement.x, placement.y);
                let mut clip = self.0.target.clip(node);
                if text_anchored {
                    let Some(anchor_identity) = node_anchor_identity(*session_identity, node)
                    else {
                        continue;
                    };
                    let Some(anchor) = state.anchors.get(&anchor_identity) else {
                        continue;
                    };
                    if anchor.alternate != state.alternate_screen {
                        continue;
                    }
                    let Some(anchor_x) = fixed_cells_i64(anchor.column) else {
                        continue;
                    };
                    let anchor_y = i64::from(anchor.line) << 32;
                    let Some(positioned_x) = x.checked_add(anchor_x) else {
                        continue;
                    };
                    let Some(positioned_y) = y.checked_add(anchor_y) else {
                        continue;
                    };
                    x = positioned_x;
                    y = positioned_y;
                    if let Some(value) = clip.as_mut() {
                        let Some(positioned_x) = value.x.checked_add(anchor_x) else {
                            continue;
                        };
                        let Some(positioned_y) = value.y.checked_add(anchor_y) else {
                            continue;
                        };
                        value.x = positioned_x;
                        value.y = positioned_y;
                    }
                }
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
                    clip,
                    frame: frame.clone(),
                    capture_policy: surface.definition.policy,
                });
            }
        }
        for poster in &state.retained_posters {
            let Some(anchor) = state.anchors.get(&poster.anchor) else {
                continue;
            };
            if anchor.alternate != state.alternate_screen {
                continue;
            }
            let Some(anchor_x) = fixed_cells_i64(anchor.column) else {
                continue;
            };
            let anchor_y = i64::from(anchor.line) << 32;
            let Some(x) = poster.x.checked_add(anchor_x) else {
                continue;
            };
            let Some(y) = poster.y.checked_add(anchor_y) else {
                continue;
            };
            let clip = match poster.clip {
                Some(mut clip) => {
                    let Some(x) = clip.x.checked_add(anchor_x) else {
                        continue;
                    };
                    let Some(y) = clip.y.checked_add(anchor_y) else {
                        continue;
                    };
                    clip.x = x;
                    clip.y = y;
                    Some(clip)
                },
                None => None,
            };
            items.push(RenderItem {
                track_key: poster.track_key,
                surface_key: poster.track_key.surface,
                surface_generation: poster.surface_generation,
                channel_generation: poster.channel_generation,
                x,
                y,
                width: poster.width,
                height: poster.height,
                text_anchored: true,
                text_layer: poster.text_layer,
                z_index: poster.z_index,
                clip,
                frame: poster.frame.clone(),
                capture_policy: poster.capture_policy,
            });
        }
        items.sort_by_key(|item| (item.text_layer, item.z_index));
        let items = Arc::new(items);
        state.render_cache = Some(items.clone());
        RenderSnapshot {
            topology_revision: state.topology_revision,
            content_revision: state.content_revision,
            capture_revision: state.capture_revision,
            items,
        }
    }

    pub fn mark_presented(
        &self,
        identity: TrackIdentity,
        channel_generation: ChannelGeneration,
        surface_generation: SurfaceGeneration,
        frame_id: u64,
        pts_us: i64,
    ) -> Result<(), PresentationRejection> {
        let mut state = self.lock();
        let Some(surface) = state.surfaces.get(&identity.surface) else {
            // Retained terminal-native posters have no live track milestone to acknowledge.
            return Ok(());
        };
        if surface.generation != surface_generation {
            return Err(PresentationRejection::Superseded("stale surface generation"));
        }
        let Some(track) = state.tracks.get_mut(&identity) else {
            return Ok(());
        };
        if track.state.channel_generation != channel_generation
            || track.frame.as_ref().is_none_or(|frame| frame.frame_id != frame_id)
        {
            return Err(PresentationRejection::Superseded("stale frame presentation"));
        }
        track.last_presented_pts_us = Some(pts_us);
        track.last_presentation_id = track
            .last_presentation_id
            .checked_add(1)
            .ok_or(PresentationRejection::Failed("presentation ID exhausted"))?;
        track.state.milestones |= MILESTONE_PRESENTED;
        track.state.revision = track
            .state
            .revision
            .advance()
            .map_err(|_| PresentationRejection::Failed("track revision exhausted"))?;
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
        if track.lifecycle == 6 && condition != 9 {
            return TrackWaitEvaluation::Lost;
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
        let state = self.lock();
        let mut sessions = state
            .scenes
            .keys()
            .filter(|session| !state.detached_sessions.contains(session))
            .copied()
            .collect::<Vec<_>>();
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

fn node_anchor_identity(session: SessionIdentity, node: &SceneNode) -> Option<AnchorIdentity> {
    (node.geometry.iter().find(|entry| entry.0 == 0)?.1.as_u64()? == 2).then_some(())?;
    let context_id = node.geometry.iter().find(|entry| entry.0 == 6)?.1.as_u64()?;
    let anchor_id = node.geometry.iter().find(|entry| entry.0 == 7)?.1.as_u64()?;
    session.context(context_id).ok()?.anchor(anchor_id).ok()
}

fn retained_poster(
    target: &dyn PresentationTarget,
    state: &State,
    session: SessionIdentity,
    node: &SceneNode,
) -> Option<(RetainedPoster, u64)> {
    if !node.visible {
        return None;
    }
    let placement = target.placement(node)?;
    let NodePlacement { x, y, width, height, text_layer, text_anchored } = placement;
    if !text_anchored {
        return None;
    }
    let anchor = node_anchor_identity(session, node)?;
    state.anchors.get(&anchor)?;
    let surface_key =
        session.context(node.surface_context_id).ok()?.surface(node.surface_id).ok()?;
    let surface = state.surfaces.get(&surface_key)?;
    if surface.definition.policy & vivid_protocol::surface::POLICY_DENY_POSTER_RETENTION != 0 {
        return None;
    }
    let (track_key, track, frame) =
        [SLOT_PRIMARY_VIDEO, SLOT_RASTER, SLOT_POSTER].into_iter().find_map(|slot| {
            let track_id = *surface.active_slots.get(&slot)?;
            let track_key = TrackIdentity { surface: surface_key, track_id };
            let track = state.tracks.get(&track_key)?;
            (track.lifecycle == 1)
                .then_some(track.frame.as_ref().map(|frame| (track_key, track, frame)))
                .flatten()
        })?;
    let pixels = u64::from(frame.width).checked_mul(u64::from(frame.height))?;
    Some((
        RetainedPoster {
            anchor,
            track_key,
            surface_generation: surface.generation,
            channel_generation: track.state.channel_generation,
            x,
            y,
            width,
            height,
            text_layer,
            z_index: node.z_index,
            clip: target.clip(node),
            frame: frame.clone(),
            capture_policy: surface.definition.policy,
        },
        pixels,
    ))
}

fn fixed_cells_i64(cells: usize) -> Option<i64> {
    i64::try_from(cells).ok()?.checked_shl(32)
}

fn remove_anchors(state: &mut State, removed: &[AnchorIdentity]) {
    if removed.is_empty() {
        return;
    }
    let removed = removed.iter().copied().collect::<HashSet<_>>();
    for identity in &removed {
        state.anchors.remove(identity);
        state.gone_anchors.insert(*identity);
    }
    for (session, scene) in &mut state.scenes {
        scene.nodes.retain(|_, node| {
            node_anchor_identity(*session, node).is_none_or(|anchor| !removed.contains(&anchor))
        });
    }
    state.retained_posters.retain(|poster| !removed.contains(&poster.anchor));
    gc_detached_sessions(state);
}

fn gc_detached_sessions(state: &mut State) {
    let detached = state.detached_sessions.iter().copied().collect::<Vec<_>>();
    for session in detached {
        let has_posters =
            state.retained_posters.iter().any(|poster| poster.anchor.context.session == session);
        if !has_posters {
            state.detached_sessions.remove(&session);
            state.anchors.retain(|identity, _| identity.context.session != session);
            state.gone_anchors.retain(|identity| identity.context.session != session);
        }
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
        last_media_record_sequence: track.last_media_record_sequence,
        maximum_channel_bytes: track.maximum_channel_bytes,
        maximum_channel_records: track.maximum_channel_records,
        metrics: track.metrics,
    }
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
    use vivid_protocol::messages::LaneClass;
    use vivid_protocol::surface::{CoordinateModel, SurfaceDescriptor, SurfaceRole};
    use vivid_protocol::track::{KindConfiguration, RasterConfiguration, VideoConfiguration};

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
    fn rgba_buffers_return_to_a_bounded_decoder_pool() {
        let pool = Arc::new(Mutex::new(Vec::new()));
        for value in 0..3 {
            drop(RgbaBuffer::pooled(vec![value; 16], Arc::downgrade(&pool)));
        }
        let free = pool.lock().unwrap();
        assert_eq!(free.len(), 2, "a decoder retains at most two free frames");
        assert!(free.iter().all(|buffer| buffer.len() == 16));
    }

    #[test]
    fn unchanged_snapshots_share_the_cached_render_list() {
        let scene = SharedScene::for_test();
        let first = scene.snapshot();
        let second = scene.snapshot();
        assert!(Arc::ptr_eq(&first.items, &second.items));
        assert_eq!(scene.optimization_metrics().snapshot_rebuilds, 1);

        scene.register_session(session(9, 1), TargetGeneration::ONE).unwrap();
        let changed = scene.snapshot();
        assert!(!Arc::ptr_eq(&second.items, &changed.items));
        assert!(changed.topology_revision > second.topology_revision);
        assert_eq!(scene.optimization_metrics().snapshot_rebuilds, 2);
    }

    #[test]
    fn reused_local_ids_are_isolated_by_complete_owner() {
        let scene = SharedScene::for_test();
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
    fn latency_recovery_metrics_are_isolated_for_reused_track_ids() {
        let scene = SharedScene::for_test();
        let first = session(2, 1);
        let second = session(2, 2);
        scene.register_session(first, TargetGeneration::ONE).unwrap();
        scene.register_session(second, TargetGeneration::ONE).unwrap();
        let first_surface = first.context(1).unwrap().surface(1).unwrap();
        let second_surface = second.context(1).unwrap().surface(1).unwrap();
        scene.create_surface(first_surface, definition(1, 1)).unwrap();
        scene.create_surface(second_surface, definition(1, 1)).unwrap();
        let configuration = |track_id| TrackConfiguration {
            context_id: 1,
            surface_id: 1,
            track_id,
            slot: SLOT_RASTER,
            mode: TrackMode::Live,
            lane: LaneClass::Bulk,
            maximum_record_body: 128,
            maximum_rate_millihertz: 1_000,
            maximum_encoded_bits_per_second: 8_192,
            maximum_records_per_second: 1,
            maximum_inflight_body_bytes: 1_024,
            kind: KindConfiguration::Raster(RasterConfiguration {
                width: 1,
                height: 1,
                alpha_mode: ALPHA_STRAIGHT,
                delta_enabled: false,
                maximum_delta_operations: 1,
                zstd_enabled: false,
            }),
            target_latency_us: 0,
            maximum_latency_us: 100_000,
            retained_pixel_charge: 1,
        };
        let first_track = first_surface.track(7).unwrap();
        let second_track = second_surface.track(7).unwrap();
        scene.create_track(first_track, configuration(7)).unwrap();
        scene.create_track(second_track, configuration(7)).unwrap();

        scene.record_late_video_discard(first_track, 3, true).unwrap();
        scene.record_audio_rebase(first_track).unwrap();

        assert_eq!(
            scene.track_status(first_track).unwrap().metrics,
            TrackMetrics {
                discarded_late_frames: 3,
                latency_keyframe_requests: 1,
                audio_rebases: 1,
                ..TrackMetrics::default()
            }
        );
        assert_eq!(scene.track_status(second_track).unwrap().metrics, TrackMetrics::default());
    }

    #[test]
    fn target_generation_advances_for_live_and_late_sessions() {
        let scene = SharedScene::for_test();
        let first = session(1, 1);
        let second = session(1, 2);
        let late = session(1, 3);
        scene.register_session(first, TargetGeneration::ONE).unwrap();
        scene.register_session(second, TargetGeneration::ONE).unwrap();

        let resized = TargetGeneration::new(2);
        scene.advance_target_generation(resized);
        assert_eq!(scene.scene_status(first, 0).target_generation, resized);
        assert_eq!(scene.scene_status(second, 0).target_generation, resized);

        scene.register_session(late, TargetGeneration::ONE).unwrap();
        assert_eq!(scene.scene_status(late, 0).target_generation, resized);
        scene.advance_target_generation(resized);
        assert_eq!(scene.scene_status(late, 0).target_generation, resized);
    }

    #[test]
    fn scene_commit_is_atomic_on_a_missing_surface() {
        let scene = SharedScene::for_test();
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

    /// Retiring a replaced track leaves the surface untouched, so it must not move the revision
    /// the producer is holding: the producer names that revision in its next surface update, and
    /// a silent advance rejects the update it makes on the following resize.
    #[test]
    fn destroying_a_track_moves_the_surface_revision_only_when_it_vacates_a_slot() {
        let scene = SharedScene::for_test();
        let session = session(4, 1);
        let context = session.context(1).unwrap();
        let surface_identity = context.surface(1).unwrap();
        scene.register_session(session, TargetGeneration::ONE).unwrap();
        scene.create_surface(surface_identity, definition(1, 1)).unwrap();
        let raster = |track_id: u64| TrackConfiguration {
            context_id: 1,
            surface_id: 1,
            track_id,
            slot: SLOT_RASTER,
            mode: TrackMode::Live,
            lane: LaneClass::Bulk,
            maximum_record_body: 128,
            maximum_rate_millihertz: 1_000,
            maximum_encoded_bits_per_second: 8_192,
            maximum_records_per_second: 1,
            maximum_inflight_body_bytes: 1_024,
            kind: KindConfiguration::Raster(RasterConfiguration {
                width: 1,
                height: 1,
                alpha_mode: ALPHA_STRAIGHT,
                delta_enabled: false,
                maximum_delta_operations: 1,
                zstd_enabled: false,
            }),
            target_latency_us: 0,
            maximum_latency_us: 1_000_000,
            retained_pixel_charge: 1,
        };
        let ready = |track_id: u64| {
            let identity = surface_identity.track(track_id).unwrap();
            scene.create_track(identity, raster(track_id)).unwrap();
            scene.accept_channel(identity, ChannelGeneration::ONE, 1_024, 8).unwrap();
            scene.mark_output_ready(identity, ChannelGeneration::ONE).unwrap();
            identity
        };
        let retired = ready(1);
        let replacement = ready(2);

        // The replacement takes the slot, exactly as a settled resize does.
        scene
            .activate_tracks(
                surface_identity,
                SurfaceRevision::ONE,
                &[(SLOT_RASTER, 1, ChannelGeneration::ONE, MILESTONE_OUTPUT_READY)],
            )
            .unwrap();
        scene
            .activate_tracks(
                surface_identity,
                SurfaceRevision::new(2),
                &[(SLOT_RASTER, 2, ChannelGeneration::ONE, MILESTONE_OUTPUT_READY)],
            )
            .unwrap();
        let after_activation = scene.surface_status(surface_identity).unwrap().revision;

        scene.destroy_track(retired).unwrap();
        assert_eq!(
            scene.surface_status(surface_identity).unwrap().revision,
            after_activation,
            "retiring a track that held no slot changed the surface revision"
        );

        // Destroying the track that does hold the slot is a real surface mutation.
        scene.destroy_track(replacement).unwrap();
        assert_eq!(
            scene.surface_status(surface_identity).unwrap().revision,
            after_activation.advance().unwrap()
        );
    }

    /// A frame is composited from a snapshot and acknowledged after the GPU submit, so a resize
    /// landing in between supersedes it. That is ordinary window movement, not a presenter fault,
    /// and it must stay distinguishable from a record that genuinely could not be advanced.
    #[test]
    fn a_presentation_a_resize_overtook_is_superseded_rather_than_failed() {
        let scene = SharedScene::for_test();
        let session = session(6, 1);
        let context = session.context(1).unwrap();
        let surface_identity = context.surface(1).unwrap();
        let track_identity = surface_identity.track(1).unwrap();
        scene.register_session(session, TargetGeneration::ONE).unwrap();
        scene.create_surface(surface_identity, definition(1, 1)).unwrap();
        scene
            .create_track(
                track_identity,
                TrackConfiguration {
                    context_id: 1,
                    surface_id: 1,
                    track_id: 1,
                    slot: SLOT_RASTER,
                    mode: TrackMode::Live,
                    lane: LaneClass::Bulk,
                    maximum_record_body: 128,
                    maximum_rate_millihertz: 1_000,
                    maximum_encoded_bits_per_second: 8_192,
                    maximum_records_per_second: 1,
                    maximum_inflight_body_bytes: 1_024,
                    kind: KindConfiguration::Raster(RasterConfiguration {
                        width: 1,
                        height: 1,
                        alpha_mode: ALPHA_STRAIGHT,
                        delta_enabled: false,
                        maximum_delta_operations: 1,
                        zstd_enabled: false,
                    }),
                    target_latency_us: 0,
                    maximum_latency_us: 1_000_000,
                    retained_pixel_charge: 1,
                },
            )
            .unwrap();
        scene.accept_channel(track_identity, ChannelGeneration::ONE, 1_024, 8).unwrap();
        scene.mark_output_ready(track_identity, ChannelGeneration::ONE).unwrap();
        let frame = Frame {
            frame_id: 1,
            pts_us: 0,
            width: 1,
            height: 1,
            sar_num: 1,
            sar_den: 1,
            alpha_mode: ALPHA_STRAIGHT,
            rgba: Arc::new(RgbaBuffer::new(vec![0_u8, 0, 0, 255])),
            damage: None,
        };
        scene.publish_decoded_frame(track_identity, ChannelGeneration::ONE, frame).unwrap();
        let generation = scene.surface_status(surface_identity).unwrap().generation;

        // The frame that was composited against the current surface records normally.
        scene
            .mark_presented(track_identity, ChannelGeneration::ONE, generation, 1, 0)
            .expect("a frame composited against the live surface must record its presentation");

        // A resize replaces the surface mapping. The frame still on screen belongs to the target
        // that just went away.
        let mut resized = definition(1, 1);
        resized.logical_width = 20;
        let status = scene.surface_status(surface_identity).unwrap();
        scene
            .update_surface(surface_identity, status.revision, status.generation, resized)
            .unwrap();
        assert_eq!(
            scene.mark_presented(track_identity, ChannelGeneration::ONE, generation, 1, 0),
            Err(PresentationRejection::Superseded("stale surface generation"))
        );

        // A newer frame under the current surface supersedes an older frame's acknowledgement too.
        let current = scene.surface_status(surface_identity).unwrap().generation;
        assert_eq!(
            scene.mark_presented(track_identity, ChannelGeneration::ONE, current, 7, 0),
            Err(PresentationRejection::Superseded("stale frame presentation"))
        );
    }

    #[test]
    fn authenticated_anchor_positions_and_retains_a_clean_goodbye_poster() {
        let scene = SharedScene::for_test();
        let session = session(2, 1);
        let context = session.context(1).unwrap();
        let surface_identity = context.surface(1).unwrap();
        let track_identity = surface_identity.track(1).unwrap();
        let anchor_identity = context.anchor(9).unwrap();
        scene.register_session(session, TargetGeneration::ONE).unwrap();
        scene.create_surface(surface_identity, definition(1, 1)).unwrap();
        scene
            .create_track(
                track_identity,
                TrackConfiguration {
                    context_id: 1,
                    surface_id: 1,
                    track_id: 1,
                    slot: SLOT_RASTER,
                    mode: TrackMode::Live,
                    lane: LaneClass::Bulk,
                    maximum_record_body: 128,
                    maximum_rate_millihertz: 1_000,
                    maximum_encoded_bits_per_second: 8_192,
                    maximum_records_per_second: 1,
                    maximum_inflight_body_bytes: 1_024,
                    kind: KindConfiguration::Raster(RasterConfiguration {
                        width: 1,
                        height: 1,
                        alpha_mode: ALPHA_STRAIGHT,
                        delta_enabled: false,
                        maximum_delta_operations: 1,
                        zstd_enabled: false,
                    }),
                    target_latency_us: 0,
                    maximum_latency_us: 1_000_000,
                    retained_pixel_charge: 1,
                },
            )
            .unwrap();
        scene.accept_channel(track_identity, ChannelGeneration::ONE, 1_024, 8).unwrap();
        scene
            .publish_frame(
                track_identity,
                ChannelGeneration::ONE,
                80,
                1,
                1,
                true,
                2,
                Frame {
                    frame_id: 1,
                    pts_us: 0,
                    width: 1,
                    height: 1,
                    sar_num: 1,
                    sar_den: 1,
                    alpha_mode: ALPHA_STRAIGHT,
                    rgba: Arc::new(RgbaBuffer::new(vec![255, 0, 0, 255])),
                    damage: None,
                },
            )
            .unwrap();
        assert_eq!(scene.track_status(track_identity).unwrap().last_media_record_sequence, 2);
        assert!(scene.mark_eos(track_identity, ChannelGeneration::ONE, 1, 1).is_err());
        scene.mark_eos(track_identity, ChannelGeneration::ONE, 1, 2).unwrap();
        scene
            .activate_tracks(
                surface_identity,
                SurfaceRevision::ONE,
                &[(SLOT_RASTER, 1, ChannelGeneration::ONE, MILESTONE_OUTPUT_READY)],
            )
            .unwrap();
        scene.add_anchor(anchor_identity, 3, 5, false).unwrap();
        scene.begin_transaction(context, 1).unwrap();
        scene
            .queue_node_create(
                context,
                1,
                SceneNode {
                    owning_context_id: 1,
                    node_id: 1,
                    surface_context_id: 1,
                    surface_id: 1,
                    geometry: vec![
                        (0, Value::Unsigned(2)),
                        (1, Value::Unsigned(0)),
                        (2, Value::Unsigned(0)),
                        (3, Value::Unsigned(2_u64 << 32)),
                        (4, Value::Unsigned(1_u64 << 32)),
                        (5, Value::Unsigned(1)),
                        (6, Value::Unsigned(1)),
                        (7, Value::Unsigned(9)),
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
        scene
            .commit_transaction(context, 1, TargetGeneration::ONE, Some(SceneRevision::ZERO))
            .unwrap();
        let item = scene.snapshot().items[0].clone();
        assert_eq!((item.x, item.y), (3_i64 << 32, 5_i64 << 32));

        scene.detach_session(session);
        assert_eq!(scene.snapshot().items.len(), 1);
        assert!(scene.scroll_anchors(0, 24, 2, 0).is_empty());
        assert_eq!(scene.snapshot().items[0].y, 3_i64 << 32);
        assert_eq!(scene.clear_terminal(), vec![anchor_identity]);
        assert!(scene.snapshot().items.is_empty());
    }

    #[test]
    fn a_complete_priming_record_precedes_playback_clock_pacing() {
        let scene = SharedScene::for_test();
        let session = session(3, 1);
        let context = session.context(1).unwrap();
        let surface_identity = context.surface(1).unwrap();
        let track_identity = surface_identity.track(1).unwrap();
        let maximum_record_body = vivid_protocol::media::video_body_len(16).unwrap();
        scene.register_session(session, TargetGeneration::ONE).unwrap();
        scene.create_surface(surface_identity, definition(1, 1)).unwrap();
        scene
            .create_track(
                track_identity,
                TrackConfiguration {
                    context_id: 1,
                    surface_id: 1,
                    track_id: 1,
                    slot: SLOT_PRIMARY_VIDEO,
                    mode: TrackMode::Timed,
                    lane: LaneClass::Bulk,
                    maximum_record_body,
                    maximum_rate_millihertz: 30_000,
                    maximum_encoded_bits_per_second: 1_000_000,
                    maximum_records_per_second: 30,
                    maximum_inflight_body_bytes: u64::from(maximum_record_body) * 2,
                    kind: KindConfiguration::Video(VideoConfiguration {
                        codec: "av1".into(),
                        packetization: "av1-low-overhead-tu-v1".into(),
                        extradata: vec![],
                        coded_width: 1,
                        coded_height: 1,
                        profile: 0,
                        level: 0,
                        maximum_reorder_depth: 1,
                        color_primaries: 1,
                        transfer: 1,
                        matrix: 0,
                        signal_range: 1,
                        aspect_numerator: 1,
                        aspect_denominator: 1,
                        maximum_access_unit_bytes: 16,
                        codec_string: None,
                        decoder_configuration: None,
                    }),
                    target_latency_us: 0,
                    maximum_latency_us: 1_000_000,
                    retained_pixel_charge: 1,
                },
            )
            .unwrap();
        scene.accept_channel(track_identity, ChannelGeneration::ONE, 1_024, 8).unwrap();
        scene
            .publish_decoded_frame(
                track_identity,
                ChannelGeneration::ONE,
                Frame {
                    frame_id: 1,
                    pts_us: 0,
                    width: 1,
                    height: 1,
                    sar_num: 1,
                    sar_den: 1,
                    alpha_mode: ALPHA_STRAIGHT,
                    rgba: Arc::new(RgbaBuffer::new(vec![0, 0, 0, 255])),
                    damage: None,
                },
            )
            .unwrap();
        scene
            .activate_tracks(
                surface_identity,
                SurfaceRevision::ONE,
                &[(SLOT_PRIMARY_VIDEO, 1, ChannelGeneration::ONE, MILESTONE_OUTPUT_READY)],
            )
            .unwrap();

        let priming_scene = scene.clone();
        let (priming_done_tx, priming_done_rx) = std::sync::mpsc::channel();
        let priming = std::thread::spawn(move || {
            let result = priming_scene.wait_until_due(track_identity, 20_000, true);
            priming_done_tx.send(()).unwrap();
            result
        });
        if priming_done_rx.recv_timeout(Duration::from_millis(250)).is_err() {
            scene.start_playback(track_identity, 0).unwrap();
            let _ = priming.join();
            panic!("all decoder outputs from the priming record must finish before PLAY");
        }
        priming.join().unwrap().unwrap();

        let waiting_scene = scene.clone();
        let waiting =
            std::thread::spawn(move || waiting_scene.wait_until_due(track_identity, 40_000, false));
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            !waiting.is_finished(),
            "a second decoded frame must not replace the priming frame before PLAY"
        );

        let playback_started = Instant::now();
        scene.start_playback(track_identity, 0).unwrap();
        waiting.join().unwrap().unwrap();
        assert!(playback_started.elapsed() >= Duration::from_millis(25));
        assert!(matches!(
            scene.evaluate_track_wait(track_identity, ChannelGeneration::ONE, 5, None),
            TrackWaitEvaluation::Satisfied(_)
        ));

        scene.mark_eos(track_identity, ChannelGeneration::ONE, 0, 0).unwrap();
        scene.mark_buffered_ended(track_identity, ChannelGeneration::ONE).unwrap();
        assert!(matches!(
            scene.evaluate_track_wait(track_identity, ChannelGeneration::ONE, 6, None),
            TrackWaitEvaluation::Satisfied(_)
        ));
        scene.lose_track(track_identity).unwrap();
        assert!(matches!(
            scene.evaluate_track_wait(
                track_identity,
                ChannelGeneration::ONE,
                2,
                Some(MILESTONE_OUTPUT_READY)
            ),
            TrackWaitEvaluation::Lost
        ));
        assert!(matches!(
            scene.evaluate_track_wait(track_identity, ChannelGeneration::ONE, 9, None),
            TrackWaitEvaluation::Satisfied(_)
        ));
    }
}
