use serde::Serialize;

#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub save_to_clipboard: bool,
}

impl_config_deserialize!(Selection { save_to_clipboard });
