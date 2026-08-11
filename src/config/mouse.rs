use serde::{Deserialize, Deserializer, Serialize};

use crate::config::bindings::{self, MouseBinding};
use crate::config::ui_config;

#[derive(Serialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct Mouse {
    pub hide_when_typing: bool,
    #[serde(skip_serializing)]
    pub bindings: MouseBindings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MouseBindings(pub Vec<MouseBinding>);

impl Default for MouseBindings {
    fn default() -> Self {
        Self(bindings::default_mouse_bindings())
    }
}

impl<'de> Deserialize<'de> for MouseBindings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(ui_config::deserialize_bindings(deserializer, Self::default().0)?))
    }
}

impl_config_deserialize!(Mouse { hide_when_typing, bindings });
impl_serde_replace!(MouseBindings);
