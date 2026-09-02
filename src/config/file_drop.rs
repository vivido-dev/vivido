use serde::Serialize;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct FileDrop {
    /// After a remote receiver commits a dropped or pasted file, type its committed absolute
    /// remote path into the terminal, the way a local drag types a local path.
    pub paste_remote_path: bool,
}

impl Default for FileDrop {
    fn default() -> Self {
        Self { paste_remote_path: true }
    }
}

impl_config_deserialize!(FileDrop { paste_remote_path });

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_remote_path_paste_is_enabled_by_default_and_can_be_disabled() {
        assert!(toml::from_str::<FileDrop>("").unwrap().paste_remote_path);
        assert!(
            !toml::from_str::<FileDrop>("paste_remote_path = false").unwrap().paste_remote_path
        );
    }
}
