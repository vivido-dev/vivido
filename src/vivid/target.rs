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

use vivid_protocol::context::{OP_DESKTOP_INPUT, OP_KNOWN_MASK};
use vivid_protocol::registry;
use vivid_protocol::scene::SceneNode;

use crate::display::SizeInfo;

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

    /// Decode a node's geometry into placement units, or `None` when it is malformed.
    fn placement(&self, node: &SceneNode) -> Option<NodePlacement>;

    /// Decode a node's optional clip into placement units.
    fn clip(&self, node: &SceneNode) -> Option<ClipRect>;

    /// Device pixels per placement unit, horizontally and vertically.
    ///
    /// The renderer multiplies placement-space coordinates by this to reach the framebuffer. A
    /// terminal returns its cell size; a desktop target returns one.
    fn placement_scale(&self, size: &SizeInfo) -> (f32, f32);
}

/// The `terminal-surface-v1` target: a grid of cells with a text plane and anchors.
#[derive(Debug, Default, Clone, Copy)]
pub struct TerminalTarget;

impl TerminalTarget {
    const PROFILES: &'static [&'static str] = &[
        registry::CORE_CONTROL,
        registry::LIVE_MEDIA,
        registry::OBSERVABILITY,
        registry::TERMINAL_SURFACE,
        registry::TIMED_MEDIA,
    ];
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
}

#[cfg(test)]
mod tests {
    use vivid_protocol::cbor::Value;
    use vivid_protocol::scene::Fit;

    use super::*;

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
        let placement = TerminalTarget.placement(&node(grid_geometry(), None)).unwrap();
        assert_eq!(placement.x, 2 << 32);
        assert_eq!(placement.text_layer, 1);
        assert!(!placement.text_anchored);
    }

    #[test]
    fn anchored_geometry_is_recognised() {
        let placement = TerminalTarget.placement(&node(anchored_geometry(), None)).unwrap();
        assert!(placement.text_anchored);
    }

    #[test]
    fn malformed_geometry_is_refused() {
        // Wrong key count for the declared coordinate space.
        let mut short = anchored_geometry();
        short.pop();
        assert!(TerminalTarget.placement(&node(short, None)).is_none());

        // Zero extent, unknown space, and an out-of-range text layer.
        for (key, value) in [(3, 0_u64), (0, 7), (5, 3)] {
            let mut broken = grid_geometry();
            broken[key as usize].1 = Value::Unsigned(value);
            assert!(
                TerminalTarget.placement(&node(broken, None)).is_none(),
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
        assert!(TerminalTarget.clip(&node).is_none());
        assert!(TerminalTarget.validate_node(&node).is_err());
    }

    #[test]
    fn an_absent_clip_is_valid() {
        assert!(TerminalTarget.validate_node(&node(grid_geometry(), None)).is_ok());
    }

    #[test]
    fn the_terminal_target_masks_off_desktop_input() {
        assert_eq!(TerminalTarget.root_operation_classes() & OP_DESKTOP_INPUT, 0);
        assert!(TerminalTarget.accepts_anchors());
        assert_eq!(TerminalTarget.profile_name(), registry::TERMINAL_SURFACE);
    }
}
