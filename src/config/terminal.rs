use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, de};
use toml::Value;

use crate::config::ui_config::Program;
use crate::terminal::term::{ClipboardAccess, Osc52};

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Terminal {
    /// OSC52 clipboard policy.
    pub osc52: SerdeOsc52,
    /// Ask before pasting text that would run commands in an application without bracketed paste.
    pub paste_protection: bool,
    /// Allow terminal applications to create desktop notifications through OSC 9/99.
    pub osc_notifications: bool,
    /// Draw a progress bar for OSC 9;4 reports and activity reported by spinner titles.
    pub progress: bool,
    /// When a shell command running longer than [`Terminal::notify_on_command_finish_after`]
    /// finishes, signal it.
    pub notify_on_command_finish: NotifyOnCommandFinish,
    /// How a finished long command signals: the normal bell path, a desktop notification, or both.
    pub notify_on_command_finish_action: NotifyOnCommandFinishAction,
    /// How long a command must run before its finish signals, in seconds.
    notify_on_command_finish_after: u64,
    /// Path to a shell program to run on startup.
    pub shell: Option<Program>,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            osc52: Default::default(),
            paste_protection: true,
            osc_notifications: true,
            progress: true,
            notify_on_command_finish: Default::default(),
            notify_on_command_finish_action: Default::default(),
            notify_on_command_finish_after: 5,
            shell: None,
        }
    }
}

impl Terminal {
    /// How long a command must run before its finish signals.
    pub fn notify_on_command_finish_after(&self) -> Duration {
        Duration::from_secs(self.notify_on_command_finish_after)
    }

    /// Which signals a finished command emits, given the window focus and the measured runtime.
    ///
    /// A command with no measured start — the shell emitted `D` without `B`/`C` — never signals,
    /// as there is no runtime to compare against the threshold.
    pub fn command_finish_actions(
        &self,
        focused: bool,
        duration: Option<Duration>,
    ) -> NotifyOnCommandFinishAction {
        let eligible = match self.notify_on_command_finish {
            NotifyOnCommandFinish::Never => false,
            NotifyOnCommandFinish::Unfocused => !focused,
            NotifyOnCommandFinish::Always => true,
        };
        let slow_enough =
            duration.is_some_and(|elapsed| elapsed > self.notify_on_command_finish_after());
        if eligible && slow_enough {
            self.notify_on_command_finish_action
        } else {
            NotifyOnCommandFinishAction { bell: false, notify: false }
        }
    }
}

/// When a finished long command signals its completion.
#[derive(Serialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyOnCommandFinish {
    /// Never signal.
    #[default]
    Never,
    /// Signal only when the window is not focused.
    Unfocused,
    /// Signal even when the window is focused.
    Always,
}

/// How a finished long command signals: each channel is independent.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotifyOnCommandFinishAction {
    /// Ring the bell (visual flash, urgency hint, and `bell.command`, as with `\x07`).
    pub bell: bool,
    /// Show a desktop notification naming the exit code and runtime.
    pub notify: bool,
}

impl Default for NotifyOnCommandFinishAction {
    fn default() -> Self {
        Self { bell: true, notify: false }
    }
}

#[derive(Serialize, Default, Copy, Clone, Debug, PartialEq)]
pub struct SerdeOsc52(pub Osc52);

impl<'de> Deserialize<'de> for SerdeOsc52 {
    /// Accept `{ read = "ask", write = "allow" }`, either key optional, or one of the single words
    /// from before reading and setting had separate policies.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut osc52 = Osc52::default();
        match Value::deserialize(deserializer)? {
            Value::String(mode) => {
                let (read, write) = match mode.to_ascii_lowercase().as_str() {
                    "disabled" => (ClipboardAccess::Deny, ClipboardAccess::Deny),
                    "onlycopy" => (ClipboardAccess::Deny, ClipboardAccess::Allow),
                    "onlypaste" => (ClipboardAccess::Allow, ClipboardAccess::Deny),
                    "copypaste" => (ClipboardAccess::Allow, ClipboardAccess::Allow),
                    _ => {
                        return Err(de::Error::custom(format!(
                            "unknown osc52 mode `{mode}`; use a table such as \
                             {{ read = \"ask\", write = \"allow\" }}"
                        )));
                    },
                };
                osc52 = Osc52 { read, write };
            },
            Value::Table(table) => {
                for (key, value) in table {
                    let access = value.as_str().and_then(clipboard_access).ok_or_else(|| {
                        de::Error::custom(format!(
                            "osc52.{key} must be one of `deny`, `ask`, `allow`, not {value}"
                        ))
                    })?;
                    match key.as_str() {
                        "read" => osc52.read = access,
                        "write" => osc52.write = access,
                        _ => return Err(de::Error::unknown_field(&key, &["read", "write"])),
                    }
                }
            },
            value => {
                return Err(de::Error::custom(format!(
                    "osc52 must be a table such as {{ read = \"ask\", write = \"allow\" }}, not \
                     {value}"
                )));
            },
        }
        Ok(SerdeOsc52(osc52))
    }
}

fn clipboard_access(value: &str) -> Option<ClipboardAccess> {
    match value.to_ascii_lowercase().as_str() {
        "deny" => Some(ClipboardAccess::Deny),
        "ask" => Some(ClipboardAccess::Ask),
        "allow" => Some(ClipboardAccess::Allow),
        _ => None,
    }
}

impl_config_deserialize!(Terminal {
    osc52,
    paste_protection,
    osc_notifications,
    progress,
    notify_on_command_finish,
    notify_on_command_finish_action,
    notify_on_command_finish_after,
    shell: option
});
impl_config_deserialize!(NotifyOnCommandFinishAction { bell, notify });
impl_config_deserialize_enum!(NotifyOnCommandFinish { Never, Unfocused, Always });
impl_serde_replace!(SerdeOsc52);

#[cfg(test)]
mod tests {
    use super::*;

    use ClipboardAccess::{Allow, Ask, Deny};

    fn osc52(toml: &str) -> Result<Osc52, toml::de::Error> {
        toml::from_str::<Terminal>(toml).map(|terminal| terminal.osc52.0)
    }

    #[test]
    fn osc_notifications_are_enabled_by_default_and_can_be_disabled() {
        assert!(toml::from_str::<Terminal>("").unwrap().osc_notifications);
        assert!(
            !toml::from_str::<Terminal>("osc_notifications = false").unwrap().osc_notifications
        );
    }

    #[test]
    fn paste_protection_is_on_by_default_and_can_be_disabled() {
        assert!(toml::from_str::<Terminal>("").unwrap().paste_protection);
        assert!(!toml::from_str::<Terminal>("paste_protection = false").unwrap().paste_protection);
    }

    #[test]
    fn osc52_reads_a_table_of_policies_filling_in_missing_keys() {
        assert_eq!(osc52("").unwrap(), Osc52 { read: Ask, write: Allow });
        assert_eq!(
            osc52("osc52 = { read = \"Allow\" }").unwrap(),
            Osc52 { read: Allow, write: Allow }
        );
        assert_eq!(
            osc52("[osc52]\nread = \"deny\"\nwrite = \"ask\"").unwrap(),
            Osc52 { read: Deny, write: Ask }
        );
    }

    #[test]
    fn osc52_keeps_the_single_word_modes() {
        for (mode, read, write) in [
            ("disabled", Deny, Deny),
            ("OnlyCopy", Deny, Allow),
            ("onlypaste", Allow, Deny),
            ("copypaste", Allow, Allow),
        ] {
            assert_eq!(osc52(&format!("osc52 = \"{mode}\"")).unwrap(), Osc52 { read, write });
        }
    }

    #[test]
    fn osc52_rejects_unknown_modes_keys_and_policies() {
        for value in ["\"sometimes\"", "{ reed = \"ask\" }", "{ read = \"maybe\" }", "true"] {
            let table = toml::from_str::<toml::Table>(&format!("osc52 = {value}")).unwrap();
            assert!(SerdeOsc52::deserialize(table["osc52"].clone()).is_err(), "{value}");
        }
    }

    #[test]
    fn command_finish_signals_nothing_by_default() {
        let terminal = toml::from_str::<Terminal>("").unwrap();
        assert_eq!(terminal.notify_on_command_finish, super::NotifyOnCommandFinish::Never);
        assert_eq!(
            terminal.notify_on_command_finish_action,
            super::NotifyOnCommandFinishAction { bell: true, notify: false }
        );
        assert_eq!(terminal.notify_on_command_finish_after(), std::time::Duration::from_secs(5));
    }

    #[test]
    fn command_finish_config_parses_modes_actions_and_threshold() {
        let terminal = toml::from_str::<Terminal>(
            "notify_on_command_finish = \"unfocused\"\n\
             notify_on_command_finish_action = { bell = false, notify = true }\n\
             notify_on_command_finish_after = 30",
        )
        .unwrap();
        assert_eq!(terminal.notify_on_command_finish, super::NotifyOnCommandFinish::Unfocused);
        assert_eq!(
            terminal.notify_on_command_finish_action,
            super::NotifyOnCommandFinishAction { bell: false, notify: true }
        );
        assert_eq!(terminal.notify_on_command_finish_after(), std::time::Duration::from_secs(30));
    }

    #[test]
    fn command_finish_action_keeps_defaults_for_missing_keys() {
        let terminal =
            toml::from_str::<Terminal>("notify_on_command_finish_action = { notify = true }")
                .unwrap();
        assert_eq!(
            terminal.notify_on_command_finish_action,
            super::NotifyOnCommandFinishAction { bell: true, notify: true }
        );
    }

    #[test]
    fn command_finish_actions_follow_mode_focus_and_threshold() {
        use std::time::Duration;

        use super::{NotifyOnCommandFinish, NotifyOnCommandFinishAction};

        let terminal = toml::from_str::<Terminal>(
            "notify_on_command_finish = \"unfocused\"\n\
             notify_on_command_finish_action = { bell = true, notify = true }",
        )
        .unwrap();
        let both = NotifyOnCommandFinishAction { bell: true, notify: true };
        let neither = NotifyOnCommandFinishAction { bell: false, notify: false };

        assert_eq!(terminal.command_finish_actions(false, Some(Duration::from_secs(6))), both);
        assert_eq!(terminal.command_finish_actions(true, Some(Duration::from_secs(6))), neither);
        // The runtime must exceed the threshold; exactly five seconds is not slow enough.
        assert_eq!(terminal.command_finish_actions(false, Some(Duration::from_secs(5))), neither);
        assert_eq!(terminal.command_finish_actions(false, Some(Duration::from_secs(4))), neither);
        // No measured start means no runtime to compare, so nothing signals.
        assert_eq!(terminal.command_finish_actions(false, None), neither);

        let mut always = terminal.clone();
        always.notify_on_command_finish = NotifyOnCommandFinish::Always;
        assert_eq!(always.command_finish_actions(true, Some(Duration::from_secs(6))), both);

        let mut never = terminal.clone();
        never.notify_on_command_finish = NotifyOnCommandFinish::Never;
        assert_eq!(never.command_finish_actions(false, Some(Duration::from_secs(60))), neither);
    }
}
