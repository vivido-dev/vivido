use log::LevelFilter;
use serde::Serialize;

use vivido_config_derive::ConfigDeserialize;

/// Debugging options.
#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Debug {
    pub log_level: LevelFilter,

    pub print_events: bool,

    /// Keep the log file after quitting.
    pub persistent_logging: bool,

    /// Should show render timer.
    pub render_timer: bool,

    /// Highlight damage information produced by vivido.
    pub highlight_damage: bool,

    /// Removed renderer selection compatibility key.
    #[config(alias = "renderer", removed = "Vivido now uses Vello/wgpu exclusively")]
    #[serde(skip_serializing)]
    renderer_removed: Option<String>,

    /// Removed EGL preference compatibility key.
    #[config(alias = "prefer_egl", removed = "Vivido no longer creates EGL contexts")]
    #[serde(skip_serializing)]
    prefer_egl_removed: bool,

    /// Record ref test.
    #[config(skip)]
    #[serde(skip_serializing)]
    pub ref_test: bool,
}

impl Default for Debug {
    fn default() -> Self {
        Self {
            log_level: LevelFilter::Warn,
            print_events: Default::default(),
            persistent_logging: Default::default(),
            render_timer: Default::default(),
            highlight_damage: Default::default(),
            ref_test: Default::default(),
            renderer_removed: Default::default(),
            prefer_egl_removed: Default::default(),
        }
    }
}
