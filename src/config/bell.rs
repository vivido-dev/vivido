use std::time::Duration;

use serde::Serialize;

use crate::config::ui_config::Program;
use crate::display::color::Rgb;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
/// Visual bell animation settings.
pub struct BellConfig {
    /// Visual bell animation function.
    pub animation: BellAnimation,

    /// Command to run on bell.
    pub command: Option<Program>,

    /// Visual bell flash color.
    pub color: Rgb,

    /// Visual bell duration in milliseconds.
    duration: u16,
}

impl Default for BellConfig {
    fn default() -> Self {
        Self {
            color: Rgb::new(255, 255, 255),
            animation: Default::default(),
            command: Default::default(),
            duration: Default::default(),
        }
    }
}

impl BellConfig {
    /// Return the configured bell animation duration.
    pub fn duration(&self) -> Duration {
        Duration::from_millis(self.duration as u64)
    }
}

/// `VisualBellAnimations` are modeled after a subset of CSS transitions and Robert
/// Penner's Easing Functions.
#[derive(Serialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BellAnimation {
    // CSS animation.
    /// Ease-in/ease-out animation.
    Ease,
    // CSS animation.
    /// Animation that decelerates toward completion.
    EaseOut,
    // Penner animation.
    /// Decelerating sine easing curve.
    EaseOutSine,
    // Penner animation.
    /// Decelerating quad easing curve.
    EaseOutQuad,
    // Penner animation.
    /// Decelerating cubic easing curve.
    EaseOutCubic,
    // Penner animation.
    /// Decelerating quart easing curve.
    EaseOutQuart,
    // Penner animation.
    /// Decelerating quint easing curve.
    EaseOutQuint,
    // Penner animation.
    /// Decelerating expo easing curve.
    EaseOutExpo,
    // Penner animation.
    /// Decelerating circ easing curve.
    EaseOutCirc,
    // Penner animation.
    #[default]
    /// Constant-speed animation.
    Linear,
}

impl_config_deserialize!(BellConfig { animation, command: option, color, duration });
impl_config_deserialize_enum!(BellAnimation {
    Ease,
    EaseOut,
    EaseOutSine,
    EaseOutQuad,
    EaseOutCubic,
    EaseOutQuart,
    EaseOutQuint,
    EaseOutExpo,
    EaseOutCirc,
    Linear,
});
