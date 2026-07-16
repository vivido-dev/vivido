use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use vivid_protocol::media;
use vivid_protocol::messages::{
    ImageSourceConfig, ParsedNodeConfig, ParsedVideoSourceConfig, RasterSourceConfig,
};

pub type SessionId = u64;
pub type SourceKey = (SessionId, u64);
pub type AnchorKey = (SessionId, u64);

const MAX_SOURCES: usize = 64;
const MAX_NODES: usize = 256;
const MAX_DECODED_PIXELS: u64 = 8192 * 8192 * 2;
const MAX_RESERVED_INGRESS_BYTES: u64 = 256 * 1024 * 1024;
const RESERVED_INGRESS_WINDOW: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum SourceConfig {
    Raster(RasterSourceConfig),
    Video(ParsedVideoSourceConfig),
    Image(ImageSourceConfig),
}

impl SourceConfig {
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            Self::Raster(config) => (config.width, config.height),
            Self::Video(config) => (config.width, config.height),
            Self::Image(config) => (config.width, config.height),
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextAnchor {
    column: usize,
    line: i32,
}

#[derive(Debug, Clone)]
pub enum SceneMutation {
    Create(SceneNode),
    Update(SceneNode),
    Delete { session_id: SessionId, node_id: u64 },
}

impl SceneNode {
    pub fn from_protocol(session_id: SessionId, config: ParsedNodeConfig) -> Self {
        Self {
            session_id,
            node_id: config.node_id,
            source_id: config.source_id,
            x: config.x,
            y: config.y,
            width: config.width,
            height: config.height,
            text_layer: config.text_layer,
            z_index: config.z_index,
            visible: config.visible,
            anchor_id: config.anchor_id,
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
}

#[derive(Debug)]
struct Source {
    config: SourceConfig,
    latest_frame: Option<Frame>,
    play_started: Option<Instant>,
    played_before_pause: Duration,
    first_pts_us: Option<i64>,
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
    revision: u64,
}

#[derive(Debug, Default)]
struct Inner {
    state: Mutex<State>,
    playback_changed: Condvar,
}

#[derive(Clone, Debug, Default)]
pub struct SharedScene(Arc<Inner>);

impl SharedScene {
    pub fn add_anchor(
        &self,
        session_id: SessionId,
        anchor_id: u64,
        column: usize,
        line: i32,
    ) -> Result<(), &'static str> {
        let mut state = self.lock();
        if anchor_id == 0 {
            return Err("anchor ID is zero");
        }
        if state.anchors.len() >= MAX_NODES {
            return Err("anchor quota exceeded");
        }
        if state.anchors.insert((session_id, anchor_id), TextAnchor { column, line }).is_some() {
            return Err("anchor ID already exists");
        }
        state.revision = state.revision.wrapping_add(1);
        Ok(())
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
        if width == 0 || height == 0 || width > 8192 || height > 8192 {
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
                last_epoch: 0,
                eos_epoch: None,
            },
        );
        Ok(())
    }

    pub fn source_config(&self, key: SourceKey) -> Option<SourceConfig> {
        self.lock().sources.get(&key).map(|source| source.config.clone())
    }

    pub fn start_playback(&self, key: SourceKey) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        source.play_started.get_or_insert_with(Instant::now);
        self.0.playback_changed.notify_all();
        Ok(())
    }

    pub fn pause_playback(&self, key: SourceKey) -> Result<(), &'static str> {
        let mut state = self.lock();
        let source = state.sources.get_mut(&key).ok_or("source does not exist")?;
        if !matches!(source.config, SourceConfig::Video(_)) {
            return Err("PAUSE applies only to video");
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
        if !matches!(source.config, SourceConfig::Video(_)) || epoch <= source.last_epoch {
            return Err("FLUSH requires a video source and a greater epoch");
        }
        source.last_epoch = epoch;
        source.play_started = None;
        source.played_before_pause = Duration::ZERO;
        source.first_pts_us = None;
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
        self.0.playback_changed.notify_all();
        Ok(())
    }

    pub fn eos_epoch(&self, key: SourceKey) -> Option<u32> {
        self.lock().sources.get(&key).and_then(|source| source.eos_epoch)
    }

    pub fn source_epoch(&self, key: SourceKey) -> Option<u32> {
        self.lock().sources.get(&key).map(|source| source.last_epoch)
    }

    pub fn wait_for_presentation(&self, key: SourceKey, pts_us: i64) -> bool {
        let mut state = self.lock();
        loop {
            let Some(source) = state.sources.get_mut(&key) else {
                return false;
            };
            let Some(started) = source.play_started else {
                state = self
                    .0
                    .playback_changed
                    .wait_timeout(state, Duration::from_millis(100))
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0;
                continue;
            };
            let first_pts = *source.first_pts_us.get_or_insert(pts_us.max(0));
            let relative_us = pts_us.saturating_sub(first_pts).max(0) as u64;
            let target = Duration::from_micros(relative_us);
            if target <= source.played_before_pause {
                return true;
            }
            let deadline = started + target.saturating_sub(source.played_before_pause);
            let now = Instant::now();
            if deadline <= now {
                return true;
            }
            let timeout = deadline.saturating_duration_since(now).min(Duration::from_millis(100));
            state = self
                .0
                .playback_changed
                .wait_timeout(state, timeout)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
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
        if decoded_pixels > MAX_DECODED_PIXELS {
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
        if nodes.len() > MAX_NODES {
            return Err("node quota exceeded");
        }
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
                let (x, y, text_anchored) = if let Some(anchor_id) = node.anchor_id {
                    let anchor = state.anchors.get(&(node.session_id, anchor_id))?;
                    (
                        node.x.saturating_add((anchor.column as i64) << 32),
                        node.y.saturating_add(i64::from(anchor.line) << 32),
                        true,
                    )
                } else {
                    (node.x, node.y, false)
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
                            (
                                node.x.saturating_add((anchor.column as i64) << 32),
                                node.y.saturating_add(
                                    (i64::from(anchor.line) + display_offset as i64) << 32,
                                ),
                            )
                        } else {
                            (node.x, node.y)
                        };
                        x < right
                            && y < bottom
                            && x.saturating_add(node.width) > 0
                            && y.saturating_add(node.height) > 0
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
    if node.width <= 0 || node.height <= 0 {
        return Err("node output rectangle is empty");
    }
    Ok(())
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
        // viewport coordinate.
        assert!(scene.snapshot().1.is_empty());
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
}
