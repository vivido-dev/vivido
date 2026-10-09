use std::cmp;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::terminal::vvte::ansi::{CursorShape as VteCursorShape, CursorStyle as VteCursorStyle};

use crate::config::ui_config::Percentage;

/// The minimum blink interval value in milliseconds.
const MIN_BLINK_INTERVAL: u64 = 10;

/// The minimum number of blinks before pausing.
const MIN_BLINK_CYCLES_BEFORE_PAUSE: u64 = 1;

#[derive(Serialize, Copy, Clone, Debug, PartialEq)]
/// Cursor appearance and blink settings.
pub struct Cursor {
    /// Font style selection.
    pub style: ConfigCursorStyle,
    /// Draw an unfocused block cursor as an outline.
    pub unfocused_hollow: bool,

    thickness: Percentage,
    blink_interval: u64,
    blink_timeout: u8,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            thickness: Percentage::new(0.15),
            unfocused_hollow: true,
            blink_interval: 750,
            blink_timeout: 5,
            style: Default::default(),
        }
    }
}

impl Cursor {
    #[inline]
    /// Return cursor thickness as a fraction of cell width.
    pub fn thickness(self) -> f32 {
        self.thickness.as_f32()
    }

    #[inline]
    /// Return the configured style.
    pub fn style(self) -> VteCursorStyle {
        self.style.into()
    }

    #[inline]
    /// Return the configured cursor blink interval in milliseconds.
    pub fn blink_interval(self) -> u64 {
        cmp::max(self.blink_interval, MIN_BLINK_INTERVAL)
    }

    #[inline]
    /// Return the cursor blink inactivity timeout.
    pub fn blink_timeout(self) -> Duration {
        if self.blink_timeout == 0 {
            Duration::ZERO
        } else {
            cmp::max(
                // Show/hide is what we consider a cycle, so multiply by `2`.
                Duration::from_millis(self.blink_interval * 2 * MIN_BLINK_CYCLES_BEFORE_PAUSE),
                Duration::from_secs(self.blink_timeout as u64),
            )
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(untagged, deny_unknown_fields)]
/// A cursor shape with optional blink behavior.
pub enum ConfigCursorStyle {
    /// A cursor shape using the configured default blink policy.
    Shape(CursorShape),
    /// A cursor shape with an explicit blink policy.
    WithBlinking {
        #[serde(default)]
        /// Cursor geometry.
        shape: CursorShape,
        #[serde(default)]
        /// Cursor blink policy.
        blinking: CursorBlinking,
    },
}

impl Default for ConfigCursorStyle {
    fn default() -> Self {
        Self::Shape(CursorShape::default())
    }
}

impl ConfigCursorStyle {
    /// Check if blinking is force enabled/disabled.
    pub fn blinking_override(&self) -> Option<bool> {
        match self {
            Self::Shape(_) => None,
            Self::WithBlinking { blinking, .. } => blinking.blinking_override(),
        }
    }
}

impl From<ConfigCursorStyle> for VteCursorStyle {
    fn from(config_style: ConfigCursorStyle) -> Self {
        match config_style {
            ConfigCursorStyle::Shape(shape) => Self { shape: shape.into(), blinking: false },
            ConfigCursorStyle::WithBlinking { shape, blinking } => {
                Self { shape: shape.into(), blinking: blinking.into() }
            },
        }
    }
}

#[derive(Serialize, Default, Debug, Copy, Clone, PartialEq, Eq)]
/// Cursor blink policy, including application-request override behavior.
pub enum CursorBlinking {
    /// Never blink, regardless of application requests.
    Never,
    #[default]
    /// Initially disabled; applications may enable blinking.
    Off,
    /// Initially enabled; applications may disable blinking.
    On,
    /// Always blink, regardless of application requests.
    Always,
}

impl CursorBlinking {
    fn blinking_override(&self) -> Option<bool> {
        match self {
            Self::Never => Some(false),
            Self::Off | Self::On => None,
            Self::Always => Some(true),
        }
    }
}

impl From<CursorBlinking> for bool {
    fn from(blinking: CursorBlinking) -> bool {
        blinking == CursorBlinking::On || blinking == CursorBlinking::Always
    }
}

#[derive(Serialize, Debug, Default, Eq, PartialEq, Copy, Clone, Hash)]
/// The geometric shape of the terminal cursor.
pub enum CursorShape {
    #[default]
    /// A filled cell-sized cursor.
    Block,
    /// A horizontal cursor below the cell.
    Underline,
    /// A vertical cursor at the cell edge.
    Beam,
}

impl From<CursorShape> for VteCursorShape {
    fn from(value: CursorShape) -> Self {
        match value {
            CursorShape::Block => VteCursorShape::Block,
            CursorShape::Underline => VteCursorShape::Underline,
            CursorShape::Beam => VteCursorShape::Beam,
        }
    }
}

impl_config_deserialize!(Cursor {
    style,
    unfocused_hollow,
    thickness,
    blink_interval,
    blink_timeout,
});
impl_serde_replace!(ConfigCursorStyle);
impl_config_deserialize_enum!(CursorBlinking { Never, Off, On, Always });
impl_config_deserialize_enum!(CursorShape { Block, Underline, Beam });
