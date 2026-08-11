use log::LevelFilter;
use serde::Serialize;

/// Debugging options.
#[derive(Serialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
    #[serde(skip_serializing)]
    renderer_removed: Option<String>,

    /// Removed EGL preference compatibility key.
    #[serde(skip_serializing)]
    prefer_egl_removed: bool,

    /// Record ref test.
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

impl_config_deserialize!(Debug {
    log_level,
    print_events,
    persistent_logging,
    render_timer,
    highlight_damage,
    renderer_removed: option_alias_removed("renderer", "Vivido now uses Vello/wgpu exclusively"),
    prefer_egl_removed: alias_removed("prefer_egl", "Vivido no longer creates EGL contexts"),
    ref_test: skip,
});
