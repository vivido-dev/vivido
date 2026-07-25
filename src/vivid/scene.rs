use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use vivid_protocol::media;
use vivid_protocol::messages::{
    self, ClipRect, ImageSourceConfig, MAX_SCENE_NODES, ParsedAudioSourceConfig, ParsedNodeConfig,
    ParsedSceneNode, ParsedVideoSourceConfig, PlayRequest, PlaybackSnapshot, RasterSourceConfig,
    SceneCursor, SceneQuery, SceneStatus, SceneValidationKey, SceneValidationNode,
    SceneValidationSource, SourceStatus, WaitSatisfied, validate_scene_snapshot,
};
use vivid_protocol::revision::{SceneRevision, SourceRevision};

pub type SessionId = u64;
pub type SourceKey = (SessionId, u64);
pub type AnchorKey = (SessionId, u64);

const MAX_SOURCES: usize = 64;
const MAX_DECODED_PIXELS: u64 = 8192 * 8192 * 2;
const MAX_RESERVED_INGRESS_BYTES: u64 = 256 * 1024 * 1024;
const RESERVED_INGRESS_WINDOW: u64 = 4 * 1024 * 1024;
const MAX_SOURCE_TOMBSTONES: usize = 32;
const SOURCE_TOMBSTONE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub enum SourceConfig {
    Raster(RasterSourceConfig),
    Video(ParsedVideoSourceConfig),
    Image(ImageSourceConfig),
    Audio(ParsedAudioSourceConfig),
}

impl SourceConfig {
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            Self::Raster(config) => (config.width, config.height),
            Self::Video(config) => (config.width, config.height),
            Self::Image(config) => (config.width, config.height),
            Self::Audio(_) => (0, 0),
        }
    }

    fn maximum_body(&self) -> Option<u64> {
        match self {
            Self::Raster(config) => {
                media::rgba8_raw_frame_body_len(config.width, config.height).ok().map(u64::from)
            },
            Self::Video(config) => {
                media::video_body_len(config.max_access_unit_bytes).ok().map(u64::from)
            },
            Self::Image(config) => Some(u64::from(config.encoded_length)),
            Self::Audio(config) => {
                media::audio_body_len(config.max_access_unit_bytes).ok().map(u64::from)
            },
        }
    }

    fn kind(&self) -> u64 {
        match self {
            Self::Video(_) => messages::SOURCE_KIND_VIDEO,
            Self::Raster(_) => messages::SOURCE_KIND_RASTER,
            Self::Image(_) => messages::SOURCE_KIND_IMAGE,
            Self::Audio(_) => messages::SOURCE_KIND_AUDIO,
        }
    }

    fn linked_source_id(&self) -> u64 {
        match self {
            Self::Audio(config) => config.linked_video_source_id.unwrap_or(0),
            _ => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub frame_id: u64,
    pub pts_us: i64,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
    pub alpha_mode: u64,
    pub sar_num: u32,
    pub sar_den: u32,
}

#[derive(Debug, Clone)]
pub struct SceneNode {
    pub session_id: SessionId,
    pub node_id: u64,
    pub source_id: u64,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub text_layer: u64,
    pub z_index: i64,
    pub visible: bool,
    pub anchor_id: Option<u64>,
    pub clip: Option<ClipRect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextAnchor {
    column: usize,
    line: i32,
    /// Which screen the marker was consumed on. Anchored media renders only while its own
    /// screen is active, so a primary-screen image does not float above the alternate screen.
    alternate: bool,
}

#[derive(Debug, Clone)]
pub enum SceneMutation {
    Create(SceneNode),
    Update(SceneNode),
    Delete { session_id: SessionId, node_id: u64 },
}

impl SceneNode {
    pub fn from_protocol(session_id: SessionId, config: ParsedSceneNode) -> Self {
        let node = config.node;
        Self {
            session_id,
            node_id: node.node_id,
            source_id: node.source_id,
            x: node.x,
            y: node.y,
            width: node.width,
            height: node.height,
            text_layer: node.text_layer,
            z_index: node.z_index,
            visible: node.visible,
            anchor_id: node.anchor_id,
            clip: config.clip,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RenderItem {
    pub source_key: SourceKey,
    pub node_id: u64,
    pub frame: Frame,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub text_layer: u64,
    pub z_index: i64,
    pub text_anchored: bool,
    pub clip: Option<ClipRect>,
}

#[derive(Debug)]
struct Source {
    config: SourceConfig,
    revision: SourceRevision,
    field_revisions: [u64; 9],
    lifecycle: u64,
    milestones: u64,
    attachment_state: u64,
    attachment_generation: u64,
    last_media_id: u64,
    last_media_sequence: u64,
    last_decoded_pts_us: i64,
    last_presented_pts_us: i64,
    last_presented_media_id: u64,
    last_presentation_id: u64,
    visible: bool,
    latest_frame: Option<Frame>,
    play_started: Option<Instant>,
    played_before_pause: Duration,
    first_pts_us: Option<i64>,
    play_request: Option<PlayRequest>,
    buffered_until_pts_us: Option<i64>,
    last_epoch: u32,
    eos_epoch: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceObservation {
    pub revision: SourceRevision,
    pub field_revisions: [u64; 9],
    pub lifecycle: u64,
    pub kind: u64,
    pub milestones: u64,
    pub epoch: u32,
    pub attachment_state: u64,
    pub attachment_generation: u64,
    pub last_media_id: u64,
    pub last_media_sequence: u64,
    pub last_decoded_pts_us: i64,
    pub last_presented_pts_us: i64,
    pub last_presented_media_id: u64,
    pub last_presentation_id: u64,
    pub linked_source_id: u64,
    pub visible: bool,
    pub terminal_loss_code: Option<u64>,
}

#[derive(Debug, Clone)]
struct SourceTombstone {
    observation: SourceObservation,
    expires_at: Instant,
}

#[derive(Debug, Default)]
struct State {
    sources: HashMap<SourceKey, Source>,
    tombstones: HashMap<SourceKey, SourceTombstone>,
    tombstone_order: VecDeque<SourceKey>,
    nodes: HashMap<(SessionId, u64), SceneNode>,
    anchors: HashMap<AnchorKey, TextAnchor>,
    gone_anchors: HashSet<AnchorKey>,
    gone_anchor_order: VecDeque<AnchorKey>,
    detached_sessions: HashSet<SessionId>,
    decoded_pixels: u64,
    queued_pixels: u64,
    revision: u64,
    scene_revisions: HashMap<SessionId, SceneRevision>,
    scene_change_reasons: HashMap<SessionId, u64>,
    alternate_screen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionObservationSnapshot {
    pub scene_revision: SceneRevision,
    pub scene_change_reasons: u64,
    pub sources: Vec<(u64, SourceObservation, Option<PlaybackSnapshot>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceWaitEvaluation {
    Satisfied(WaitSatisfied),
    Pending,
    NotVisible,
    NotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SceneCounts {
    pub sources: u64,
    pub nodes: u64,
    pub anchors: u64,
    pub retained_pixels: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SceneQueryPrecondition {
    pub current_revision: SceneRevision,
}

#[derive(Debug, Default)]
struct Inner {
    state: Mutex<State>,
    playback_changed: Condvar,
}

#[derive(Clone, Debug, Default)]
pub struct SharedScene(Arc<Inner>);

fn advance_scene_revision(
    state: &mut State,
    session_id: SessionId,
    reason: u64,
) -> Result<SceneRevision, &'static str> {
    let revision = state.scene_revisions.entry(session_id).or_default();
    *revision = revision.advance().map_err(|_| "scene revision exhausted")?;
    *state.scene_change_reasons.entry(session_id).or_default() |= reason;
    Ok(*revision)
}

fn advance_changed_scenes(
    state: &mut State,
    sessions: impl IntoIterator<Item = SessionId>,
    reason: u64,
) -> Result<(), &'static str> {
    for session_id in sessions.into_iter().collect::<HashSet<_>>() {
        advance_scene_revision(state, session_id, reason)?;
    }
    Ok(())
}

fn advance_source_revision(
    source: &mut Source,
    changed_fields: u64,
) -> Result<SourceRevision, &'static str> {
    if changed_fields == 0 || changed_fields & !messages::SOURCE_CHANGED_FIELD_MASK != 0 {
        return Err("source revision requires valid changed fields");
    }
    source.revision = source.revision.advance().map_err(|_| "source revision exhausted")?;
    for (bit, revision) in source.field_revisions.iter_mut().enumerate() {
        if changed_fields & (1 << bit) != 0 {
            *revision = source.revision.get();
        }
    }
    Ok(source.revision)
}

fn set_milestone(source: &mut Source, milestone: u64) -> bool {
    let previous = source.milestones;
    source.milestones |= milestone;
    source.milestones != previous
}

const fn changed_field(changed: bool, field: u64) -> u64 {
    if changed { field } else { 0 }
}

fn observation(source: &Source, terminal_loss_code: Option<u64>) -> SourceObservation {
    SourceObservation {
        revision: source.revision,
        field_revisions: source.field_revisions,
        lifecycle: source.lifecycle,
        kind: source.config.kind(),
        milestones: source.milestones,
        epoch: source.last_epoch,
        attachment_state: source.attachment_state,
        attachment_generation: source.attachment_generation,
        last_media_id: source.last_media_id,
        last_media_sequence: source.last_media_sequence,
        last_decoded_pts_us: source.last_decoded_pts_us,
        last_presented_pts_us: source.last_presented_pts_us,
        last_presented_media_id: source.last_presented_media_id,
        last_presentation_id: source.last_presentation_id,
        linked_source_id: source.config.linked_source_id(),
        visible: source.visible,
        terminal_loss_code,
    }
}

fn purge_expired_tombstones(state: &mut State, now: Instant) {
    while let Some(key) = state.tombstone_order.front().copied() {
        let expired =
            state.tombstones.get(&key).is_none_or(|tombstone| tombstone.expires_at <= now);
        if !expired {
            break;
        }
        state.tombstone_order.pop_front();
        state.tombstones.remove(&key);
    }
}

fn insert_anchor(
    state: &mut State,
    session_id: SessionId,
    anchor_id: u64,
    column: usize,
    line: i32,
    alternate: bool,
) -> Result<(), &'static str> {
    if anchor_id == 0 {
        return Err("anchor ID is zero");
    }
    if state.anchors.len() >= MAX_SCENE_NODES {
        return Err("anchor quota exceeded");
    }
    if state
        .anchors
        .insert((session_id, anchor_id), TextAnchor { column, line, alternate })
        .is_some()
    {
        return Err("anchor ID already exists");
    }
    state.gone_anchors.remove(&(session_id, anchor_id));
    state.gone_anchor_order.retain(|key| *key != (session_id, anchor_id));
    state.revision = state.revision.wrapping_add(1);
    Ok(())
}

fn retain_gone_anchors(state: &mut State, removed: &[AnchorKey]) {
    for key in removed {
        if state.gone_anchors.insert(*key) {
            state.gone_anchor_order.push_back(*key);
        }
    }
    while state.gone_anchor_order.len() > MAX_SCENE_NODES {
        if let Some(key) = state.gone_anchor_order.pop_front() {
            state.gone_anchors.remove(&key);
        }
    }
}

impl SharedScene {
    pub fn anchor_positions(&self) -> Vec<(AnchorKey, usize, i32, bool)> {
        let state = self.lock();
        state
            .anchors
            .iter()
            .map(|(&key, anchor)| (key, anchor.column, anchor.line, anchor.alternate))
            .collect()
    }

    /// Apply terminal resize/reflow results and remove anchors whose semantic positions vanished.
    pub fn apply_anchor_resize(
        &self,
        positions: impl IntoIterator<Item = (AnchorKey, Option<(usize, i32, bool)>)>,
    ) -> Result<Vec<AnchorKey>, &'static str> {
        let mut state = self.lock();
        let mut removed = Vec::new();
        let mut changed = false;
        for (key, position) in positions {
            let Some(anchor) = state.anchors.get_mut(&key) else {
                continue;
            };
            match position {
                Some((column, line, alternate)) => {
                    changed |= anchor.column != column
                        || anchor.line != line
                        || anchor.alternate != alternate;
                    *anchor = TextAnchor { column, line, alternate };
                },
                None => removed.push(key),
            }
        }

        if !removed.is_empty() {
            let removed_set = removed.iter().copied().collect::<HashSet<_>>();
            let changed_sessions = state
                .nodes
                .iter()
                .filter_map(|(&(session_id, _), node)| {
                    node.anchor_id
                        .is_some_and(|anchor_id| removed_set.contains(&(session_id, anchor_id)))
                        .then_some(session_id)
                })
                .collect::<Vec<_>>();
            advance_changed_scenes(
                &mut state,
                changed_sessions,
                messages::SCENE_CHANGED_ANCHOR_GONE,
            )?;
            state.anchors.retain(|key, _| !removed_set.contains(key));
            retain_gone_anchors(&mut state, &removed);
            state.nodes.retain(|(session_id, _), node| {
                node.anchor_id
                    .is_none_or(|anchor_id| !removed_set.contains(&(*session_id, anchor_id)))
            });
            gc_detached_sources(&mut state);
            changed = true;
        }
        if changed {
            state.revision = state.revision.wrapping_add(1);
        }
        Ok(removed)
    }

    #[cfg(test)]
    pub fn add_anchor(
        &self,
        session_id: SessionId,
        anchor_id: u64,
        column: usize,
        line: i32,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let alternate = state.alternate_screen;
        insert_anchor(&mut state, session_id, anchor_id, column, line, alternate)
    }

    /// Add an anchor to the terminal screen which contained its marker. The terminal parser is
    /// authoritative because its screen-swap and marker events can be delivered independently.
    pub fn add_anchor_for_screen(
        &self,
        session_id: SessionId,
        anchor_id: u64,
        column: usize,
        line: i32,
        alternate: bool,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        insert_anchor(&mut state, session_id, anchor_id, column, line, alternate)
    }

    /// Record which screen the terminal presents. Anchors belonging to the inactive screen stay
    /// registered but hidden; anchors created on the alternate screen are discarded when that
    /// screen is left, because its content does not survive the switch.
    pub fn set_alternate_screen(&self, alternate: bool) -> Result<Vec<AnchorKey>, &'static str> {
        let mut state = self.lock();
        if state.alternate_screen == alternate {
            return Ok(Vec::new());
        }
        state.alternate_screen = alternate;
        let mut removed = Vec::new();
        if !alternate {
            removed = state
                .anchors
                .iter()
                .filter(|(_, anchor)| anchor.alternate)
                .map(|(&key, _)| key)
                .collect();
            if !removed.is_empty() {
                let removed_set = removed.iter().copied().collect::<HashSet<_>>();
                let changed_sessions = state
                    .nodes
                    .iter()
                    .filter_map(|(&(session_id, _), node)| {
                        node.anchor_id
                            .is_some_and(|anchor_id| removed_set.contains(&(session_id, anchor_id)))
                            .then_some(session_id)
                    })
                    .collect::<Vec<_>>();
                advance_changed_scenes(
                    &mut state,
                    changed_sessions,
                    messages::SCENE_CHANGED_ANCHOR_GONE,
                )?;
                state.anchors.retain(|key, _| !removed_set.contains(key));
                retain_gone_anchors(&mut state, &removed);
                state.nodes.retain(|(session_id, _), node| {
                    node.anchor_id
                        .is_none_or(|anchor_id| !removed_set.contains(&(*session_id, anchor_id)))
                });
                gc_detached_sources(&mut state);
            }
        }
        state.revision = state.revision.wrapping_add(1);
        self.0.playback_changed.notify_all();
        Ok(removed)
    }

    pub fn add_source(
        &self,
        session_id: SessionId,
        source_id: u64,
        config: SourceConfig,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let key = (session_id, source_id);
        purge_expired_tombstones(&mut state, Instant::now());
        if state.sources.contains_key(&key) || state.tombstones.contains_key(&key) {
            return Err("source ID already exists");
        }
        if state.sources.len() >= MAX_SOURCES {
            return Err("source quota exceeded");
        }
        let (width, height) = config.dimensions();
        if !matches!(config, SourceConfig::Audio(_))
            && (width == 0 || height == 0 || width > 8192 || height > 8192)
        {
            return Err("source dimensions are outside limits");
        }
        let requested_pixels = u64::from(width) * u64::from(height);
        let reserved_pixels = state
            .sources
            .values()
            .try_fold(0_u64, |total, source| {
                let (width, height) = source.config.dimensions();
                total.checked_add(u64::from(width) * u64::from(height))
            })
            .ok_or("source pixel reservation overflow")?;
        if reserved_pixels.saturating_add(requested_pixels) > MAX_DECODED_PIXELS {
            return Err("decoded pixel reservation quota exceeded");
        }
        let requested_ingress = config
            .maximum_body()
            .ok_or("source media body is invalid")?
            .max(RESERVED_INGRESS_WINDOW);
        let reserved_ingress = state
            .sources
            .values()
            .try_fold(0_u64, |total, source| {
                total.checked_add(source.config.maximum_body()?.max(RESERVED_INGRESS_WINDOW))
            })
            .ok_or("source ingress reservation overflow")?;
        if reserved_ingress.saturating_add(requested_ingress) > MAX_RESERVED_INGRESS_BYTES {
            return Err("source ingress reservation quota exceeded");
        }
        state.sources.insert(
            key,
            Source {
                config,
                revision: SourceRevision::new(1),
                field_revisions: [1, 0, 0, 0, 0, 0, 0, 0, 0],
                lifecycle: messages::SOURCE_LIFECYCLE_CREATED,
                milestones: 0,
                attachment_state: messages::ATTACHMENT_NEVER,
                attachment_generation: 0,
                last_media_id: 0,
                last_media_sequence: 0,
                last_decoded_pts_us: 0,
                last_presented_pts_us: 0,
                last_presented_media_id: 0,
                last_presentation_id: 0,
                visible: false,
                latest_frame: None,
                play_started: None,
                played_before_pause: Duration::ZERO,
                first_pts_us: None,
                play_request: None,
                buffered_until_pts_us: None,
                last_epoch: 0,
                eos_epoch: None,
            },
        );
        Ok(())
    }

    pub fn source_config(&self, key: SourceKey) -> Option<SourceConfig> {
        self.lock().sources.get(&key).map(|source| source.config.clone())
    }

    pub fn scene_revision(&self, session_id: SessionId) -> SceneRevision {
        self.lock().scene_revisions.get(&session_id).copied().unwrap_or_default()
    }

    pub fn note_context_revocation(
        &self,
        authority_root_session: SessionId,
    ) -> Result<SceneRevision, &'static str> {
        let mut state = self.lock();
        advance_scene_revision(
            &mut state,
            authority_root_session,
            messages::SCENE_CHANGED_CONTEXT_REVOKED,
        )
    }

    pub fn source_observation(&self, key: SourceKey) -> Option<SourceObservation> {
        let mut state = self.lock();
        purge_expired_tombstones(&mut state, Instant::now());
        state
            .sources
            .get(&key)
            .map(|source| observation(source, None))
            .or_else(|| state.tombstones.get(&key).map(|tombstone| tombstone.observation))
    }

    pub fn source_content_revision(&self, key: SourceKey) -> Option<u64> {
        self.lock().sources.get(&key).map(|_| 0)
    }

    pub fn anchor_state(&self, session_id: SessionId, anchor_id: u64) -> u64 {
        let state = self.lock();
        if state.anchors.contains_key(&(session_id, anchor_id)) {
            messages::ANCHOR_STATE_READY
        } else if state.gone_anchors.contains(&(session_id, anchor_id)) {
            messages::ANCHOR_STATE_GONE
        } else {
            messages::ANCHOR_STATE_UNKNOWN
        }
    }

    pub fn take_observation_snapshot(&self, session_id: SessionId) -> SessionObservationSnapshot {
        let mut state = self.lock();
        let now = Instant::now();
        purge_expired_tombstones(&mut state, now);
        let scene_revision = state.scene_revisions.get(&session_id).copied().unwrap_or_default();
        let scene_change_reasons = state.scene_change_reasons.remove(&session_id).unwrap_or(0);
        let mut sources = state
            .sources
            .iter()
            .filter(|((owner, _), _)| *owner == session_id)
            .map(|((_, source_id), source)| {
                (*source_id, observation(source, None), timed_playback_snapshot(source, now))
            })
            .chain(state.tombstones.iter().filter_map(|(&(owner, source_id), tombstone)| {
                (owner == session_id).then_some((
                    source_id,
                    tombstone.observation,
                    tombstone_playback_snapshot(tombstone.observation),
                ))
            }))
            .collect::<Vec<_>>();
        sources.sort_by_key(|(source_id, _, _)| *source_id);
        SessionObservationSnapshot { scene_revision, scene_change_reasons, sources }
    }

    pub fn source_status(
        &self,
        key: SourceKey,
        outstanding_byte_credit: u64,
        outstanding_packet_credit: u64,
    ) -> Option<SourceStatus> {
        let mut state = self.lock();
        let now = Instant::now();
        purge_expired_tombstones(&mut state, now);
        if let Some(source) = state.sources.get(&key) {
            let observed = observation(source, None);
            return Some(source_status_from_observation(
                key.1,
                observed,
                outstanding_byte_credit.max(source.config.maximum_body().unwrap_or(0)),
                outstanding_packet_credit,
                timed_playback_snapshot(source, now),
            ));
        }
        state.tombstones.get(&key).map(|tombstone| {
            source_status_from_observation(
                key.1,
                tombstone.observation,
                0,
                0,
                tombstone_playback_snapshot(tombstone.observation),
            )
        })
    }

    pub fn source_keys(&self) -> Vec<SourceKey> {
        let mut state = self.lock();
        purge_expired_tombstones(&mut state, Instant::now());
        let mut keys =
            state.sources.keys().chain(state.tombstones.keys()).copied().collect::<Vec<_>>();
        keys.sort_unstable();
        keys
    }

    pub fn scene_status(
        &self,
        session_id: SessionId,
        query: &SceneQuery,
    ) -> Result<SceneStatus, SceneQueryPrecondition> {
        let state = self.lock();
        let revision = state.scene_revisions.get(&session_id).copied().unwrap_or_default();
        if query.expected_revision.is_some_and(|expected| expected != revision)
            || query.cursor.is_some_and(|cursor| cursor.scene_revision != revision)
        {
            return Err(SceneQueryPrecondition { current_revision: revision });
        }
        let mut nodes = state
            .nodes
            .iter()
            .filter(|((owner, _), _)| *owner == session_id)
            .map(|(_, node)| ParsedSceneNode {
                node: ParsedNodeConfig {
                    node_id: node.node_id,
                    source_id: node.source_id,
                    context_id: (session_id << 32) | 1,
                    x: node.x,
                    y: node.y,
                    width: node.width,
                    height: node.height,
                    text_layer: node.text_layer,
                    z_index: node.z_index,
                    visible: node.visible,
                    anchor_id: node.anchor_id,
                },
                clip: node.clip,
            })
            .collect::<Vec<_>>();
        nodes.sort_by_key(|node| node.node.node_id);
        let total_nodes = nodes.len() as u64;
        let offset = query.cursor.map_or(0, |cursor| cursor.offset);
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(nodes.len());
        let maximum = query.maximum_nodes.unwrap_or(64).min(MAX_SCENE_NODES as u64) as usize;
        let end = start.saturating_add(maximum).min(nodes.len());
        let page = nodes[start..end].to_vec();
        let cursor = (end < nodes.len())
            .then_some(SceneCursor { scene_revision: revision, offset: end as u64 });
        Ok(SceneStatus { scene_revision: revision, nodes: page, cursor, total_nodes })
    }

    pub fn anchor_status(
        &self,
        session_id: SessionId,
        anchor_id: u64,
        columns: u32,
        rows: u32,
        display_offset: usize,
        display_generation: u64,
    ) -> messages::AnchorStatus {
        let state = self.lock();
        let Some(anchor) = state.anchors.get(&(session_id, anchor_id)) else {
            return messages::AnchorStatus {
                anchor_id,
                state: if state.gone_anchors.contains(&(session_id, anchor_id)) {
                    messages::ANCHOR_STATE_GONE
                } else {
                    messages::ANCHOR_STATE_UNKNOWN
                },
                column: 0,
                row: 0,
                visible: false,
                display_generation,
            };
        };
        let viewport_row = i64::from(anchor.line).saturating_add(display_offset as i64);
        let visible = anchor.alternate == state.alternate_screen
            && anchor.column < columns as usize
            && (0..i64::from(rows)).contains(&viewport_row);
        messages::AnchorStatus {
            anchor_id,
            state: messages::ANCHOR_STATE_READY,
            column: anchor.column as u64,
            row: viewport_row.max(0) as u64,
            visible,
            display_generation,
        }
    }

    pub fn counts(&self, session_id: SessionId) -> SceneCounts {
        let state = self.lock();
        SceneCounts {
            sources: state.sources.keys().filter(|(owner, _)| *owner == session_id).count() as u64,
            nodes: state.nodes.keys().filter(|(owner, _)| *owner == session_id).count() as u64,
            anchors: state.anchors.keys().filter(|(owner, _)| *owner == session_id).count() as u64,
            retained_pixels: state
                .sources
                .iter()
                .filter(|((owner, _), _)| *owner == session_id)
                .filter_map(|(_, source)| source.latest_frame.as_ref())
                .map(|frame| u64::from(frame.width) * u64::from(frame.height))
                .sum(),
        }
    }

    pub fn configured_pixel_capacity(&self, session_id: SessionId) -> u64 {
        self.lock()
            .sources
            .iter()
            .filter(|((owner, _), _)| *owner == session_id)
            .map(|(_, source)| match &source.config {
                SourceConfig::Raster(config) => u64::from(config.width) * u64::from(config.height),
                SourceConfig::Video(config) => u64::from(config.width) * u64::from(config.height),
                SourceConfig::Image(config) => u64::from(config.width) * u64::from(config.height),
                SourceConfig::Audio(_) => 0,
            })
            .sum()
    }

    pub fn evaluate_wait(
        &self,
        key: SourceKey,
        condition: u64,
        value: Option<u64>,
    ) -> SourceWaitEvaluation {
        let mut state = self.lock();
        purge_expired_tombstones(&mut state, Instant::now());
        let observed = state
            .sources
            .get(&key)
            .map(|source| observation(source, None))
            .or_else(|| state.tombstones.get(&key).map(|tombstone| tombstone.observation));
        let Some(observed) = observed else {
            return SourceWaitEvaluation::NotFound;
        };
        let satisfied = match condition {
            messages::WAIT_SOURCE_REVISION => (observed.revision.get() > value.unwrap_or(u64::MAX))
                .then_some(observed.revision.get()),
            messages::WAIT_FIRST_VISIBLE_PRESENTATION => {
                (observed.milestones & messages::MILESTONE_FIRST_VISIBLE_PRESENTATION != 0)
                    .then_some(observed.last_presentation_id)
            },
            messages::WAIT_RASTER_FRAME => (observed.kind == messages::SOURCE_KIND_RASTER
                && observed.last_presented_media_id >= value.unwrap_or(u64::MAX))
            .then_some(observed.last_presented_media_id),
            messages::WAIT_VIDEO_PTS => value
                .and_then(|value| i64::try_from(value).ok())
                .filter(|value| {
                    observed.kind == messages::SOURCE_KIND_VIDEO
                        && observed.last_presented_pts_us >= *value
                })
                .map(|_| observed.last_presented_pts_us.max(0) as u64),
            messages::WAIT_PLAYBACK_STARTED => {
                (observed.milestones & messages::MILESTONE_PLAYBACK_STARTED != 0).then_some(0)
            },
            messages::WAIT_PLAYBACK_ENDED => {
                (observed.milestones & messages::MILESTONE_PLAYBACK_ENDED != 0).then_some(0)
            },
            messages::WAIT_MEDIA_ATTACHED => {
                (observed.milestones & messages::MILESTONE_MEDIA_ATTACHED != 0)
                    .then_some(observed.attachment_generation)
            },
            messages::WAIT_MEDIA_CLOSED => (observed.attachment_state
                == messages::ATTACHMENT_CLOSED)
                .then_some(observed.attachment_generation),
            messages::WAIT_SOURCE_LOST => (observed.milestones & messages::MILESTONE_SOURCE_LOST
                != 0)
                .then_some(observed.terminal_loss_code.unwrap_or(0)),
            _ => None,
        };
        if let Some(observed_value) = satisfied {
            return SourceWaitEvaluation::Satisfied(WaitSatisfied {
                source_id: key.1,
                source_revision: observed.revision,
                condition,
                observed_value: matches!(
                    condition,
                    messages::WAIT_SOURCE_REVISION
                        | messages::WAIT_RASTER_FRAME
                        | messages::WAIT_VIDEO_PTS
                )
                .then_some(observed_value),
            });
        }
        if matches!(
            condition,
            messages::WAIT_FIRST_VISIBLE_PRESENTATION
                | messages::WAIT_RASTER_FRAME
                | messages::WAIT_VIDEO_PTS
        ) && !observed.visible
        {
            SourceWaitEvaluation::NotVisible
        } else {
            SourceWaitEvaluation::Pending
        }
    }

    pub fn mark_attached(&self, key: SourceKey) -> Result<SourceObservation, &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        source.attachment_generation =
            source.attachment_generation.checked_add(1).ok_or("attachment generation exhausted")?;
        source.attachment_state = messages::ATTACHMENT_ATTACHED;
        source.lifecycle = messages::SOURCE_LIFECYCLE_ATTACHED;
        set_milestone(source, messages::MILESTONE_MEDIA_ATTACHED);
        advance_source_revision(
            source,
            messages::SOURCE_CHANGED_LIFECYCLE
                | messages::SOURCE_CHANGED_ATTACHMENT
                | messages::SOURCE_CHANGED_MILESTONES,
        )?;
        Ok(observation(source, None))
    }

    pub fn mark_attachment_closed(&self, key: SourceKey) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        if source.attachment_state != messages::ATTACHMENT_CLOSED {
            source.attachment_state = messages::ATTACHMENT_CLOSED;
            advance_source_revision(source, messages::SOURCE_CHANGED_ATTACHMENT)?;
        }
        Ok(())
    }

    pub fn mark_media_accepted(
        &self,
        key: SourceKey,
        epoch: u32,
        media_id: u64,
        record_sequence: u64,
        random_access: bool,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        if epoch < source.last_epoch {
            return Err("stale source epoch");
        }
        let epoch_changed = source.last_epoch != epoch;
        source.last_epoch = epoch;
        source.last_media_id = media_id;
        source.last_media_sequence = record_sequence;
        let lifecycle_changed = source.lifecycle != messages::SOURCE_LIFECYCLE_ACTIVE;
        source.lifecycle = messages::SOURCE_LIFECYCLE_ACTIVE;
        let mut milestone_changed = set_milestone(source, messages::MILESTONE_FIRST_MEDIA_RECORD);
        if random_access {
            milestone_changed |= set_milestone(source, messages::MILESTONE_RANDOM_ACCESS_ACCEPTED);
        }
        let changed_fields = changed_field(epoch_changed, messages::SOURCE_CHANGED_EPOCH)
            | changed_field(lifecycle_changed, messages::SOURCE_CHANGED_LIFECYCLE)
            | changed_field(milestone_changed, messages::SOURCE_CHANGED_MILESTONES);
        if changed_fields != 0 {
            advance_source_revision(source, changed_fields)?;
        }
        Ok(())
    }

    pub fn mark_decoder_initialized(&self, key: SourceKey) -> Result<(), &'static str> {
        self.mark_source_milestone(key, messages::MILESTONE_DECODER_INITIALIZED)
    }

    pub fn mark_decoded_output(&self, key: SourceKey, pts_us: i64) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        source.last_decoded_pts_us = pts_us;
        if set_milestone(source, messages::MILESTONE_FIRST_DECODED_OUTPUT) {
            advance_source_revision(source, messages::SOURCE_CHANGED_MILESTONES)?;
        }
        self.0.playback_changed.notify_all();
        Ok(())
    }

    pub fn mark_presented(
        &self,
        key: SourceKey,
        media_id: u64,
        pts_us: i64,
        visible: bool,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        source.last_presentation_id =
            source.last_presentation_id.checked_add(1).ok_or("presentation ID exhausted")?;
        source.last_presented_media_id = media_id;
        source.last_presented_pts_us = pts_us;
        if visible && set_milestone(source, messages::MILESTONE_FIRST_VISIBLE_PRESENTATION) {
            advance_source_revision(source, messages::SOURCE_CHANGED_MILESTONES)?;
        }
        self.0.playback_changed.notify_all();
        Ok(())
    }

    pub fn mark_playback_ended(&self, key: SourceKey) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        let lifecycle_changed = source.lifecycle != messages::SOURCE_LIFECYCLE_ENDED;
        let milestone_changed = set_milestone(source, messages::MILESTONE_PLAYBACK_ENDED);
        source.lifecycle = messages::SOURCE_LIFECYCLE_ENDED;
        if lifecycle_changed || milestone_changed {
            advance_source_revision(
                source,
                messages::SOURCE_CHANGED_LIFECYCLE
                    | messages::SOURCE_CHANGED_PLAYBACK
                    | messages::SOURCE_CHANGED_MILESTONES,
            )?;
        }
        self.0.playback_changed.notify_all();
        Ok(())
    }

    #[allow(dead_code)] // Called by the Stage 3 policy command path.
    pub fn mark_policy_changed(&self, key: SourceKey) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        advance_source_revision(source, messages::SOURCE_CHANGED_CAPTURE_POLICY)?;
        Ok(())
    }

    #[allow(dead_code)] // Called by the Stage 3 descriptor command path.
    pub fn mark_descriptor_changed(&self, key: SourceKey) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        advance_source_revision(source, messages::SOURCE_CHANGED_DESCRIPTOR)?;
        Ok(())
    }

    fn mark_source_milestone(&self, key: SourceKey, milestone: u64) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        if set_milestone(source, milestone) {
            advance_source_revision(source, messages::SOURCE_CHANGED_MILESTONES)?;
        }
        Ok(())
    }

    pub fn start_playback(&self, key: SourceKey, request: PlayRequest) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        if request.source_id != key.1
            || !matches!(source.config, SourceConfig::Video(_) | SourceConfig::Audio(_))
        {
            return Err("PLAY source does not match a timed source");
        }
        let resume = source.play_request == Some(request)
            && source.play_started.is_none()
            && source.first_pts_us == Some(request.start_pts_us);
        source.play_request = Some(request);
        source.first_pts_us = Some(request.start_pts_us);
        if !resume {
            source.played_before_pause = Duration::ZERO;
            source.play_started = None;
        }
        source.lifecycle = messages::SOURCE_LIFECYCLE_ACTIVE;
        let started = maybe_start_buffered(source);
        if started {
            set_milestone(source, messages::MILESTONE_PLAYBACK_STARTED);
        }
        let mut changed_fields =
            messages::SOURCE_CHANGED_LIFECYCLE | messages::SOURCE_CHANGED_PLAYBACK;
        if started {
            changed_fields |= messages::SOURCE_CHANGED_MILESTONES;
        }
        advance_source_revision(source, changed_fields)?;
        self.0.playback_changed.notify_all();
        Ok(())
    }

    /// Record decoded/pre-roll progress without blocking the source's ingress worker.
    pub fn observe_buffered_pts(&self, key: SourceKey, pts_us: i64) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        source.buffered_until_pts_us =
            Some(source.buffered_until_pts_us.map_or(pts_us, |current| current.max(pts_us)));
        if maybe_start_buffered(source) {
            set_milestone(source, messages::MILESTONE_PLAYBACK_STARTED);
            advance_source_revision(
                source,
                messages::SOURCE_CHANGED_PLAYBACK | messages::SOURCE_CHANGED_MILESTONES,
            )?;
        }
        self.0.playback_changed.notify_all();
        Ok(())
    }

    pub fn is_before_play_start(&self, key: SourceKey, pts_us: i64) -> bool {
        self.lock()
            .sources
            .get(&key)
            .and_then(|source| source.play_request)
            .is_some_and(|request| pts_us < request.start_pts_us)
    }

    pub fn pause_playback(&self, key: SourceKey) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        if !matches!(source.config, SourceConfig::Video(_) | SourceConfig::Audio(_)) {
            return Err("PAUSE applies only to video or audio");
        }
        if let Some(started) = source.play_started.take() {
            source.played_before_pause =
                source.played_before_pause.saturating_add(started.elapsed());
        }
        source.lifecycle = messages::SOURCE_LIFECYCLE_PAUSED;
        advance_source_revision(
            source,
            messages::SOURCE_CHANGED_LIFECYCLE | messages::SOURCE_CHANGED_PLAYBACK,
        )?;
        self.0.playback_changed.notify_all();
        Ok(())
    }

    pub fn flush_playback(&self, key: SourceKey, epoch: u32) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        if !matches!(source.config, SourceConfig::Video(_) | SourceConfig::Audio(_))
            || epoch <= source.last_epoch
        {
            return Err("FLUSH requires a media source and a greater epoch");
        }
        source.last_epoch = epoch;
        source.play_started = None;
        source.played_before_pause = Duration::ZERO;
        source.first_pts_us = None;
        source.play_request = None;
        source.buffered_until_pts_us = None;
        source.eos_epoch = None;
        source.lifecycle = messages::SOURCE_LIFECYCLE_PAUSED;
        advance_source_revision(
            source,
            messages::SOURCE_CHANGED_LIFECYCLE
                | messages::SOURCE_CHANGED_EPOCH
                | messages::SOURCE_CHANGED_PLAYBACK,
        )?;
        self.0.playback_changed.notify_all();
        Ok(())
    }

    pub fn signal_eos(&self, key: SourceKey, epoch: u32) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        if epoch < source.last_epoch {
            return Err("stale source epoch");
        }
        let epoch_changed = source.last_epoch != epoch;
        source.last_epoch = epoch;
        let eos_changed = source.eos_epoch != Some(epoch);
        source.eos_epoch = Some(epoch);
        let eos_milestone_changed = set_milestone(source, messages::MILESTONE_EOS_ACCEPTED);
        let playback_started = maybe_start_buffered(source);
        if playback_started {
            set_milestone(source, messages::MILESTONE_PLAYBACK_STARTED);
        }
        if epoch_changed || eos_changed || eos_milestone_changed || playback_started {
            let mut changed_fields = messages::SOURCE_CHANGED_PLAYBACK;
            if epoch_changed {
                changed_fields |= messages::SOURCE_CHANGED_EPOCH;
            }
            if eos_milestone_changed || playback_started {
                changed_fields |= messages::SOURCE_CHANGED_MILESTONES;
            }
            advance_source_revision(source, changed_fields)?;
        }
        self.0.playback_changed.notify_all();
        Ok(())
    }

    pub fn eos_epoch(&self, key: SourceKey) -> Option<u32> {
        self.lock().sources.get(&key).and_then(|source| source.eos_epoch)
    }

    pub fn source_epoch(&self, key: SourceKey) -> Option<u32> {
        self.lock().sources.get(&key).map(|source| source.last_epoch)
    }

    pub fn linked_audio_sources(&self, video: SourceKey) -> Vec<SourceKey> {
        self.lock()
            .sources
            .iter()
            .filter_map(|(&key, source)| match &source.config {
                SourceConfig::Audio(config)
                    if key.0 == video.0 && config.linked_video_source_id == Some(video.1) =>
                {
                    Some(key)
                },
                _ => None,
            })
            .collect()
    }

    pub fn presentation_due(&self, key: SourceKey, pts_us: i64) -> Option<bool> {
        let state = self.lock();
        let source = state.sources.get(&key)?;
        let Some(first_pts) = source.first_pts_us else {
            return Some(false);
        };
        if pts_us < first_pts {
            return Some(true);
        }
        let Some(started) = source.play_started else {
            return Some(false);
        };
        let relative_us = pts_us.saturating_sub(first_pts).max(0) as u64;
        let target = Duration::from_micros(relative_us);
        Some(started.elapsed().saturating_add(source.played_before_pause) >= target)
    }

    pub fn publish_frame(
        &self,
        key: SourceKey,
        epoch: u32,
        frame: Frame,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let old_pixels = state
            .sources
            .get(&key)
            .and_then(|source| source.latest_frame.as_ref())
            .map_or(0, |frame| u64::from(frame.width) * u64::from(frame.height));
        let new_pixels = u64::from(frame.width) * u64::from(frame.height);
        let decoded_pixels =
            state.decoded_pixels.saturating_sub(old_pixels).saturating_add(new_pixels);
        if decoded_pixels.saturating_add(state.queued_pixels) > MAX_DECODED_PIXELS {
            return Err("decoded pixel quota exceeded");
        }
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        if epoch < source.last_epoch {
            return Err("stale source epoch");
        }
        let epoch_changed = source.last_epoch != epoch;
        source.last_epoch = epoch;
        source.last_decoded_pts_us = frame.pts_us;
        let milestone_changed = set_milestone(source, messages::MILESTONE_FIRST_DECODED_OUTPUT);
        source.latest_frame = Some(frame);
        if epoch_changed || milestone_changed {
            let changed_fields = changed_field(epoch_changed, messages::SOURCE_CHANGED_EPOCH)
                | changed_field(milestone_changed, messages::SOURCE_CHANGED_MILESTONES);
            advance_source_revision(source, changed_fields)?;
        }
        state.decoded_pixels = decoded_pixels;
        state.revision = state.revision.wrapping_add(1);
        Ok(())
    }

    /// Reserve aggregate decoded-frame memory before retaining a queued presentation frame.
    pub fn reserve_queued_pixels(&self, pixels: u64) -> bool {
        let mut state = self.lock();
        let Some(total) = state
            .decoded_pixels
            .checked_add(state.queued_pixels)
            .and_then(|total| total.checked_add(pixels))
        else {
            return false;
        };
        if total > MAX_DECODED_PIXELS {
            return false;
        }
        state.queued_pixels += pixels;
        true
    }

    pub fn release_queued_pixels(&self, pixels: u64) {
        let mut state = self.lock();
        state.queued_pixels = state.queued_pixels.saturating_sub(pixels);
    }

    /// Atomically transfer a queued frame's pixel reservation into the source's latest frame.
    pub fn publish_queued_frame(
        &self,
        key: SourceKey,
        epoch: u32,
        frame: Frame,
        queued_pixels: u64,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        if state.queued_pixels < queued_pixels {
            return Err("queued pixel accounting underflow");
        }
        state.queued_pixels -= queued_pixels;
        let old_pixels = state
            .sources
            .get(&key)
            .and_then(|source| source.latest_frame.as_ref())
            .map_or(0, |frame| u64::from(frame.width) * u64::from(frame.height));
        let new_pixels = u64::from(frame.width) * u64::from(frame.height);
        if new_pixels != queued_pixels {
            return Err("queued frame pixel accounting mismatch");
        }
        let decoded_pixels =
            state.decoded_pixels.saturating_sub(old_pixels).saturating_add(new_pixels);
        if decoded_pixels.saturating_add(state.queued_pixels) > MAX_DECODED_PIXELS {
            return Err("decoded pixel quota exceeded");
        }
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        if epoch < source.last_epoch {
            return Err("stale source epoch");
        }
        let epoch_changed = source.last_epoch != epoch;
        source.last_epoch = epoch;
        source.last_decoded_pts_us = frame.pts_us;
        let milestone_changed = set_milestone(source, messages::MILESTONE_FIRST_DECODED_OUTPUT);
        source.latest_frame = Some(frame);
        if epoch_changed || milestone_changed {
            let changed_fields = changed_field(epoch_changed, messages::SOURCE_CHANGED_EPOCH)
                | changed_field(milestone_changed, messages::SOURCE_CHANGED_MILESTONES);
            advance_source_revision(source, changed_fields)?;
        }
        state.decoded_pixels = decoded_pixels;
        state.revision = state.revision.wrapping_add(1);
        Ok(())
    }

    pub fn commit_mutations(
        &self,
        session_id: SessionId,
        mutations: Vec<SceneMutation>,
    ) -> Result<SceneRevision, &'static str> {
        let mut state = self.lock();
        let mut nodes = state.nodes.clone();
        for mutation in mutations {
            match mutation {
                SceneMutation::Create(node) => {
                    validate_node(&state, session_id, &node)?;
                    if nodes.insert((session_id, node.node_id), node).is_some() {
                        return Err("node ID already exists");
                    }
                },
                SceneMutation::Update(node) => {
                    validate_node(&state, session_id, &node)?;
                    if !nodes.contains_key(&(session_id, node.node_id)) {
                        return Err("node does not exist");
                    }
                    nodes.insert((session_id, node.node_id), node);
                },
                SceneMutation::Delete { session_id: owner, node_id } => {
                    if owner != session_id {
                        return Err("node belongs to another session");
                    }
                    if nodes.remove(&(session_id, node_id)).is_none() {
                        return Err("node does not exist");
                    }
                },
            }
        }
        validate_scene_structure(&state, &nodes)?;
        let scene_revision = advance_scene_revision(
            &mut state,
            session_id,
            messages::SCENE_CHANGED_PRODUCER_COMMIT,
        )?;
        state.nodes = nodes;
        state.revision = state.revision.wrapping_add(1);
        Ok(scene_revision)
    }

    pub fn remove_source(&self, key: SourceKey) -> Result<(), &'static str> {
        let mut state = self.lock();
        let removes_nodes =
            state.nodes.values().any(|node| (node.session_id, node.source_id) == key);
        if removes_nodes {
            advance_scene_revision(&mut state, key.0, messages::SCENE_CHANGED_POLICY_TEARDOWN)?;
        }
        let source = state.sources.remove(&key).ok_or("source does not exist")?;
        if let Some(frame) = source.latest_frame {
            let pixels = u64::from(frame.width) * u64::from(frame.height);
            state.decoded_pixels = state.decoded_pixels.saturating_sub(pixels);
        }
        state.nodes.retain(|(owner, _), node| *owner != key.0 || node.source_id != key.1);
        state.revision = state.revision.wrapping_add(1);
        self.0.playback_changed.notify_all();
        Ok(())
    }

    pub fn lose_source(
        &self,
        key: SourceKey,
        terminal_loss_code: u64,
    ) -> Result<SourceObservation, &'static str> {
        let mut state = self.lock();
        purge_expired_tombstones(&mut state, Instant::now());
        let removes_nodes =
            state.nodes.values().any(|node| (node.session_id, node.source_id) == key);
        if removes_nodes {
            advance_scene_revision(&mut state, key.0, messages::SCENE_CHANGED_SOURCE_LOSS)?;
        }
        let mut source = state.sources.remove(&key).ok_or("source does not exist")?;
        source.lifecycle = messages::SOURCE_LIFECYCLE_TOMBSTONE;
        source.attachment_state = messages::ATTACHMENT_CLOSED;
        set_milestone(&mut source, messages::MILESTONE_SOURCE_LOST);
        advance_source_revision(
            &mut source,
            messages::SOURCE_CHANGED_LIFECYCLE
                | messages::SOURCE_CHANGED_ATTACHMENT
                | messages::SOURCE_CHANGED_MILESTONES,
        )?;
        if let Some(frame) = source.latest_frame.take() {
            let pixels = u64::from(frame.width) * u64::from(frame.height);
            state.decoded_pixels = state.decoded_pixels.saturating_sub(pixels);
        }
        state.nodes.retain(|(owner, _), node| *owner != key.0 || node.source_id != key.1);
        let observation = observation(&source, Some(terminal_loss_code));
        while state.tombstones.len() >= MAX_SOURCE_TOMBSTONES {
            let Some(oldest) = state.tombstone_order.pop_front() else {
                break;
            };
            state.tombstones.remove(&oldest);
        }
        state.tombstone_order.push_back(key);
        state.tombstones.insert(
            key,
            SourceTombstone { observation, expires_at: Instant::now() + SOURCE_TOMBSTONE_TTL },
        );
        state.revision = state.revision.wrapping_add(1);
        self.0.playback_changed.notify_all();
        Ok(observation)
    }

    pub fn snapshot(&self) -> (u64, Vec<RenderItem>) {
        let state = self.lock();
        let mut items = state
            .nodes
            .values()
            .filter(|node| node.visible)
            .filter_map(|node| {
                let key = (node.session_id, node.source_id);
                let frame = state.sources.get(&key)?.latest_frame.clone()?;
                let (x, y, text_anchored, anchor_offset) = if let Some(anchor_id) = node.anchor_id {
                    let anchor = state.anchors.get(&(node.session_id, anchor_id))?;
                    if anchor.alternate != state.alternate_screen {
                        return None;
                    }
                    let offset = ((anchor.column as i64) << 32, i64::from(anchor.line) << 32);
                    (node.x.checked_add(offset.0)?, node.y.checked_add(offset.1)?, true, offset)
                } else {
                    (node.x, node.y, false, (0, 0))
                };
                let clip = match node.clip {
                    Some(clip) => Some(ClipRect {
                        x: clip.x.checked_add(anchor_offset.0)?,
                        y: clip.y.checked_add(anchor_offset.1)?,
                        ..clip
                    }),
                    None => None,
                };
                Some(RenderItem {
                    source_key: key,
                    node_id: node.node_id,
                    frame,
                    x,
                    y,
                    width: node.width,
                    height: node.height,
                    text_layer: node.text_layer,
                    z_index: node.z_index,
                    text_anchored,
                    clip,
                })
            })
            .collect::<Vec<_>>();
        items.sort_by_key(|item| (item.text_layer, item.z_index, item.node_id));
        (state.revision, items)
    }

    pub fn aggregate_visibility(
        &self,
        columns: u32,
        rows: u32,
        display_offset: usize,
        renderable: bool,
    ) -> Result<Vec<(SourceKey, bool, u64)>, &'static str> {
        let mut state = self.lock();
        let right = i64::from(columns) << 32;
        let bottom = i64::from(rows) << 32;
        let states = state
            .sources
            .keys()
            .copied()
            .map(|key| {
                let intersects = state
                    .nodes
                    .values()
                    .filter(|node| node.visible && (node.session_id, node.source_id) == key)
                    .any(|node| {
                        let (x, y) = if let Some(anchor_id) = node.anchor_id {
                            let Some(anchor) = state.anchors.get(&(node.session_id, anchor_id))
                            else {
                                return false;
                            };
                            if anchor.alternate != state.alternate_screen {
                                return false;
                            }
                            (
                                node.x.saturating_add((anchor.column as i64) << 32),
                                node.y.saturating_add(
                                    (i64::from(anchor.line) + display_offset as i64) << 32,
                                ),
                            )
                        } else {
                            (node.x, node.y)
                        };
                        let mut left = x;
                        let mut top = y;
                        let mut node_right = x.saturating_add(node.width);
                        let mut node_bottom = y.saturating_add(node.height);
                        if let Some(clip) = node.clip {
                            let (clip_x, clip_y) = if node.anchor_id.is_some() {
                                (
                                    clip.x.saturating_add(x.saturating_sub(node.x)),
                                    clip.y.saturating_add(y.saturating_sub(node.y)),
                                )
                            } else {
                                (clip.x, clip.y)
                            };
                            left = left.max(clip_x);
                            top = top.max(clip_y);
                            node_right = node_right.min(clip_x.saturating_add(clip.width));
                            node_bottom = node_bottom.min(clip_y.saturating_add(clip.height));
                        }
                        left < right
                            && top < bottom
                            && node_right > 0
                            && node_bottom > 0
                            && left < node_right
                            && top < node_bottom
                    });
                let visible = renderable && intersects;
                let reasons = u64::from(!intersects) | (u64::from(!renderable) << 1);
                (key, visible, reasons)
            })
            .collect::<Vec<_>>();
        for (key, visible, _) in &states {
            let source = state.sources.get_mut(key).expect("source key was just enumerated");
            if source.visible != *visible {
                source.visible = *visible;
                advance_source_revision(source, messages::SOURCE_CHANGED_VISIBILITY)?;
            }
        }
        Ok(states)
    }

    /// Move text anchors with terminal scrolling and discard anchors whose text position is erased
    /// or evicted from scrollback. Positive `lines` move terminal content upward.
    pub fn scroll_anchors(
        &self,
        origin: i32,
        end: i32,
        lines: i32,
        history_size: usize,
    ) -> Result<Vec<AnchorKey>, &'static str> {
        if lines == 0 || origin >= end {
            return Ok(Vec::new());
        }
        let minimum_line = -(history_size.min(i32::MAX as usize) as i32);
        let mut state = self.lock();
        let mut removed = Vec::new();

        for (&key, anchor) in &mut state.anchors {
            let old_line = anchor.line;
            let next_line = if lines > 0 {
                if origin == 0 && old_line < end {
                    Some(old_line.saturating_sub(lines))
                } else if (origin..end).contains(&old_line) {
                    let line = old_line.saturating_sub(lines);
                    (line >= origin).then_some(line)
                } else {
                    Some(old_line)
                }
            } else if (origin..end).contains(&old_line) {
                let line = old_line.saturating_add(lines.saturating_abs());
                (line < end).then_some(line)
            } else {
                Some(old_line)
            };

            match next_line {
                Some(line) if line >= minimum_line => anchor.line = line,
                _ => removed.push(key),
            }
        }

        if !removed.is_empty() {
            let removed_set = removed.iter().copied().collect::<HashSet<_>>();
            let removes_nodes = state.nodes.iter().any(|((session_id, _), node)| {
                node.anchor_id
                    .is_some_and(|anchor_id| removed_set.contains(&(*session_id, anchor_id)))
            });
            if removes_nodes {
                let changed_sessions = state
                    .nodes
                    .iter()
                    .filter_map(|(&(session_id, _), node)| {
                        node.anchor_id
                            .is_some_and(|anchor_id| removed_set.contains(&(session_id, anchor_id)))
                            .then_some(session_id)
                    })
                    .collect::<Vec<_>>();
                advance_changed_scenes(
                    &mut state,
                    changed_sessions,
                    messages::SCENE_CHANGED_ANCHOR_GONE,
                )?;
            }
            state.anchors.retain(|key, _| !removed_set.contains(key));
            retain_gone_anchors(&mut state, &removed);
            state.nodes.retain(|(session_id, _), node| {
                node.anchor_id
                    .is_none_or(|anchor_id| !removed_set.contains(&(*session_id, anchor_id)))
            });
            gc_detached_sources(&mut state);
        }
        state.revision = state.revision.wrapping_add(1);
        Ok(removed)
    }

    /// Clear all placements associated with the terminal text plane.
    pub fn clear_terminal(&self) -> Result<Vec<AnchorKey>, &'static str> {
        let mut state = self.lock();
        let removed = state.anchors.keys().copied().collect::<Vec<_>>();
        if !state.nodes.is_empty() {
            let changed_sessions =
                state.nodes.keys().map(|(session_id, _)| *session_id).collect::<Vec<_>>();
            advance_changed_scenes(
                &mut state,
                changed_sessions,
                messages::SCENE_CHANGED_ANCHOR_GONE,
            )?;
        }
        state.anchors.clear();
        retain_gone_anchors(&mut state, &removed);

        // A ConPTY producer cannot wait for its terminal marker acknowledgement: doing so can
        // stop ConPTY from flushing the marker. Its later control-channel node commit can
        // therefore overtake the earlier terminal clear and marker in the UI event queue. Nodes
        // whose anchors are still pending belong to terminal output after this clear, so keep
        // them hidden until the matching marker arrives. Nodes attached to anchors already seen
        // by the terminal, and viewport-fixed nodes, retain the normal clear semantics.
        #[cfg(windows)]
        {
            let removed = removed.iter().copied().collect::<HashSet<_>>();
            state.nodes.retain(|(session_id, _), node| {
                node.anchor_id.is_some_and(|anchor_id| !removed.contains(&(*session_id, anchor_id)))
            });
        }
        #[cfg(not(windows))]
        state.nodes.clear();
        gc_detached_sources(&mut state);
        state.revision = state.revision.wrapping_add(1);
        Ok(removed)
    }

    /// Keep anchored posters after a producer exits; they are reclaimed when their anchor is
    /// cleared or evicted. Viewport-fixed nodes retain their original connection lifetime.
    pub fn detach_session(&self, session_id: SessionId) -> Result<(), &'static str> {
        let mut state = self.lock();
        state.detached_sessions.insert(session_id);
        if state
            .nodes
            .iter()
            .any(|((owner, _), node)| *owner == session_id && node.anchor_id.is_none())
        {
            advance_scene_revision(
                &mut state,
                session_id,
                messages::SCENE_CHANGED_CONTEXT_REVOKED,
            )?;
        }
        state.nodes.retain(|(owner, _), node| *owner != session_id || node.anchor_id.is_some());
        gc_detached_sources(&mut state);
        state.revision = state.revision.wrapping_add(1);
        self.0.playback_changed.notify_all();
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn timed_playback_snapshot(source: &Source, now: Instant) -> Option<PlaybackSnapshot> {
    if !matches!(source.config, SourceConfig::Video(_) | SourceConfig::Audio(_)) {
        return None;
    }
    let state = match source.lifecycle {
        messages::SOURCE_LIFECYCLE_PAUSED => messages::PLAYBACK_PAUSED,
        messages::SOURCE_LIFECYCLE_ENDED => messages::PLAYBACK_ENDED,
        messages::SOURCE_LIFECYCLE_LOST | messages::SOURCE_LIFECYCLE_TOMBSTONE => {
            messages::PLAYBACK_LOST
        },
        _ if source.play_request.is_some() && source.play_started.is_none() => {
            messages::PLAYBACK_BUFFERING
        },
        _ if source.play_started.is_some() => messages::PLAYBACK_PLAYING,
        _ => messages::PLAYBACK_IDLE,
    };
    let elapsed = source
        .play_started
        .map(|started| now.saturating_duration_since(started))
        .unwrap_or_default()
        .saturating_add(source.played_before_pause);
    let clock_pts_us = source
        .first_pts_us
        .unwrap_or(0)
        .saturating_add(i64::try_from(elapsed.as_micros()).unwrap_or(i64::MAX));
    let buffered_ahead_us = source
        .buffered_until_pts_us
        .map(|end| end.saturating_sub(clock_pts_us).max(0) as u64)
        .unwrap_or(0);
    let eos_state = if source.milestones & messages::MILESTONE_PLAYBACK_ENDED != 0 {
        messages::EOS_APPLIED
    } else if source.eos_epoch.is_some() {
        messages::EOS_ACCEPTED
    } else {
        messages::EOS_NOT_RECEIVED
    };
    Some(PlaybackSnapshot {
        state,
        clock_pts_us,
        epoch: source.last_epoch,
        buffered_ahead_us,
        underrun_count: 0,
        late_drop_count: 0,
        eos_state,
    })
}

fn tombstone_playback_snapshot(observed: SourceObservation) -> Option<PlaybackSnapshot> {
    matches!(observed.kind, messages::SOURCE_KIND_VIDEO | messages::SOURCE_KIND_AUDIO).then_some(
        PlaybackSnapshot {
            state: messages::PLAYBACK_LOST,
            clock_pts_us: observed.last_presented_pts_us,
            epoch: observed.epoch,
            buffered_ahead_us: 0,
            underrun_count: 0,
            late_drop_count: 0,
            eos_state: if observed.milestones & messages::MILESTONE_PLAYBACK_ENDED != 0 {
                messages::EOS_APPLIED
            } else if observed.milestones & messages::MILESTONE_EOS_ACCEPTED != 0 {
                messages::EOS_ACCEPTED
            } else {
                messages::EOS_NOT_RECEIVED
            },
        },
    )
}

fn source_status_from_observation(
    source_id: u64,
    observed: SourceObservation,
    outstanding_byte_credit: u64,
    outstanding_packet_credit: u64,
    playback: Option<PlaybackSnapshot>,
) -> SourceStatus {
    SourceStatus {
        source_id,
        source_revision: observed.revision,
        kind: observed.kind,
        lifecycle: observed.lifecycle,
        epoch: observed.epoch,
        attachment_state: observed.attachment_state,
        attachment_generation: observed.attachment_generation,
        last_media_id: observed.last_media_id,
        last_media_sequence: observed.last_media_sequence,
        last_decoded_pts_us: observed.last_decoded_pts_us,
        last_presented_pts_us: observed.last_presented_pts_us,
        last_presentation_id: observed.last_presentation_id,
        visible: observed.visible,
        capture_policy: 0,
        linked_source_id: observed.linked_source_id,
        milestones: observed.milestones,
        outstanding_byte_credit,
        outstanding_packet_credit,
        ingress_queue_depth: messages::QUEUE_DEPTH_EMPTY,
        descriptor: None,
        playback,
        terminal_loss_code: observed.terminal_loss_code,
    }
}

fn maybe_start_buffered(source: &mut Source) -> bool {
    let Some(request) = source.play_request else {
        return false;
    };
    if source.play_started.is_some() {
        return false;
    }
    let buffered = source
        .buffered_until_pts_us
        .map(|end| end.saturating_sub(request.start_pts_us).max(0) as u64)
        .unwrap_or(0);
    if buffered >= request.minimum_buffer_us || source.eos_epoch.is_some() {
        source.play_started = Some(Instant::now());
        return true;
    }
    false
}

fn validate_node(
    state: &State,
    session_id: SessionId,
    node: &SceneNode,
) -> Result<(), &'static str> {
    if node.session_id != session_id {
        return Err("node belongs to another session");
    }
    if !state.sources.contains_key(&(session_id, node.source_id)) {
        return Err("node source does not exist");
    }
    if let Some(anchor_id) = node.anchor_id
        && !state.anchors.contains_key(&(session_id, anchor_id))
    {
        // ConPTY can hold the marker acknowledgement until the producer emits more output. Permit
        // an authenticated Windows producer to commit the node first; snapshots keep it hidden
        // until the matching marker reaches the terminal text model.
        #[cfg(not(windows))]
        return Err("node anchor does not exist");
    }
    Ok(())
}

fn validate_scene_structure(
    state: &State,
    nodes: &HashMap<(SessionId, u64), SceneNode>,
) -> Result<(), &'static str> {
    let sources = state
        .sources
        .iter()
        .map(|(&(session_id, source_id), source)| SceneValidationSource {
            key: SceneValidationKey { owner_id: session_id, object_id: source_id },
            is_video: matches!(source.config, SourceConfig::Video(_)),
            linked_video: match &source.config {
                SourceConfig::Audio(config) => config.linked_video_source_id.map(|source_id| {
                    SceneValidationKey { owner_id: session_id, object_id: source_id }
                }),
                _ => None,
            },
        })
        .collect::<Vec<_>>();
    let nodes = nodes
        .values()
        .map(|node| SceneValidationNode {
            owner_id: node.session_id,
            node_id: node.node_id,
            fragment_id: 0,
            source: SceneValidationKey { owner_id: node.session_id, object_id: node.source_id },
            x: node.x,
            y: node.y,
            width: node.width,
            height: node.height,
            clip: node.clip,
        })
        .collect::<Vec<_>>();
    validate_scene_snapshot(&sources, &nodes).map_err(|_| "scene structure is invalid")
}

fn gc_detached_sources(state: &mut State) {
    let referenced =
        state.nodes.values().map(|node| (node.session_id, node.source_id)).collect::<HashSet<_>>();
    let mut removed_pixels = 0;
    state.sources.retain(|key, source| {
        let keep = !state.detached_sessions.contains(&key.0) || referenced.contains(key);
        if !keep {
            removed_pixels += source
                .latest_frame
                .as_ref()
                .map_or(0, |frame| u64::from(frame.width) * u64::from(frame.height));
        }
        keep
    });
    state.decoded_pixels = state.decoded_pixels.saturating_sub(removed_pixels);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_video(scene: &SharedScene, source_id: u64) {
        scene
            .add_source(
                1,
                source_id,
                SourceConfig::Video(ParsedVideoSourceConfig {
                    source_id,
                    codec: "h264".into(),
                    packetization: "h264-annexb-au-v1".into(),
                    extradata: Vec::new(),
                    width: 16,
                    height: 16,
                    profile: 0,
                    level: 0,
                    bitrate: 0,
                    color_primaries: 1,
                    transfer: 1,
                    matrix: 1,
                    range: 1,
                    sar_num: 1,
                    sar_den: 1,
                    max_access_unit_bytes: 1024,
                    codec_string: None,
                    decoder_config: None,
                }),
            )
            .unwrap();
    }

    fn video_node(session_id: SessionId, source_id: u64, node_id: u64) -> SceneNode {
        SceneNode {
            session_id,
            node_id,
            source_id,
            x: 0,
            y: 0,
            width: 16_i64 << 32,
            height: 16_i64 << 32,
            text_layer: 1,
            z_index: 0,
            visible: true,
            anchor_id: None,
            clip: None,
        }
    }

    #[test]
    fn play_waits_for_exact_requested_buffer_horizon() {
        let scene = SharedScene::default();
        add_video(&scene, 7);
        let request = PlayRequest {
            source_id: 7,
            start_pts_us: 1_000_000,
            minimum_buffer_us: 100_000,
            ..PlayRequest::baseline(7, 100_000)
        };
        scene.start_playback((1, 7), request).unwrap();
        scene.observe_buffered_pts((1, 7), 1_099_999).unwrap();
        assert_eq!(scene.presentation_due((1, 7), 1_000_000), Some(false));
        assert!(scene.is_before_play_start((1, 7), 999_999));

        scene.observe_buffered_pts((1, 7), 1_100_000).unwrap();
        assert_eq!(scene.presentation_due((1, 7), 1_000_000), Some(true));
    }

    #[test]
    fn eos_releases_short_play_preroll() {
        let scene = SharedScene::default();
        add_video(&scene, 8);
        let request = PlayRequest {
            source_id: 8,
            start_pts_us: 50_000,
            minimum_buffer_us: 500_000,
            ..PlayRequest::baseline(8, 500_000)
        };
        scene.start_playback((1, 8), request).unwrap();
        scene.observe_buffered_pts((1, 8), 75_000).unwrap();
        assert_eq!(scene.presentation_due((1, 8), 50_000), Some(false));
        scene.signal_eos((1, 8), 0).unwrap();
        assert_eq!(scene.presentation_due((1, 8), 50_000), Some(true));
    }

    #[test]
    fn source_revision_advances_only_for_enumerated_transitions() {
        let scene = SharedScene::default();
        add_video(&scene, 9);
        let key = (1, 9);
        let revision = |scene: &SharedScene| scene.source_observation(key).unwrap().revision.get();

        assert_eq!(revision(&scene), 1);
        assert_eq!(scene.mark_attached(key).unwrap().attachment_generation, 1);
        assert_eq!(revision(&scene), 2);
        scene.mark_attachment_closed(key).unwrap();
        assert_eq!(revision(&scene), 3);

        scene.mark_media_accepted(key, 1, 10, 1, false).unwrap();
        assert_eq!(revision(&scene), 4);
        scene.mark_media_accepted(key, 1, 11, 2, false).unwrap();
        assert_eq!(revision(&scene), 4, "packet counters are not source transitions");
        scene.mark_media_accepted(key, 1, 12, 3, true).unwrap();
        assert_eq!(revision(&scene), 5);

        scene.mark_decoder_initialized(key).unwrap();
        assert_eq!(revision(&scene), 6);
        scene.mark_decoder_initialized(key).unwrap();
        assert_eq!(revision(&scene), 6, "milestones are sticky");
        scene.mark_decoded_output(key, 1_000).unwrap();
        assert_eq!(revision(&scene), 7);
        scene.mark_decoded_output(key, 2_000).unwrap();
        assert_eq!(revision(&scene), 7, "decoded PTS is not a transition counter");

        scene.commit_mutations(1, vec![SceneMutation::Create(video_node(1, 9, 1))]).unwrap();
        scene.aggregate_visibility(80, 24, 0, true).unwrap();
        assert_eq!(revision(&scene), 8);
        scene.aggregate_visibility(80, 24, 0, true).unwrap();
        assert_eq!(revision(&scene), 8);
        scene.mark_presented(key, 12, 2_000, true).unwrap();
        assert_eq!(revision(&scene), 9);
        scene.mark_presented(key, 12, 2_000, true).unwrap();
        assert_eq!(revision(&scene), 9, "presentation IDs are independent counters");

        let request = PlayRequest {
            source_id: 9,
            start_pts_us: 2_000,
            minimum_buffer_us: 100,
            ..PlayRequest::baseline(9, 100)
        };
        scene.start_playback(key, request).unwrap();
        assert_eq!(revision(&scene), 10);
        scene.observe_buffered_pts(key, 2_100).unwrap();
        assert_eq!(revision(&scene), 11);
        scene.pause_playback(key).unwrap();
        assert_eq!(revision(&scene), 12);
        scene.flush_playback(key, 2).unwrap();
        assert_eq!(revision(&scene), 13);
        scene.mark_policy_changed(key).unwrap();
        assert_eq!(revision(&scene), 14);
        scene.mark_descriptor_changed(key).unwrap();
        assert_eq!(revision(&scene), 15);
        scene.signal_eos(key, 2).unwrap();
        assert_eq!(revision(&scene), 16);
        scene.mark_playback_ended(key).unwrap();
        assert_eq!(revision(&scene), 17);

        let lost = scene.lose_source(key, messages::ERROR_DEVICE_LOST).unwrap();
        assert_eq!(lost.revision.get(), 18);
        assert_eq!(lost.lifecycle, messages::SOURCE_LIFECYCLE_TOMBSTONE);
        assert_eq!(lost.milestones, messages::MILESTONE_MASK);
    }

    #[test]
    fn attachment_generation_is_independent_and_monotonic() {
        let scene = SharedScene::default();
        add_video(&scene, 10);
        let key = (1, 10);

        let first = scene.mark_attached(key).unwrap();
        scene.mark_attachment_closed(key).unwrap();
        let second = scene.mark_attached(key).unwrap();
        assert_eq!(first.attachment_generation, 1);
        assert_eq!(second.attachment_generation, 2);
        assert_eq!(second.attachment_state, messages::ATTACHMENT_ATTACHED);
        assert_eq!(second.last_media_id, 0);
        assert_eq!(second.last_media_sequence, 0);
    }

    #[test]
    fn scene_revisions_are_session_scoped_and_cover_automatic_node_loss() {
        let scene = SharedScene::default();
        add_video(&scene, 1);
        scene
            .add_source(
                2,
                1,
                SourceConfig::Video(ParsedVideoSourceConfig {
                    source_id: 1,
                    codec: "h264".into(),
                    packetization: "h264-annexb-au-v1".into(),
                    extradata: Vec::new(),
                    width: 16,
                    height: 16,
                    profile: 0,
                    level: 0,
                    bitrate: 0,
                    color_primaries: 1,
                    transfer: 1,
                    matrix: 1,
                    range: 1,
                    sar_num: 1,
                    sar_den: 1,
                    max_access_unit_bytes: 1024,
                    codec_string: None,
                    decoder_config: None,
                }),
            )
            .unwrap();

        assert_eq!(
            scene.commit_mutations(1, vec![SceneMutation::Create(video_node(1, 1, 1))]).unwrap(),
            SceneRevision::new(1)
        );
        assert_eq!(
            scene.commit_mutations(2, vec![SceneMutation::Create(video_node(2, 1, 1))]).unwrap(),
            SceneRevision::new(1)
        );
        assert_eq!(scene.scene_revision(1), SceneRevision::new(1));
        assert_eq!(scene.scene_revision(2), SceneRevision::new(1));

        scene.lose_source((1, 1), messages::ERROR_DEVICE_LOST).unwrap();
        assert_eq!(scene.scene_revision(1), SceneRevision::new(2));
        assert_eq!(scene.scene_revision(2), SceneRevision::new(1));

        assert!(
            scene
                .commit_mutations(2, vec![SceneMutation::Delete { session_id: 2, node_id: 99 }],)
                .is_err()
        );
        assert_eq!(scene.scene_revision(2), SceneRevision::new(1));
        assert_eq!(scene.commit_mutations(2, Vec::new()).unwrap(), SceneRevision::new(2));
    }

    #[test]
    fn tombstones_are_metadata_only_bounded_and_expiring() {
        let scene = SharedScene::default();
        for source_id in 1..=(MAX_SOURCE_TOMBSTONES as u64 + 1) {
            add_video(&scene, source_id);
            if source_id == 1 {
                scene
                    .commit_mutations(
                        1,
                        vec![SceneMutation::Create(video_node(1, source_id, source_id))],
                    )
                    .unwrap();
                scene
                    .publish_frame(
                        (1, source_id),
                        1,
                        Frame {
                            frame_id: 1,
                            pts_us: 0,
                            width: 16,
                            height: 16,
                            rgba: Arc::from(vec![0; 16 * 16 * 4]),
                            alpha_mode: messages::ALPHA_STRAIGHT,
                            sar_num: 1,
                            sar_den: 1,
                        },
                    )
                    .unwrap();
            }
            scene.lose_source((1, source_id), messages::ERROR_DEVICE_LOST).unwrap();
        }

        {
            let state = scene.lock();
            assert!(state.sources.is_empty());
            assert!(state.nodes.is_empty());
            assert_eq!(state.decoded_pixels, 0);
            assert_eq!(state.queued_pixels, 0);
            assert_eq!(state.tombstones.len(), MAX_SOURCE_TOMBSTONES);
            assert_eq!(state.tombstone_order.len(), MAX_SOURCE_TOMBSTONES);
            assert!(!state.tombstones.contains_key(&(1, 1)), "oldest tombstone was evicted");
        }
        assert!(scene.source_observation((1, 2)).is_some());
        assert!(
            scene
                .add_source(
                    1,
                    2,
                    SourceConfig::Raster(RasterSourceConfig {
                        source_id: 2,
                        width: 1,
                        height: 1,
                        alpha_mode: messages::ALPHA_STRAIGHT,
                        compression_mode: messages::COMPRESSION_NONE,
                    }),
                )
                .is_err()
        );

        {
            let mut state = scene.lock();
            let expired = Instant::now() - Duration::from_secs(1);
            for tombstone in state.tombstones.values_mut() {
                tombstone.expires_at = expired;
            }
        }
        assert!(scene.source_observation((1, 2)).is_none());
        assert!(scene.lock().tombstones.is_empty());
        scene
            .add_source(
                1,
                2,
                SourceConfig::Raster(RasterSourceConfig {
                    source_id: 2,
                    width: 1,
                    height: 1,
                    alpha_mode: messages::ALPHA_STRAIGHT,
                    compression_mode: messages::COMPRESSION_NONE,
                }),
            )
            .unwrap();
    }

    #[test]
    fn status_queries_are_bounded_revision_consistent_and_secret_free() {
        let scene = SharedScene::default();
        add_video(&scene, 40);
        add_video(&scene, 41);
        scene
            .commit_mutations(
                1,
                vec![
                    SceneMutation::Create(video_node(1, 40, 1)),
                    SceneMutation::Create(video_node(1, 41, 2)),
                ],
            )
            .unwrap();
        let first = scene
            .scene_status(
                1,
                &SceneQuery {
                    expected_revision: Some(SceneRevision::new(1)),
                    cursor: None,
                    maximum_nodes: Some(1),
                },
            )
            .unwrap();
        assert_eq!(first.nodes.len(), 1);
        assert_eq!(first.total_nodes, 2);
        let cursor = first.cursor.unwrap();
        let second = scene
            .scene_status(
                1,
                &SceneQuery {
                    expected_revision: Some(SceneRevision::new(1)),
                    cursor: Some(cursor),
                    maximum_nodes: Some(1),
                },
            )
            .unwrap();
        assert_eq!(second.nodes[0].node.node_id, 2);
        assert!(second.cursor.is_none());

        scene.commit_mutations(1, Vec::new()).unwrap();
        let stale = scene
            .scene_status(
                1,
                &SceneQuery {
                    expected_revision: None,
                    cursor: Some(cursor),
                    maximum_nodes: Some(1),
                },
            )
            .unwrap_err();
        assert_eq!(stale.current_revision, SceneRevision::new(2));

        let status = scene.source_status((1, 40), 4096, 4).unwrap();
        let encoded = messages::source_status(7, &status).unwrap();
        assert!(encoded.len() <= messages::MAX_STATUS_REPLY_BODY);
        for forbidden in [b"VIVID_TOKEN".as_slice(), b"media_ticket", b"/tmp/"] {
            assert!(!encoded.windows(forbidden.len()).any(|window| window == forbidden));
        }
    }

    #[test]
    fn maximum_scene_page_fits_the_status_reply_cap() {
        let scene = SharedScene::default();
        add_video(&scene, 40);
        scene
            .commit_mutations(
                1,
                (1..=MAX_SCENE_NODES as u64)
                    .map(|node_id| SceneMutation::Create(video_node(1, 40, node_id)))
                    .collect(),
            )
            .unwrap();

        let status = scene
            .scene_status(
                1,
                &SceneQuery {
                    expected_revision: Some(SceneRevision::new(1)),
                    cursor: None,
                    maximum_nodes: Some(MAX_SCENE_NODES as u64),
                },
            )
            .unwrap();
        assert_eq!(status.nodes.len(), MAX_SCENE_NODES);
        assert!(status.cursor.is_none());
        let encoded = messages::scene_status(7, &status).unwrap();
        assert!(encoded.len() <= messages::MAX_STATUS_REPLY_BODY);
    }

    #[test]
    fn anchor_status_distinguishes_ready_gone_and_unknown() {
        let scene = SharedScene::default();
        scene.add_anchor(1, 7, 3, 4).unwrap();
        let ready = scene.anchor_status(1, 7, 80, 24, 0, 9);
        assert_eq!(ready.state, messages::ANCHOR_STATE_READY);
        assert!(ready.visible);
        scene.apply_anchor_resize([((1, 7), None)]).unwrap();
        assert_eq!(scene.anchor_status(1, 7, 80, 24, 0, 9).state, messages::ANCHOR_STATE_GONE);
        assert_eq!(scene.anchor_status(1, 8, 80, 24, 0, 9).state, messages::ANCHOR_STATE_UNKNOWN);
    }

    #[test]
    fn every_source_wait_condition_uses_authoritative_state() {
        let scene = SharedScene::default();
        add_video(&scene, 50);
        let video = (1, 50);
        let expect_satisfied = |evaluation| {
            assert!(matches!(evaluation, SourceWaitEvaluation::Satisfied(_)));
        };

        assert_eq!(
            scene.evaluate_wait(video, messages::WAIT_FIRST_VISIBLE_PRESENTATION, None),
            SourceWaitEvaluation::NotVisible
        );
        scene.mark_attached(video).unwrap();
        expect_satisfied(scene.evaluate_wait(video, messages::WAIT_MEDIA_ATTACHED, None));
        scene.mark_attachment_closed(video).unwrap();
        expect_satisfied(scene.evaluate_wait(video, messages::WAIT_MEDIA_CLOSED, None));
        let revision = scene.source_observation(video).unwrap().revision.get();
        expect_satisfied(scene.evaluate_wait(
            video,
            messages::WAIT_SOURCE_REVISION,
            Some(revision - 1),
        ));

        scene.commit_mutations(1, vec![SceneMutation::Create(video_node(1, 50, 50))]).unwrap();
        scene.aggregate_visibility(80, 24, 0, true).unwrap();
        scene.mark_presented(video, 10, 9_000, true).unwrap();
        expect_satisfied(scene.evaluate_wait(
            video,
            messages::WAIT_FIRST_VISIBLE_PRESENTATION,
            None,
        ));
        expect_satisfied(scene.evaluate_wait(video, messages::WAIT_VIDEO_PTS, Some(8_000)));

        scene
            .start_playback(
                video,
                PlayRequest {
                    source_id: 50,
                    start_pts_us: 0,
                    minimum_buffer_us: 100,
                    ..PlayRequest::baseline(50, 100)
                },
            )
            .unwrap();
        scene.observe_buffered_pts(video, 100).unwrap();
        expect_satisfied(scene.evaluate_wait(video, messages::WAIT_PLAYBACK_STARTED, None));
        scene.signal_eos(video, 0).unwrap();
        scene.mark_playback_ended(video).unwrap();
        expect_satisfied(scene.evaluate_wait(video, messages::WAIT_PLAYBACK_ENDED, None));
        scene.lose_source(video, messages::ERROR_DEVICE_LOST).unwrap();
        expect_satisfied(scene.evaluate_wait(video, messages::WAIT_SOURCE_LOST, None));

        scene
            .add_source(
                1,
                51,
                SourceConfig::Raster(RasterSourceConfig {
                    source_id: 51,
                    width: 1,
                    height: 1,
                    alpha_mode: messages::ALPHA_STRAIGHT,
                    compression_mode: messages::COMPRESSION_NONE,
                }),
            )
            .unwrap();
        let raster = (1, 51);
        scene.commit_mutations(1, vec![SceneMutation::Create(video_node(1, 51, 51))]).unwrap();
        scene.aggregate_visibility(80, 24, 0, true).unwrap();
        scene.mark_presented(raster, 55, 0, true).unwrap();
        expect_satisfied(scene.evaluate_wait(raster, messages::WAIT_RASTER_FRAME, Some(55)));
    }

    #[test]
    fn transaction_rejects_missing_source_atomically() {
        let scene = SharedScene::default();
        let node = SceneNode {
            session_id: 1,
            node_id: 1,
            source_id: 9,
            x: 0,
            y: 0,
            width: 1 << 32,
            height: 1 << 32,
            text_layer: 1,
            z_index: 0,
            visible: true,
            anchor_id: None,
            clip: None,
        };
        assert!(scene.commit_mutations(1, vec![SceneMutation::Create(node)]).is_err());
        assert!(scene.snapshot().1.is_empty());
    }

    #[test]
    fn transaction_updates_and_deletes_nodes_atomically() {
        let scene = SharedScene::default();
        scene
            .add_source(
                1,
                1,
                SourceConfig::Raster(RasterSourceConfig {
                    source_id: 1,
                    width: 2,
                    height: 2,
                    alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                    compression_mode: vivid_protocol::messages::COMPRESSION_NONE,
                }),
            )
            .unwrap();
        let node = SceneNode {
            session_id: 1,
            node_id: 2,
            source_id: 1,
            x: 0,
            y: 0,
            width: 2_i64 << 32,
            height: 2_i64 << 32,
            text_layer: 1,
            z_index: 0,
            visible: true,
            anchor_id: None,
            clip: None,
        };
        scene.commit_mutations(1, vec![SceneMutation::Create(node.clone())]).unwrap();

        let mut updated = node;
        updated.z_index = 7;
        scene.commit_mutations(1, vec![SceneMutation::Update(updated)]).unwrap();
        scene
            .commit_mutations(1, vec![SceneMutation::Delete { session_id: 1, node_id: 2 }])
            .unwrap();
        assert!(
            scene
                .commit_mutations(1, vec![SceneMutation::Delete { session_id: 1, node_id: 2 }])
                .is_err()
        );
        scene.remove_source((1, 1)).unwrap();
        assert!(scene.source_config((1, 1)).is_none());
    }

    #[test]
    fn anchored_poster_scrolls_and_clear_reclaims_detached_source() {
        let scene = SharedScene::default();
        scene
            .add_source(
                4,
                1,
                SourceConfig::Raster(RasterSourceConfig {
                    source_id: 1,
                    width: 1,
                    height: 1,
                    alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                    compression_mode: vivid_protocol::messages::COMPRESSION_NONE,
                }),
            )
            .unwrap();
        scene.add_anchor(4, 7, 2, 5).unwrap();
        scene
            .commit_mutations(
                4,
                vec![SceneMutation::Create(SceneNode {
                    session_id: 4,
                    node_id: 2,
                    source_id: 1,
                    x: 0,
                    y: 0,
                    width: 3_i64 << 32,
                    height: 2_i64 << 32,
                    text_layer: 1,
                    z_index: 0,
                    visible: true,
                    anchor_id: Some(7),
                    clip: None,
                })],
            )
            .unwrap();
        scene
            .publish_frame(
                (4, 1),
                1,
                Frame {
                    frame_id: 1,
                    pts_us: 0,
                    width: 1,
                    height: 1,
                    rgba: Arc::from([255, 0, 0, 255]),
                    alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                    sar_num: 1,
                    sar_den: 1,
                },
            )
            .unwrap();

        let item = scene.snapshot().1.pop().unwrap();
        assert_eq!((item.x, item.y), (2_i64 << 32, 5_i64 << 32));
        assert!(item.text_anchored);

        assert!(scene.scroll_anchors(0, 24, 3, 3).unwrap().is_empty());
        assert_eq!(scene.snapshot().1[0].y, 2_i64 << 32);

        scene.detach_session(4).unwrap();
        assert!(scene.source_config((4, 1)).is_some());
        assert_eq!(scene.clear_terminal().unwrap(), vec![(4, 7)]);
        assert!(scene.snapshot().1.is_empty());
        assert!(scene.source_config((4, 1)).is_none());
    }

    #[test]
    fn anchor_resize_updates_positions_and_removes_evicted_anchors() {
        let scene = SharedScene::default();
        scene.add_anchor(4, 7, 2, 5).unwrap();
        scene.add_anchor(4, 8, 3, 6).unwrap();

        let removed =
            scene.apply_anchor_resize([((4, 7), Some((9, -2, false))), ((4, 8), None)]).unwrap();
        assert_eq!(removed, vec![(4, 8)]);
        assert_eq!(scene.anchor_positions(), vec![((4, 7), 9, -2, false)]);
    }

    #[cfg(windows)]
    #[test]
    fn pending_anchor_places_poster_and_follows_terminal_scroll() {
        let scene = SharedScene::default();
        scene
            .add_source(
                5,
                1,
                SourceConfig::Raster(RasterSourceConfig {
                    source_id: 1,
                    width: 1,
                    height: 1,
                    alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                    compression_mode: vivid_protocol::messages::COMPRESSION_NONE,
                }),
            )
            .unwrap();
        scene
            .commit_mutations(
                5,
                vec![SceneMutation::Create(SceneNode {
                    session_id: 5,
                    node_id: 2,
                    source_id: 1,
                    x: 0,
                    y: 0,
                    width: 1_i64 << 32,
                    height: 1_i64 << 32,
                    text_layer: 1,
                    z_index: 0,
                    visible: true,
                    anchor_id: Some(9),
                    clip: None,
                })],
            )
            .unwrap();
        scene
            .publish_frame(
                (5, 1),
                1,
                Frame {
                    frame_id: 1,
                    pts_us: 0,
                    width: 1,
                    height: 1,
                    rgba: Arc::from([255, 0, 0, 255]),
                    alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                    sar_num: 1,
                    sar_den: 1,
                },
            )
            .unwrap();

        // A node committed before its ConPTY marker is hidden rather than placed at a guessed
        // viewport coordinate. The control commit can also overtake the full-screen clear which
        // preceded the marker; that clear must not discard this logically newer pending node.
        assert!(scene.snapshot().1.is_empty());
        assert!(scene.clear_terminal().unwrap().is_empty());
        scene.add_anchor(5, 9, 3, 6).unwrap();
        let item = scene.snapshot().1.pop().unwrap();
        assert_eq!((item.x, item.y), (3_i64 << 32, 6_i64 << 32));
        assert!(item.text_anchored);

        assert!(scene.scroll_anchors(0, 24, 2, 0).unwrap().is_empty());
        assert_eq!(scene.snapshot().1[0].y, 4_i64 << 32);

        scene.detach_session(5).unwrap();
        assert_eq!(scene.snapshot().1.len(), 1);
        assert_eq!(scene.clear_terminal().unwrap(), vec![(5, 9)]);
        assert!(scene.snapshot().1.is_empty());
        assert!(scene.source_config((5, 1)).is_none());
    }

    #[test]
    fn alternate_screen_hides_primary_anchors_and_discards_alt_anchors() {
        let scene = SharedScene::default();
        scene
            .add_source(
                6,
                1,
                SourceConfig::Raster(RasterSourceConfig {
                    source_id: 1,
                    width: 1,
                    height: 1,
                    alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                    compression_mode: vivid_protocol::messages::COMPRESSION_NONE,
                }),
            )
            .unwrap();
        scene.add_anchor(6, 7, 2, 5).unwrap();
        scene
            .commit_mutations(
                6,
                vec![SceneMutation::Create(SceneNode {
                    session_id: 6,
                    node_id: 2,
                    source_id: 1,
                    x: 0,
                    y: 0,
                    width: 1_i64 << 32,
                    height: 1_i64 << 32,
                    text_layer: 1,
                    z_index: 0,
                    visible: true,
                    anchor_id: Some(7),
                    clip: None,
                })],
            )
            .unwrap();
        scene
            .publish_frame(
                (6, 1),
                1,
                Frame {
                    frame_id: 1,
                    pts_us: 0,
                    width: 1,
                    height: 1,
                    rgba: Arc::from([255, 0, 0, 255]),
                    alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                    sar_num: 1,
                    sar_den: 1,
                },
            )
            .unwrap();
        // ConPTY can deliver the marker event before the UI receives the preceding screen swap.
        // The marker must retain the terminal parser's authoritative alternate-screen identity.
        scene.add_anchor_for_screen(6, 8, 1, 1, true).unwrap();
        scene
            .commit_mutations(
                6,
                vec![SceneMutation::Create(SceneNode {
                    session_id: 6,
                    node_id: 3,
                    source_id: 1,
                    x: 0,
                    y: 0,
                    width: 1_i64 << 32,
                    height: 1_i64 << 32,
                    text_layer: 1,
                    z_index: 0,
                    visible: true,
                    anchor_id: Some(8),
                    clip: None,
                })],
            )
            .unwrap();
        assert_eq!(scene.snapshot().1.len(), 1);

        // A full-screen application takes the alternate screen: the primary-screen node must stop
        // rendering, while the marker which arrived early becomes visible on its actual screen.
        assert!(scene.set_alternate_screen(true).unwrap().is_empty());
        let items = scene.snapshot().1;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].node_id, 3);
        let states = scene.aggregate_visibility(80, 24, 0, true).unwrap();
        assert_eq!(states.len(), 1);
        assert!(states[0].1);

        // Leaving the alternate screen discards its content, so its anchors go with it; the
        // primary-screen image returns unchanged.
        assert_eq!(scene.set_alternate_screen(false).unwrap(), vec![(6, 8)]);
        let items = scene.snapshot().1;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].node_id, 2);
        assert_eq!((items[0].x, items[0].y), (2_i64 << 32, 5_i64 << 32));
        assert!(scene.set_alternate_screen(false).unwrap().is_empty());
    }
}
