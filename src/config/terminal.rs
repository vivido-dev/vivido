use serde::{Deserialize, Deserializer, Serialize, de};
use toml::Value;

use crate::config::ui_config::{Program, StringVisitor};
use crate::terminal::term::Osc52;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Terminal {
    /// OSC52 support mode.
    pub osc52: SerdeOsc52,
    /// Allow terminal applications to create desktop notifications through OSC 9/99.
    pub osc_notifications: bool,
    /// Path to a shell program to run on startup.
    pub shell: Option<Program>,
}

impl Default for Terminal {
    fn default() -> Self {
        Self { osc52: Default::default(), osc_notifications: true, shell: None }
    }
}

#[derive(Serialize, Default, Copy, Clone, Debug, PartialEq)]
pub struct SerdeOsc52(pub Osc52);

impl<'de> Deserialize<'de> for SerdeOsc52 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = deserializer.deserialize_str(StringVisitor)?;
        Osc52::deserialize(Value::String(value)).map(SerdeOsc52).map_err(de::Error::custom)
    }
}

impl_config_deserialize!(Terminal { osc52, osc_notifications, shell: option });
impl_serde_replace!(SerdeOsc52);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_notifications_are_enabled_by_default_and_can_be_disabled() {
        assert!(toml::from_str::<Terminal>("").unwrap().osc_notifications);
        assert!(
            !toml::from_str::<Terminal>("osc_notifications = false").unwrap().osc_notifications
        );
    }
}
