//! Message bar configuration.

use std::time::Duration;

use serde::Serialize;

/// Message bar configuration.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct MessageBar {
    /// Time before warning messages are dismissed, in seconds.
    warning_timeout: u16,
}

impl Default for MessageBar {
    fn default() -> Self {
        Self { warning_timeout: 5 }
    }
}

impl MessageBar {
    /// Time before warning messages are dismissed.
    #[inline]
    pub fn warning_timeout(self) -> Duration {
        Duration::from_secs(u64::from(self.warning_timeout))
    }
}

impl_config_deserialize!(MessageBar { warning_timeout });

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_timeout_defaults_to_five_seconds() {
        assert_eq!(MessageBar::default().warning_timeout(), Duration::from_secs(5));
    }

    #[test]
    fn warning_timeout_deserializes_as_seconds() {
        let config: MessageBar = toml::from_str("warning_timeout = 9").unwrap();

        assert_eq!(config.warning_timeout(), Duration::from_secs(9));
    }
}
