//! A searchable list of every action Vivido can run, opened with `ToggleCommandPalette`.
//!
//! [`entries`] builds the catalog from the built-in actions, the configured command bindings,
//! and the bound hints, each labelled with the shortcut that currently triggers it. A
//! [`CommandPalette`] filters that catalog as the user types and tracks the highlighted entry;
//! the display draws it over the top of the grid and the input processor runs the chosen action.

use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::config::ui_config::HintAction;
use crate::config::{Action, BindingKey, BindingMode, KeyBinding, UiConfig};

/// Most entries listed at once; more scroll with the selection.
pub const VISIBLE_ENTRIES: usize = 10;

/// Longest query kept; a palette query is a few words, never a paste of a file.
const MAX_QUERY_CHARS: usize = 256;

/// One runnable action and how to reach it without the palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub title: String,
    pub action: Action,
    /// The first key binding that runs this action, formatted for this platform.
    pub shortcut: Option<String>,
}

/// Every action worth offering, in the order an empty query lists them.
pub fn entries(config: &UiConfig) -> Vec<Entry> {
    let bindings = config.key_bindings();
    let shortcut = |action: &Action| preferred_shortcut(bindings, action);

    let mut entries = built_in_actions()
        .into_iter()
        .map(|(title, action)| Entry {
            title: title.to_owned(),
            shortcut: shortcut(&action),
            action,
        })
        .collect::<Vec<_>>();

    for hint in &config.hints.enabled {
        let Some(binding) = hint.binding.as_ref() else { continue };
        let binding = binding.key_binding(hint);
        let title = match &hint.action {
            HintAction::Command(program) => {
                format!("Hint: open a match with {}", program.program())
            },
            HintAction::Action(action) => {
                format!("Hint: {} a match", format!("{action:?}").to_lowercase())
            },
        };
        entries.push(Entry {
            title,
            shortcut: format_shortcut(binding.mods, &binding.trigger),
            action: binding.action.clone(),
        });
    }

    for binding in bindings {
        if let Action::Command(program) = &binding.action {
            let mut title = format!("Run {}", program.program());
            for argument in program.args() {
                title.push(' ');
                title.push_str(argument);
            }
            // The same command bound twice is still one command.
            if entries.iter().any(|entry| entry.action == binding.action) {
                continue;
            }
            entries.push(Entry {
                title,
                shortcut: format_shortcut(binding.mods, &binding.trigger),
                action: binding.action.clone(),
            });
        }
    }

    entries
}

/// The built-in actions this platform can run, with their palette titles.
fn built_in_actions() -> Vec<(&'static str, Action)> {
    let mut actions = vec![
        ("Copy selection", Action::Copy),
        ("Paste", Action::Paste),
        ("Search forward", Action::SearchForward),
        ("Search backward", Action::SearchBackward),
        ("Clear selection", Action::ClearSelection),
        ("New tab", Action::CreateNewTab),
        ("New window", Action::CreateNewWindow),
        ("Next tab", Action::SelectNextTab),
        ("Previous tab", Action::SelectPreviousTab),
        ("Close terminal", Action::Quit),
        ("Increase font size", Action::IncreaseFontSize),
        ("Decrease font size", Action::DecreaseFontSize),
        ("Reset font size", Action::ResetFontSize),
        ("Scroll to top", Action::ScrollToTop),
        ("Scroll to bottom", Action::ScrollToBottom),
        ("Scroll page up", Action::ScrollPageUp),
        ("Scroll page down", Action::ScrollPageDown),
        ("Clear scrollback", Action::ClearHistory),
        ("Toggle fullscreen", Action::ToggleFullscreen),
        ("Toggle maximized", Action::ToggleMaximized),
        ("Minimize window", Action::Minimize),
        ("Hide window", Action::Hide),
        ("Dismiss notices", Action::ClearLogNotice),
        ("Recover terminal", Action::TerminalRecovery),
        ("Toggle remote microphone", Action::ToggleMicrophone),
        ("Next remote microphone", Action::NextMicrophone),
        ("Open a new Vivido instance", Action::SpawnNewInstance),
        ("Check for updates", Action::CheckForUpdates),
    ];
    if cfg!(target_os = "linux") {
        actions.insert(2, ("Paste primary selection", Action::PasteSelection));
    }
    if cfg!(target_os = "macos") {
        actions.push(("Hide other applications", Action::HideOtherApplications));
        actions.push(("Toggle simple fullscreen", Action::ToggleSimpleFullscreen));
    }
    actions
}

/// The shortcut to show for `action`: its first binding usable outside search mode, preferring a
/// chord over a dedicated Copy, Cut, or Paste key, which few keyboards have.
fn preferred_shortcut(bindings: &[KeyBinding], action: &Action) -> Option<String> {
    let usable = || {
        bindings.iter().filter(|binding| {
            binding.action == *action && !binding.mode.intersects(BindingMode::SEARCH)
        })
    };
    usable()
        .find(|binding| !is_dedicated_edit_key(&binding.trigger))
        .or_else(|| usable().next())
        .and_then(|binding| format_shortcut(binding.mods, &binding.trigger))
}

/// Whether a binding uses a dedicated Copy, Cut, or Paste key rather than a chord.
fn is_dedicated_edit_key(trigger: &BindingKey) -> bool {
    matches!(
        trigger,
        BindingKey::Keycode {
            key: Key::Named(NamedKey::Copy | NamedKey::Cut | NamedKey::Paste),
            ..
        }
    )
}

/// Format a key binding the way this platform writes shortcuts, or `None` for a scancode.
pub fn format_shortcut(mods: ModifiersState, trigger: &BindingKey) -> Option<String> {
    let BindingKey::Keycode { key, .. } = trigger else { return None };
    let key = match key {
        Key::Character(character) => character.to_uppercase(),
        Key::Named(NamedKey::ArrowUp) => String::from("Up"),
        Key::Named(NamedKey::ArrowDown) => String::from("Down"),
        Key::Named(NamedKey::ArrowLeft) => String::from("Left"),
        Key::Named(NamedKey::ArrowRight) => String::from("Right"),
        Key::Named(named) => format!("{named:?}"),
        _ => return None,
    };

    let mut text = String::new();
    if cfg!(target_os = "macos") {
        for (modifier, symbol) in [
            (ModifiersState::CONTROL, "⌃"),
            (ModifiersState::ALT, "⌥"),
            (ModifiersState::SHIFT, "⇧"),
            (ModifiersState::SUPER, "⌘"),
        ] {
            if mods.contains(modifier) {
                text.push_str(symbol);
            }
        }
    } else {
        let super_name = if cfg!(windows) { "Win" } else { "Super" };
        for (modifier, name) in [
            (ModifiersState::CONTROL, "Ctrl"),
            (ModifiersState::ALT, "Alt"),
            (ModifiersState::SHIFT, "Shift"),
            (ModifiersState::SUPER, super_name),
        ] {
            if mods.contains(modifier) {
                text.push_str(name);
                text.push('+');
            }
        }
    }
    text.push_str(&key);
    Some(text)
}

/// The palette while it is open: its catalog, the query, and the highlighted match.
#[derive(Clone, Debug)]
pub struct CommandPalette {
    entries: Vec<Entry>,
    query: String,
    /// Indices into `entries` that match the query, best first.
    matches: Vec<usize>,
    /// Position of the highlighted entry within `matches`.
    selected: usize,
    /// First visible position within `matches`.
    scroll: usize,
}

impl CommandPalette {
    pub fn new(entries: Vec<Entry>) -> Self {
        let mut palette =
            Self { entries, query: String::new(), matches: Vec::new(), selected: 0, scroll: 0 };
        palette.refilter();
        palette
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Append typed text; control characters are dropped.
    pub fn insert(&mut self, text: &str) {
        let room = MAX_QUERY_CHARS.saturating_sub(self.query.chars().count());
        let before = self.query.len();
        self.query.extend(text.chars().filter(|character| !character.is_control()).take(room));
        if self.query.len() != before {
            self.refilter();
        }
    }

    /// Delete the last character.
    pub fn backspace(&mut self) {
        if self.query.pop().is_some() {
            self.refilter();
        }
    }

    /// Delete the last word and any spaces after it.
    pub fn delete_word(&mut self) {
        let trimmed = self.query.trim_end_matches(' ');
        let keep = trimmed.rfind(' ').map_or(0, |index| index + 1);
        if keep != self.query.len() {
            self.query.truncate(keep);
            self.refilter();
        }
    }

    /// Delete the whole query.
    pub fn clear(&mut self) {
        if !self.query.is_empty() {
            self.query.clear();
            self.refilter();
        }
    }

    /// Move the highlight by `delta` matches, wrapping at either end.
    pub fn move_selection(&mut self, delta: isize) {
        let count = self.matches.len();
        if count == 0 {
            return;
        }
        let count = count as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
        self.keep_selection_visible();
    }

    /// Move the highlight by one screenful, stopping at either end.
    pub fn page(&mut self, forward: bool) {
        let last = self.matches.len().saturating_sub(1);
        self.selected = if forward {
            (self.selected + VISIBLE_ENTRIES).min(last)
        } else {
            self.selected.saturating_sub(VISIBLE_ENTRIES)
        };
        self.keep_selection_visible();
    }

    /// The highlighted entry, if anything matches.
    pub fn selected(&self) -> Option<&Entry> {
        self.matches.get(self.selected).map(|&index| &self.entries[index])
    }

    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// The matches in view and whether each is highlighted, at most `rows` of them.
    pub fn visible(&self, rows: usize) -> impl Iterator<Item = (&Entry, bool)> {
        let rows = rows.min(VISIBLE_ENTRIES);
        let scroll = self.scroll.min(self.matches.len().saturating_sub(rows));
        self.matches
            .iter()
            .enumerate()
            .skip(scroll)
            .take(rows)
            .map(move |(position, &index)| (&self.entries[index], position == self.selected))
    }

    fn refilter(&mut self) {
        let query = self.query.to_lowercase();
        let mut scored = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                match_score(&query, &entry.title.to_lowercase()).map(|score| (score, index))
            })
            .collect::<Vec<_>>();
        // Best score first; catalog order breaks ties, so an empty query keeps it.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.matches = scored.into_iter().map(|(_, index)| index).collect();
        self.selected = 0;
        self.scroll = 0;
    }

    fn keep_selection_visible(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + VISIBLE_ENTRIES {
            self.scroll = self.selected + 1 - VISIBLE_ENTRIES;
        }
    }
}

/// Score how well `query` matches `title` (both lowercase), or `None` when it does not.
///
/// Every query word must appear in order as a subsequence of the title. Characters that start a
/// title word or continue a run score more, and an earlier first match scores more, so
/// `inc font` ranks "Increase font size" above titles that merely contain those letters.
fn match_score(query: &str, title: &str) -> Option<i64> {
    let title = title.chars().collect::<Vec<_>>();
    let mut score = 0_i64;
    let mut position = 0;
    let mut first = None;
    for word in query.split_whitespace() {
        let mut previous = None;
        for character in word.chars() {
            let found = title[position..].iter().position(|&candidate| candidate == character)?;
            let index = position + found;
            first.get_or_insert(index);
            score += 1;
            if index == 0 || title[index - 1] == ' ' {
                score += 8;
            }
            if previous.is_some_and(|previous| previous + 1 == index) {
                score += 5;
            }
            previous = Some(index);
            position = index + 1;
        }
    }
    Some(score * 100 - first.unwrap_or(0) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette(titles: &[&str]) -> CommandPalette {
        CommandPalette::new(
            titles
                .iter()
                .map(|title| Entry {
                    title: (*title).to_owned(),
                    action: Action::None,
                    shortcut: None,
                })
                .collect(),
        )
    }

    fn titles(palette: &CommandPalette) -> Vec<&str> {
        palette.visible(VISIBLE_ENTRIES).map(|(entry, _)| entry.title.as_str()).collect()
    }

    #[test]
    fn an_empty_query_lists_the_catalog_in_order() {
        let palette = palette(&["Copy selection", "Paste", "New tab"]);
        assert_eq!(titles(&palette), ["Copy selection", "Paste", "New tab"]);
        assert_eq!(palette.selected().unwrap().title, "Copy selection");
    }

    #[test]
    fn word_starts_outrank_scattered_letters() {
        let mut palette =
            palette(&["Scroll page down", "Increase font size", "Decrease font size", "Paste"]);
        palette.insert("inc font");
        assert_eq!(titles(&palette), ["Increase font size"]);

        palette.clear();
        palette.insert("fs");
        assert_eq!(
            titles(&palette)[..2],
            ["Increase font size", "Decrease font size"],
            "f and s both start words there"
        );
        palette.insert("zzz");
        assert!(palette.selected().is_none(), "nothing matches");
    }

    #[test]
    fn editing_the_query_refilters_and_resets_the_selection() {
        let mut palette = palette(&["New tab", "New window", "Next tab"]);
        palette.insert("new");
        palette.move_selection(1);
        assert_eq!(palette.selected().unwrap().title, "New window");

        palette.insert(" w");
        assert_eq!(titles(&palette), ["New window"]);
        assert_eq!(palette.selected().unwrap().title, "New window");

        palette.delete_word();
        assert_eq!(palette.query(), "new ");
        palette.backspace();
        palette.backspace();
        assert_eq!(palette.query(), "ne");
        assert_eq!(palette.match_count(), 3);
    }

    #[test]
    fn the_selection_wraps_and_scrolls_into_view() {
        let names = (0..25).map(|index| format!("Entry {index:02}")).collect::<Vec<_>>();
        let names = names.iter().map(String::as_str).collect::<Vec<_>>();
        let mut palette = palette(&names);

        palette.move_selection(-1);
        assert_eq!(palette.selected().unwrap().title, "Entry 24", "up from the top wraps");
        assert_eq!(titles(&palette).last(), Some(&"Entry 24"), "and scrolls to show it");
        assert_eq!(palette.visible(VISIBLE_ENTRIES).filter(|(_, selected)| *selected).count(), 1);

        palette.move_selection(1);
        assert_eq!(palette.selected().unwrap().title, "Entry 00");
        palette.page(true);
        assert_eq!(palette.selected().unwrap().title, "Entry 10");
        palette.page(true);
        palette.page(true);
        assert_eq!(palette.selected().unwrap().title, "Entry 24", "paging stops at the end");
    }

    #[test]
    fn typed_control_characters_are_dropped_and_the_query_is_bounded() {
        let mut palette = palette(&["Paste"]);
        palette.insert("pa\u{1b}[2J\nst");
        assert_eq!(palette.query(), "pa[2Jst");
        palette.clear();
        palette.insert(&"x".repeat(MAX_QUERY_CHARS * 2));
        assert_eq!(palette.query().chars().count(), MAX_QUERY_CHARS);
    }

    #[test]
    fn shortcuts_are_written_for_the_platform() {
        let trigger =
            |key: Key| BindingKey::Keycode { key, location: crate::config::KeyLocation::Any };
        let shift_p = format_shortcut(
            ModifiersState::CONTROL | ModifiersState::SHIFT,
            &trigger(Key::Character("p".into())),
        );
        let page_up =
            format_shortcut(ModifiersState::SHIFT, &trigger(Key::Named(NamedKey::PageUp)));
        if cfg!(target_os = "macos") {
            assert_eq!(shift_p.as_deref(), Some("⌃⇧P"));
            assert_eq!(page_up.as_deref(), Some("⇧PageUp"));
        } else {
            assert_eq!(shift_p.as_deref(), Some("Ctrl+Shift+P"));
            assert_eq!(page_up.as_deref(), Some("Shift+PageUp"));
        }
    }

    #[test]
    fn a_chord_is_shown_before_a_dedicated_key_or_a_search_only_binding() {
        let binding = |key: Key, mods: ModifiersState, mode: BindingMode| KeyBinding {
            trigger: BindingKey::Keycode { key, location: crate::config::KeyLocation::Any },
            mods,
            mode,
            notmode: BindingMode::empty(),
            action: Action::Copy,
        };
        let chord = ModifiersState::CONTROL | ModifiersState::SHIFT;
        let bindings = [
            binding(Key::Named(NamedKey::Copy), ModifiersState::empty(), BindingMode::empty()),
            binding(Key::Character("x".into()), chord, BindingMode::SEARCH),
            binding(Key::Character("c".into()), chord, BindingMode::empty()),
        ];
        let expected = format_shortcut(chord, &bindings[2].trigger);
        assert_eq!(preferred_shortcut(&bindings, &Action::Copy), expected);
        assert_eq!(
            preferred_shortcut(&bindings[..1], &Action::Copy).as_deref(),
            Some("Copy"),
            "the dedicated key is still better than nothing"
        );
        assert_eq!(preferred_shortcut(&bindings, &Action::Paste), None);
    }

    #[test]
    fn the_catalog_labels_actions_with_their_default_shortcuts() {
        let config = UiConfig::default();
        let entries = entries(&config);
        assert!(!entries.iter().any(|entry| entry.action == Action::ToggleCommandPalette));
        let recover =
            entries.iter().find(|entry| entry.action == Action::TerminalRecovery).unwrap();
        assert!(
            recover.shortcut.as_deref().is_some_and(|shortcut| shortcut.ends_with("F12")),
            "{recover:?}"
        );
        let mut titles = entries.iter().map(|entry| entry.title.as_str()).collect::<Vec<_>>();
        titles.sort_unstable();
        let count = titles.len();
        titles.dedup();
        assert_eq!(titles.len(), count, "every title is distinct");
    }
}
