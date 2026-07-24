use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use vivid_protocol::media;
use vivid_protocol::messages::{
    ClipRect, ImageSourceConfig, MAX_SCENE_NODES, ParsedAudioSourceConfig, ParsedSceneNode,
    ParsedVideoSourceConfig, PlayRequest, RasterSourceConfig, SceneValidationKey,
    SceneValidationNode, SceneValidationSource, validate_scene_snapshot,
};

pub type SessionId = u64;
pub type SourceKey = (SessionId, u64);
pub type AnchorKey = (SessionId, u64);

const MAX_SOURCES: usize = 64;
const MAX_DECODED_PIXELS: u64 = 8192 * 8192 * 2;
const MAX_RESERVED_INGRESS_BYTES: u64 = 256 * 1024 * 1024;
const RESERVED_INGRESS_WINDOW: u64 = 4 * 1024 * 1024;

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
    latest_frame: Option<Frame>,
    play_started: Option<Instant>,
    played_before_pause: Duration,
    first_pts_us: Option<i64>,
    play_request: Option<PlayRequest>,
    buffered_until_pts_us: Option<i64>,
    last_epoch: u32,
    eos_epoch: Option<u32>,
}

#[derive(Debug, Default)]
struct State {
    sources: HashMap<SourceKey, Source>,
    nodes: HashMap<(SessionId, u64), SceneNode>,
    anchors: HashMap<AnchorKey, TextAnchor>,
    detached_sessions: HashSet<SessionId>,
    decoded_pixels: u64,
    queued_pixels: u64,
    revision: u64,
    alternate_screen: bool,
}

#[derive(Debug, Default)]
struct Inner {
    state: Mutex<State>,
    playback_changed: Condvar,
}

#[derive(Clone, Debug, Default)]
pub struct SharedScene(Arc<Inner>);

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
    state.revision = state.revision.wrapping_add(1);
    Ok(())
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
    ) -> Vec<AnchorKey> {
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
            state.anchors.retain(|key, _| !removed_set.contains(key));
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
        removed
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
    pub fn set_alternate_screen(&self, alternate: bool) -> Vec<AnchorKey> {
        let mut state = self.lock();
        if state.alternate_screen == alternate {
            return Vec::new();
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
                state.anchors.retain(|key, _| !removed_set.contains(key));
                state.nodes.retain(|(session_id, _), node| {
                    node.anchor_id
                        .is_none_or(|anchor_id| !removed_set.contains(&(*session_id, anchor_id)))
                });
                gc_detached_sources(&mut state);
            }
        }
        state.revision = state.revision.wrapping_add(1);
        self.0.playback_changed.notify_all();
        removed
    }

    pub fn add_source(
        &self,
        session_id: SessionId,
        source_id: u64,
        config: SourceConfig,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        let key = (session_id, source_id);
        if state.sources.contains_key(&key) {
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
        maybe_start_buffered(source);
        self.0.playback_changed.notify_all();
        Ok(())
    }

    /// Record decoded/pre-roll progress without blocking the source's ingress worker.
    pub fn observe_buffered_pts(&self, key: SourceKey, pts_us: i64) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        source.buffered_until_pts_us =
            Some(source.buffered_until_pts_us.map_or(pts_us, |current| current.max(pts_us)));
        maybe_start_buffered(source);
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
        self.0.playback_changed.notify_all();
        Ok(())
    }

    pub fn signal_eos(&self, key: SourceKey, epoch: u32) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        if epoch < source.last_epoch {
            return Err("stale source epoch");
        }
        source.eos_epoch = Some(epoch);
        maybe_start_buffered(source);
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
        source.last_epoch = epoch;
        source.latest_frame = Some(frame);
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
        source.last_epoch = epoch;
        source.latest_frame = Some(frame);
        state.decoded_pixels = decoded_pixels;
        state.revision = state.revision.wrapping_add(1);
        Ok(())
    }

    pub fn commit_mutations(
        &self,
        session_id: SessionId,
        mutations: Vec<SceneMutation>,
    ) -> Result<(), &'static str> {
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
        state.nodes = nodes;
        state.revision = state.revision.wrapping_add(1);
        Ok(())
    }

    pub fn remove_source(&self, key: SourceKey) -> Result<(), &'static str> {
        let mut state = self.lock();
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
    ) -> Vec<(SourceKey, bool, u64)> {
        let state = self.lock();
        let right = i64::from(columns) << 32;
        let bottom = i64::from(rows) << 32;
        state
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
            .collect()
    }

    /// Move text anchors with terminal scrolling and discard anchors whose text position is erased
    /// or evicted from scrollback. Positive `lines` move terminal content upward.
    pub fn scroll_anchors(
        &self,
        origin: i32,
        end: i32,
        lines: i32,
        history_size: usize,
    ) -> Vec<AnchorKey> {
        if lines == 0 || origin >= end {
            return Vec::new();
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
            state.anchors.retain(|key, _| !removed_set.contains(key));
            state.nodes.retain(|(session_id, _), node| {
                node.anchor_id
                    .is_none_or(|anchor_id| !removed_set.contains(&(*session_id, anchor_id)))
            });
            gc_detached_sources(&mut state);
        }
        state.revision = state.revision.wrapping_add(1);
        removed
    }

    /// Clear all placements associated with the terminal text plane.
    pub fn clear_terminal(&self) -> Vec<AnchorKey> {
        let mut state = self.lock();
        let removed = state.anchors.keys().copied().collect::<Vec<_>>();
        state.anchors.clear();

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
        removed
    }

    /// Keep anchored posters after a producer exits; they are reclaimed when their anchor is
    /// cleared or evicted. Viewport-fixed nodes retain their original connection lifetime.
    pub fn detach_session(&self, session_id: SessionId) {
        let mut state = self.lock();
        state.detached_sessions.insert(session_id);
        state.nodes.retain(|(owner, _), node| *owner != session_id || node.anchor_id.is_some());
        gc_detached_sources(&mut state);
        state.revision = state.revision.wrapping_add(1);
        self.0.playback_changed.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn maybe_start_buffered(source: &mut Source) {
    let Some(request) = source.play_request else {
        return;
    };
    if source.play_started.is_some() {
        return;
    }
    let buffered = source
        .buffered_until_pts_us
        .map(|end| end.saturating_sub(request.start_pts_us).max(0) as u64)
        .unwrap_or(0);
    if buffered >= request.minimum_buffer_us || source.eos_epoch.is_some() {
        source.play_started = Some(Instant::now());
    }
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

        assert!(scene.scroll_anchors(0, 24, 3, 3).is_empty());
        assert_eq!(scene.snapshot().1[0].y, 2_i64 << 32);

        scene.detach_session(4);
        assert!(scene.source_config((4, 1)).is_some());
        assert_eq!(scene.clear_terminal(), vec![(4, 7)]);
        assert!(scene.snapshot().1.is_empty());
        assert!(scene.source_config((4, 1)).is_none());
    }

    #[test]
    fn anchor_resize_updates_positions_and_removes_evicted_anchors() {
        let scene = SharedScene::default();
        scene.add_anchor(4, 7, 2, 5).unwrap();
        scene.add_anchor(4, 8, 3, 6).unwrap();

        let removed = scene.apply_anchor_resize([((4, 7), Some((9, -2, false))), ((4, 8), None)]);
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
        assert!(scene.clear_terminal().is_empty());
        scene.add_anchor(5, 9, 3, 6).unwrap();
        let item = scene.snapshot().1.pop().unwrap();
        assert_eq!((item.x, item.y), (3_i64 << 32, 6_i64 << 32));
        assert!(item.text_anchored);

        assert!(scene.scroll_anchors(0, 24, 2, 0).is_empty());
        assert_eq!(scene.snapshot().1[0].y, 4_i64 << 32);

        scene.detach_session(5);
        assert_eq!(scene.snapshot().1.len(), 1);
        assert_eq!(scene.clear_terminal(), vec![(5, 9)]);
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
        assert!(scene.set_alternate_screen(true).is_empty());
        let items = scene.snapshot().1;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].node_id, 3);
        let states = scene.aggregate_visibility(80, 24, 0, true);
        assert_eq!(states.len(), 1);
        assert!(states[0].1);

        // Leaving the alternate screen discards its content, so its anchors go with it; the
        // primary-screen image returns unchanged.
        assert_eq!(scene.set_alternate_screen(false), vec![(6, 8)]);
        let items = scene.snapshot().1;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].node_id, 2);
        assert_eq!((items[0].x, items[0].y), (2_i64 << 32, 5_i64 << 32));
        assert!(scene.set_alternate_screen(false).is_empty());
    }
}
