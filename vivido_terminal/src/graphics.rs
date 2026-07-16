//! Protocol-neutral placeholders for terminal graphics and media.
//!
//! Protocol decoders (Sixel, Kitty, or a future Vivido protocol) should translate escape
//! sequences into [`GraphicsCommand`] values. The terminal forwards these commands to the UI;
//! decoding and GPU resource management intentionally live outside the terminal grid.

use std::fmt;
use std::sync::Arc;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Graphics protocol which produced a media command.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum GraphicsProtocol {
    Sixel,
    Kitty,
    Custom(String),
}

/// Type of media represented by a transmitted resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum MediaKind {
    Image,
    Animation,
    Video,
}

/// Stable identifier assigned by a protocol decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct MediaId(pub u64);

/// Pixel dimensions supplied by the protocol, when known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PixelDimensions {
    pub width: u32,
    pub height: u32,
}

/// Grid-relative placement for a decoded media resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct MediaPlacement {
    pub line: i32,
    pub column: usize,
    pub columns: Option<usize>,
    pub lines: Option<usize>,
    pub z_index: i32,
}

/// Playback state for animation and video resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PlaybackAction {
    Play,
    Pause,
    Stop,
    SeekMillis(u64),
}

/// Target for a media deletion command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum DeleteTarget {
    Resource(MediaId),
    All,
}

/// Commands shared by protocol decoders and rendering backends.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum GraphicsCommand {
    Transmit {
        protocol: GraphicsProtocol,
        id: MediaId,
        kind: MediaKind,
        format: Option<String>,
        dimensions: Option<PixelDimensions>,
        payload: Arc<[u8]>,
    },
    Place {
        id: MediaId,
        placement: MediaPlacement,
    },
    Playback {
        id: MediaId,
        action: PlaybackAction,
    },
    Delete(DeleteTarget),
}

/// Error returned by a future protocol decoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphicsDecodeError {
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
    fn protocol(&self) -> GraphicsProtocol;

    fn decode(&mut self, bytes: &[u8]) -> Result<Vec<GraphicsCommand>, GraphicsDecodeError>;
}
