use serde::Serialize;

use crate::terminal::term::SEMANTIC_ESCAPE_CHARS;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Characters which end a word for double-click selection.
    pub semantic_escape_chars: String,
    pub save_to_clipboard: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Self { semantic_escape_chars: SEMANTIC_ESCAPE_CHARS.to_owned(), save_to_clipboard: false }
    }
}

impl_config_deserialize!(Selection { semantic_escape_chars, save_to_clipboard });

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_boundaries_default_to_the_terminal_set_and_can_be_replaced() {
        assert_eq!(toml::from_str::<Selection>("").unwrap(), Selection::default());
        let selection = toml::from_str::<Selection>("semantic_escape_chars = \" /\"").unwrap();
        assert_eq!(selection.semantic_escape_chars, " /");
    }
}
