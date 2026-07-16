use serde::Serialize;

use vivido_config_derive::ConfigDeserialize;
#[derive(ConfigDeserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub save_to_clipboard: bool,
}
