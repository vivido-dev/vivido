use serde::{Deserialize, Deserializer, Serialize, de};
use toml::Value;

use crate::config::ui_config::{Program, StringVisitor};
use crate::terminal::term::Osc52;

#[derive(Serialize, Default, Clone, Debug, PartialEq)]
pub struct Terminal {
    /// OSC52 support mode.
    pub osc52: SerdeOsc52,
    /// Path to a shell program to run on startup.
    pub shell: Option<Program>,
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

impl_config_deserialize!(Terminal { osc52, shell: option });
impl_serde_replace!(SerdeOsc52);
