//! In-app update configuration.

use serde::{Deserialize, Serialize};

/// Controls update discovery for installed graphical applications.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Updates {
    /// Allow manual and automatic update discovery.
    pub enabled: bool,

    /// Check quietly after graphical startup.
    pub startup_check: bool,
}

impl Default for Updates {
    fn default() -> Self {
        Self { enabled: true, startup_check: true }
    }
}

impl_serde_replace!(Updates);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_update_settings_default_to_enabled() {
        let updates = toml::from_str::<Updates>("").expect("empty updates table");
        assert!(updates.enabled);
        assert!(updates.startup_check);
    }

    #[test]
    fn update_settings_can_be_disabled_independently() {
        let updates = toml::from_str::<Updates>("enabled = false\nstartup_check = false\n")
            .expect("explicit updates table");
        assert!(!updates.enabled);
        assert!(!updates.startup_check);
    }

    #[test]
    fn unknown_update_settings_are_rejected() {
        assert!(toml::from_str::<Updates>("automatic_install = true").is_err());
    }
}
