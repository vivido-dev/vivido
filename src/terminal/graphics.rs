//! Protocol-neutral placeholders for terminal graphics and media.
//!
//! Protocol decoders (Sixel, Kitty, or a future Vivido protocol) should translate escape
//! sequences into [`GraphicsCommand`] values. The terminal forwards these commands to the UI;
//! decoding and GPU resource management intentionally live outside the terminal grid.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Graphics protocol which produced a media command.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GraphicsProtocol {
    /// DEC Sixel image protocol.
    Sixel,
    /// Kitty graphics protocol.
    Kitty,
    /// A host-defined graphics protocol.
    Custom(String),
}

/// Type of media represented by a transmitted resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaKind {
    /// A still image.
    Image,
    /// An animated image.
    Animation,
    /// A video stream.
    Video,
}

/// Stable identifier assigned by a protocol decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaId(pub u64);

/// Pixel dimensions supplied by the protocol, when known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PixelDimensions {
    /// Width in this value's coordinate system.
    pub width: u32,
    /// Height in this value's coordinate system.
    pub height: u32,
}

/// Grid-relative placement for a decoded media resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaPlacement {
    /// Grid line coordinate.
    pub line: i32,
    /// Grid column coordinate.
    pub column: usize,
    /// Number of terminal columns.
    pub columns: Option<usize>,
    /// Number of terminal lines.
    pub lines: Option<usize>,
    /// Stacking order relative to other media placements.
    pub z_index: i32,
}

/// Playback state for animation and video resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlaybackAction {
    /// Start or resume playback.
    Play,
    /// Pause playback.
    Pause,
    /// Stop the addressed process or media playback.
    Stop,
    /// Seek to a media position expressed in milliseconds.
    SeekMillis(u64),
}

/// Target for a media deletion command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeleteTarget {
    /// One media resource.
    Resource(MediaId),
    /// All resources owned by this decoder.
    All,
}

/// Commands shared by protocol decoders and rendering backends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphicsCommand {
    /// Transmit encoded media resource data.
    Transmit {
        /// Protocol that produced this media command.
        protocol: GraphicsProtocol,
        /// Identifier scoped to this value's owning context.
        id: MediaId,
        /// Classification of this item.
        kind: MediaKind,
        /// Optional payload format identifier.
        format: Option<String>,
        /// Optional media pixel dimensions.
        dimensions: Option<PixelDimensions>,
        /// Shared encoded payload bytes.
        payload: Arc<[u8]>,
    },
    /// Place an existing resource in the terminal grid.
    Place {
        /// Identifier scoped to this value's owning context.
        id: MediaId,
        /// Grid placement for this media resource.
        placement: MediaPlacement,
    },
    /// Change playback of an existing resource.
    Playback {
        /// Identifier scoped to this value's owning context.
        id: MediaId,
        /// Action to execute.
        action: PlaybackAction,
    },
    /// Delete the selected owned media resources.
    Delete(DeleteTarget),
}

/// Error returned by a future protocol decoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphicsDecodeError {
    /// Human-readable diagnostic.
    pub message: String,
}

impl fmt::Display for GraphicsDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GraphicsDecodeError {}

/// Interface to be implemented by Sixel, Kitty, or custom protocol parsers.
pub trait GraphicsProtocolDecoder {
    /// Return the graphics protocol accepted by this decoder.
    fn protocol(&self) -> GraphicsProtocol;

    /// Decode protocol bytes into neutral graphics commands.
    ///
    /// # Errors
    ///
    /// Returns a decode error for malformed, unsupported, or oversized graphics input.
    fn decode(&mut self, bytes: &[u8]) -> Result<Vec<GraphicsCommand>, GraphicsDecodeError>;
}
