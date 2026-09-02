//! The presentation-target abstraction.
//!
//! Core §1 gives a session exactly one presentation target, and the target decides what a scene
//! node's geometry means, which profiles are negotiable, whether terminal anchors exist, and what
//! authority the root context starts with. Vivido has only ever had a terminal target, so those
//! decisions were spread across the session actor, the scene, and the renderer.
//!
//! Gathering them behind one trait is what lets a desktop target land beside the terminal one
//! without either fabricating the other's coordinate truth — desktop §1 is explicit that a desktop
//! target "does not fabricate terminal grid metrics".

use std::sync::Mutex;

use vivid_protocol::cbor::Value;
use vivid_protocol::context::{OP_DESKTOP_INPUT, OP_KNOWN_MASK, OP_TERMINAL_ANCHOR};
use vivid_protocol::geometry::{NodeGeometry, Rotation, TargetExtent, decode_clip};
use vivid_protocol::messages::PayloadMap;
use vivid_protocol::registry;
use vivid_protocol::scene::SceneNode;
use vivid_protocol::surface::{DesktopSurfaceParameters, SurfaceDefinition};
use vivid_protocol::target::{
    DesktopTarget as DesktopDescriptor, DesktopTargetState, OutputDescriptor, TargetTransition,
    reason,
};

use crate::display::SizeInfo;
use crate::terminal::grid::Dimensions;

/// Geometry of the window backing a presentation target.
///
/// Every field is in physical pixels. A terminal target uses all of it; a desktop target uses only
/// the viewport, and reports scale 1:1 — separating logical from physical pixels is a DPI concern
/// that arrives with real capture, where the points-versus-backing-pixels distinction matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayGeometry {
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub columns: u32,
    pub rows: u32,
    pub cell_width: u32,
    pub cell_height: u32,
}

impl From<SizeInfo> for DisplayGeometry {
    fn from(size: SizeInfo) -> Self {
        Self {
            viewport_width: size.width() as u32,
            viewport_height: size.height() as u32,
            columns: size.columns() as u32,
            rows: size.screen_lines() as u32,
            cell_width: size.cell_width().round() as u32,
            cell_height: size.cell_height().round() as u32,
        }
    }
}

/// One `TARGET_CHANGED` a target owes its producers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetChange {
    pub descriptor: PayloadMap,
    pub generation: u64,
    pub reason: u64,
}

/// A clip rectangle in the target's placement space, as signed 32.32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipRect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

/// Where a scene node sits, in the target's own placement units.
///
/// For the terminal target one unit is one cell and the last two fields carry the text-layer and
/// anchoring semantics of `terminal-surface-v1`. A desktop target places in logical pixels and
/// leaves both at their defaults, because it has no text plane to interleave with and no anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodePlacement {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub text_layer: u64,
    pub text_anchored: bool,
}

/// One presentation target profile.
pub trait PresentationTarget: Send + Sync + 'static {
    /// The profile a session must select in `HELLO` key 7.
    fn profile_name(&self) -> &'static str;

    /// Every profile this target can accept, including core.
    fn supported_profiles(&self) -> &'static [&'static str];

    /// Operation classes the root context starts with.
    ///
    /// This is where a target says what it can actually do rather than what the registry defines:
    /// a terminal target masks off desktop input because it has nowhere to inject it.
    fn root_operation_classes(&self) -> u64;

    /// Whether `terminal-surface-v1` anchors exist for this target.
    fn accepts_anchors(&self) -> bool;

    /// Reject a node this target cannot place.
    fn validate_node(&self, node: &SceneNode) -> Result<(), &'static str>;

    /// Reject a surface whose semantic profile or profile parameters this target cannot present.
    fn validate_surface(&self, definition: &SurfaceDefinition) -> Result<(), &'static str>;

    /// Decode a node's geometry into placement units, or `None` when it is malformed.
    fn placement(&self, node: &SceneNode) -> Option<NodePlacement>;

    /// Decode a node's optional clip into placement units.
    fn clip(&self, node: &SceneNode) -> Option<ClipRect>;

    /// Device pixels per placement unit, horizontally and vertically.
    ///
    /// The renderer multiplies placement-space coordinates by this to reach the framebuffer. A
    /// terminal returns its cell size; a desktop target returns one.
    fn placement_scale(&self, size: &SizeInfo) -> (f32, f32);

    /// The target descriptor for `WELCOME` key 5.
    fn descriptor(&self) -> PayloadMap;

    /// The current target generation.
    fn generation(&self) -> u64;

    /// The current extent in target logical pixels, which normalized geometry projects against.
    fn extent(&self) -> TargetExtent;

    /// Accept new window geometry, returning the new generation when the target actually moved.
    fn offer_geometry(&self, geometry: DisplayGeometry) -> Option<u64>;

    /// Take a queued change, or re-announce the current one as settled.
    ///
    /// A resize is announced unsettled on the frame that applies it and settled once the settle
    /// timer fires, so the settled announcement is rebuilt from current state rather than from the
    /// queued change that an earlier frame already consumed.
    fn take_change(&self, settled_generation: Option<u64>) -> Option<TargetChange>;
}

/// Active anchors a terminal target advertises room for.
pub const MAX_ACTIVE_ANCHORS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalMetrics {
    geometry: DisplayGeometry,
    generation: u64,
    settled: bool,
}

/// The `terminal-surface-v1` target: a grid of cells with a text plane and anchors.
#[derive(Debug)]
pub struct TerminalTarget {
    current: Mutex<TerminalMetrics>,
    queued: Mutex<Option<TerminalMetrics>>,
}

impl TerminalTarget {
    const PROFILES: &'static [&'static str] = &[
        registry::AUDIO_GAIN,
        registry::CORE_CONTROL,
        registry::FILE_DROP,
        // Offered only when the config option is on; see `ServiceShared::offered_profiles`.
        registry::FILE_DROP_PATH,
        registry::LIVE_MEDIA,
        registry::OBSERVABILITY,
        registry::TERMINAL_SURFACE,
        registry::TIMED_MEDIA,
        registry::WEB_CARRIER,
    ];

    pub fn new(geometry: DisplayGeometry) -> Result<Self, &'static str> {
        Self::validate(geometry)?;
        Ok(Self {
            current: Mutex::new(TerminalMetrics { geometry, generation: 1, settled: true }),
            queued: Mutex::new(None),
        })
    }

    fn validate(geometry: DisplayGeometry) -> Result<(), &'static str> {
        if geometry.viewport_width == 0
            || geometry.viewport_height == 0
            || geometry.columns == 0
            || geometry.rows == 0
            || geometry.cell_width == 0
            || geometry.cell_height == 0
        {
            return Err("terminal target geometry must be positive");
        }
        Ok(())
    }

    fn descriptor_for(geometry: DisplayGeometry, settled: bool) -> PayloadMap {
        vec![
            (0, Value::Unsigned(u64::from(geometry.viewport_width))),
            (1, Value::Unsigned(u64::from(geometry.viewport_height))),
            (2, Value::Unsigned(u64::from(geometry.columns))),
            (3, Value::Unsigned(u64::from(geometry.rows))),
            (4, Value::Unsigned(u64::from(geometry.cell_width))),
            (5, Value::Unsigned(u64::from(geometry.cell_height))),
            (6, Value::Bool(settled)),
            (7, Value::Unsigned(3)),
            (8, Value::Unsigned(MAX_ACTIVE_ANCHORS as u64)),
        ]
    }
}

impl PresentationTarget for TerminalTarget {
    fn profile_name(&self) -> &'static str {
        registry::TERMINAL_SURFACE
    }

    fn supported_profiles(&self) -> &'static [&'static str] {
        Self::PROFILES
    }

    fn root_operation_classes(&self) -> u64 {
        // A terminal has no desktop to inject into.
        OP_KNOWN_MASK & !OP_DESKTOP_INPUT
    }

    fn accepts_anchors(&self) -> bool {
        true
    }

    fn validate_surface(&self, definition: &SurfaceDefinition) -> Result<(), &'static str> {
        if definition.semantic_profile == registry::DESKTOP_CONTENT {
            return Err("a terminal target cannot present a desktop-content-v1 surface");
        }
        Ok(())
    }

    fn validate_node(&self, node: &SceneNode) -> Result<(), &'static str> {
        node.validate().map_err(|_| "invalid scene node")?;
        self.placement(node).ok_or("invalid terminal node geometry")?;
        if self.clip(node).is_none() && node.clip.is_some() {
            return Err("invalid terminal node clip");
        }
        Ok(())
    }

    fn placement(&self, node: &SceneNode) -> Option<NodePlacement> {
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
                && (node.geometry.len() != 8
                    || value(6)?.as_u64()? == 0
                    || value(7)?.as_u64()? == 0))
        {
            return None;
        }
        Some(NodePlacement {
            x,
            y,
            width,
            height,
            text_layer: layer,
            text_anchored: coordinate_space == 2,
        })
    }

    fn clip(&self, node: &SceneNode) -> Option<ClipRect> {
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

    fn placement_scale(&self, size: &SizeInfo) -> (f32, f32) {
        // The unrounded cell size, matching what the renderer has always used. The integer cell
        // metrics in the target descriptor are rounded for the wire; the two must not be confused.
        (size.cell_width(), size.cell_height())
    }

    fn descriptor(&self) -> PayloadMap {
        let current = *self.current.lock().expect("terminal metrics");
        Self::descriptor_for(current.geometry, current.settled)
    }

    fn generation(&self) -> u64 {
        self.current.lock().expect("terminal metrics").generation
    }

    fn extent(&self) -> TargetExtent {
        let current = self.current.lock().expect("terminal metrics");
        TargetExtent::new(current.geometry.viewport_width, current.geometry.viewport_height)
    }

    fn offer_geometry(&self, geometry: DisplayGeometry) -> Option<u64> {
        if Self::validate(geometry).is_err() {
            return None;
        }
        let mut current = self.current.lock().expect("terminal metrics");
        if current.geometry == geometry {
            return None;
        }
        let generation = current.generation.checked_add(1)?;
        *current = TerminalMetrics { geometry, generation, settled: false };
        *self.queued.lock().expect("terminal queued") = Some(*current);
        Some(generation)
    }

    fn take_change(&self, settled_generation: Option<u64>) -> Option<TargetChange> {
        // Taken in the same order as `offer_geometry` acquires them.
        let mut current = self.current.lock().expect("terminal metrics");
        let queued = self.queued.lock().expect("terminal queued").take();
        let metrics = match queued {
            Some(metrics) => metrics,
            None if settled_generation == Some(current.generation) => *current,
            None => return None,
        };
        let settled = settled_generation == Some(metrics.generation);
        if settled && current.generation == metrics.generation {
            current.settled = true;
        }
        drop(current);
        Some(TargetChange {
            descriptor: Self::descriptor_for(metrics.geometry, settled),
            generation: metrics.generation,
            reason: 0x1f,
        })
    }
}

/// The `desktop-surface-v1` target: a virtual desktop in logical pixels, with no grid and no
/// anchors. Desktop §1 is explicit that it "does not fabricate terminal grid metrics".
#[derive(Debug)]
pub struct DesktopTarget {
    state: Mutex<DesktopTargetState>,
    queued: Mutex<Option<TargetChange>>,
}

impl DesktopTarget {
    const PROFILES: &'static [&'static str] = &[
        registry::AUDIO_GAIN,
        registry::CORE_CONTROL,
        registry::DESKTOP_SURFACE,
        registry::DESKTOP_INPUT,
        registry::FILE_DROP,
        registry::LIVE_MEDIA,
        registry::OBSERVABILITY,
        registry::TIMED_MEDIA,
        registry::WEB_CARRIER,
    ];

    pub fn new(geometry: DisplayGeometry) -> Result<Self, &'static str> {
        let descriptor = Self::describe(geometry, true, 1)?;
        Ok(Self {
            state: Mutex::new(
                DesktopTargetState::new(descriptor).map_err(|_| "invalid desktop target")?,
            ),
            queued: Mutex::new(None),
        })
    }

    /// One window becomes a single-output virtual desktop.
    fn describe(
        geometry: DisplayGeometry,
        settled: bool,
        topology_revision: u64,
    ) -> Result<DesktopDescriptor, &'static str> {
        if geometry.viewport_width == 0 || geometry.viewport_height == 0 {
            return Err("desktop target extent must be positive");
        }
        Ok(DesktopDescriptor {
            origin_x: 0,
            origin_y: 0,
            width: geometry.viewport_width,
            height: geometry.viewport_height,
            outputs: vec![OutputDescriptor {
                // Session-local, and deliberately not a device identity: desktop §1 forbids a
                // serial, user name, desktop name, window title, or login-session ID here.
                output_id: 1,
                origin_x: 0,
                origin_y: 0,
                width: geometry.viewport_width,
                height: geometry.viewport_height,
                scale_numerator: 1,
                scale_denominator: 1,
                rotation: Rotation::None,
                primary: true,
            }],
            settled,
            topology_revision,
        })
    }
}

impl PresentationTarget for DesktopTarget {
    fn profile_name(&self) -> &'static str {
        registry::DESKTOP_SURFACE
    }

    fn supported_profiles(&self) -> &'static [&'static str] {
        Self::PROFILES
    }

    fn root_operation_classes(&self) -> u64 {
        // A desktop has no terminal text to anchor into, and it can receive desktop input.
        OP_KNOWN_MASK & !OP_TERMINAL_ANCHOR
    }

    fn accepts_anchors(&self) -> bool {
        false
    }

    fn validate_surface(&self, definition: &SurfaceDefinition) -> Result<(), &'static str> {
        if definition.semantic_profile == registry::TERMINAL_CONTENT {
            return Err("a desktop target cannot present a terminal-content-v1 surface");
        }
        if definition.semantic_profile == registry::DESKTOP_CONTENT {
            // Desktop §2 keys 0 through 4, decoded rather than passed through: a topology that
            // carries free-form data has no way through this.
            DesktopSurfaceParameters::decode(&definition.profile_parameters)
                .map_err(|_| "invalid desktop-content-v1 profile parameters")?;
        }
        Ok(())
    }

    fn validate_node(&self, node: &SceneNode) -> Result<(), &'static str> {
        node.validate().map_err(|_| "invalid scene node")?;
        NodeGeometry::decode(&node.geometry).map_err(|_| "invalid desktop node geometry")?;
        if let Some(clip) = node.clip.as_ref() {
            decode_clip(clip).map_err(|_| "invalid desktop node clip")?;
        }
        Ok(())
    }

    fn placement(&self, node: &SceneNode) -> Option<NodePlacement> {
        let geometry = NodeGeometry::decode(&node.geometry).ok()?;
        let rect = geometry.project(self.extent()).ok()?;
        Some(NodePlacement {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            // A desktop has no text plane to interleave with and no anchors to follow.
            text_layer: 0,
            text_anchored: false,
        })
    }

    fn clip(&self, node: &SceneNode) -> Option<ClipRect> {
        let clip = decode_clip(node.clip.as_ref()?).ok()?;
        Some(ClipRect { x: clip.x, y: clip.y, width: clip.width, height: clip.height })
    }

    fn placement_scale(&self, _size: &SizeInfo) -> (f32, f32) {
        // Placement units already are target logical pixels.
        (1.0, 1.0)
    }

    fn descriptor(&self) -> PayloadMap {
        self.state.lock().expect("desktop target").current().encode()
    }

    fn generation(&self) -> u64 {
        self.state.lock().expect("desktop target").generation().get()
    }

    fn extent(&self) -> TargetExtent {
        self.state.lock().expect("desktop target").current().extent()
    }

    fn offer_geometry(&self, geometry: DisplayGeometry) -> Option<u64> {
        let mut state = self.state.lock().expect("desktop target");
        let revision = state.current().topology_revision.saturating_add(1);
        let next = Self::describe(geometry, false, revision).ok()?;
        match state.offer(next).ok()? {
            TargetTransition::Unchanged => None,
            TargetTransition::Settled { .. } => None,
            TargetTransition::Advanced { generation, reason } => {
                let change = TargetChange {
                    descriptor: state.current().encode(),
                    generation: generation.get(),
                    reason,
                };
                *self.queued.lock().expect("desktop queued") = Some(change);
                Some(generation.get())
            },
        }
    }

    fn take_change(&self, settled_generation: Option<u64>) -> Option<TargetChange> {
        let mut state = self.state.lock().expect("desktop target");
        if let Some(change) = self.queued.lock().expect("desktop queued").take() {
            return Some(change);
        }
        // Re-announce as settled. Core §2.2 has this repeat the current generation exactly rather
        // than advancing it, which `DesktopTargetState` enforces.
        if settled_generation != Some(state.generation().get()) {
            return None;
        }
        let mut settled = state.current().clone();
        if settled.settled {
            return None;
        }
        settled.settled = true;
        let generation = match state.offer(settled).ok()? {
            TargetTransition::Settled { generation } => generation.get(),
            TargetTransition::Advanced { generation, .. } => generation.get(),
            TargetTransition::Unchanged => return None,
        };
        Some(TargetChange {
            descriptor: state.current().encode(),
            generation,
            reason: reason::PRESENTATION_WINDOW,
        })
    }
}

#[cfg(test)]
mod tests {
    use vivid_protocol::cbor::Value;
    use vivid_protocol::scene::Fit;

    use super::*;

    fn geometry() -> DisplayGeometry {
        DisplayGeometry {
            viewport_width: 800,
            viewport_height: 600,
            columns: 80,
            rows: 24,
            cell_width: 10,
            cell_height: 25,
        }
    }

    fn terminal() -> TerminalTarget {
        TerminalTarget::new(geometry()).unwrap()
    }

    fn node(geometry: Vec<(u64, Value)>, clip: Option<Vec<(u64, Value)>>) -> SceneNode {
        SceneNode {
            owning_context_id: 1,
            node_id: 1,
            surface_context_id: 1,
            surface_id: 1,
            geometry,
            fit: Fit::Contain,
            linear_sampling: true,
            z_index: 0,
            visible: true,
            opacity: u16::MAX,
            clip,
        }
    }

    fn grid_geometry() -> Vec<(u64, Value)> {
        vec![
            (0, Value::Unsigned(1)),
            (1, Value::Unsigned(2 << 32)),
            (2, Value::Unsigned(3 << 32)),
            (3, Value::Unsigned(10 << 32)),
            (4, Value::Unsigned(5 << 32)),
            (5, Value::Unsigned(1)),
        ]
    }

    fn anchored_geometry() -> Vec<(u64, Value)> {
        let mut geometry = grid_geometry();
        // Coordinate space 2 is text-anchored, and adds the anchor context and anchor IDs.
        geometry[0].1 = Value::Unsigned(2);
        geometry.push((6, Value::Unsigned(1)));
        geometry.push((7, Value::Unsigned(9)));
        geometry
    }

    #[test]
    fn grid_geometry_places_in_cells() {
        let placement = terminal().placement(&node(grid_geometry(), None)).unwrap();
        assert_eq!(placement.x, 2 << 32);
        assert_eq!(placement.text_layer, 1);
        assert!(!placement.text_anchored);
    }

    #[test]
    fn anchored_geometry_is_recognised() {
        let placement = terminal().placement(&node(anchored_geometry(), None)).unwrap();
        assert!(placement.text_anchored);
    }

    #[test]
    fn malformed_geometry_is_refused() {
        // Wrong key count for the declared coordinate space.
        let mut short = anchored_geometry();
        short.pop();
        assert!(terminal().placement(&node(short, None)).is_none());

        // Zero extent, unknown space, and an out-of-range text layer.
        for (key, value) in [(3, 0_u64), (0, 7), (5, 3)] {
            let mut broken = grid_geometry();
            broken[key as usize].1 = Value::Unsigned(value);
            assert!(
                terminal().placement(&node(broken, None)).is_none(),
                "key {key} = {value} should have been refused"
            );
        }
    }

    #[test]
    fn a_present_but_invalid_clip_fails_validation() {
        let clip = Some(vec![
            (0, Value::Unsigned(0)),
            (1, Value::Unsigned(0)),
            (2, Value::Unsigned(0)),
            (3, Value::Unsigned(1 << 32)),
        ]);
        let node = node(grid_geometry(), clip);
        assert!(terminal().clip(&node).is_none());
        assert!(terminal().validate_node(&node).is_err());
    }

    #[test]
    fn an_absent_clip_is_valid() {
        assert!(terminal().validate_node(&node(grid_geometry(), None)).is_ok());
    }

    #[test]
    fn the_terminal_target_masks_off_desktop_input() {
        assert_eq!(terminal().root_operation_classes() & OP_DESKTOP_INPUT, 0);
        assert!(terminal().accepts_anchors());
        assert_eq!(terminal().profile_name(), registry::TERMINAL_SURFACE);
    }
}
