//! Confirmation before clipboard text crosses between the user and a terminal program.
//!
//! Three requests ask first: pasting text that would run commands in an application which has
//! not enabled bracketed paste ([`is_unsafe_paste`]), and, under an `ask` OSC 52 policy, a program
//! reading or replacing the clipboard. A [`ClipboardPrompt`] holds one such request until the user
//! answers. The display draws it over the top of the grid, like the command palette, so it works
//! the same in every window, Vivida pane, and platform; while it is open it takes every key and
//! nothing is typed or pasted into the terminal.

use std::fmt;
use std::sync::Arc;

use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::terminal::term::ClipboardType;

/// Most lines of the text shown; the rest are counted.
pub const PREVIEW_LINES: usize = 6;

/// Longest preview line kept, in characters; the display cuts it again to the prompt's width.
const PREVIEW_LINE_CHARS: usize = 256;

/// Builds the OSC 52 reply that carries the clipboard text.
pub type ReadReply = Arc<dyn Fn(&str) -> String + Sync + Send + 'static>;

/// What is waiting for the user's answer.
pub enum Request {
    /// The user pasted text that would run commands.
    Paste(String),
    /// A program asked for the clipboard's contents, which are `text`.
    Read { clipboard: ClipboardType, text: String, reply: ReadReply },
    /// A program asked to replace the clipboard's contents with `text`.
    Write { clipboard: ClipboardType, text: String },
}

impl fmt::Debug for Request {
    /// Names the request without its text, which can be a password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Request::Paste(_) => f.write_str("Paste"),
            Request::Read { clipboard, .. } => write!(f, "Read({clipboard:?})"),
            Request::Write { clipboard, .. } => write!(f, "Write({clipboard:?})"),
        }
    }
}

/// Whether pasting `text` into an application without bracketed paste could run a command.
///
/// Such an application cannot tell pasted text from typing, so a line break presses Enter and any
/// other control character acts as the key that produces it, such as `Ctrl+O`, which runs the line
/// in bash. A tab only completes.
pub fn is_unsafe_paste(text: &str) -> bool {
    text.chars().any(|character| character.is_control() && character != '\t')
}

/// What a key press does to the open prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAnswer {
    /// Answer the request: `true` pastes or allows, `false` cancels or denies.
    Answer(bool),
    /// Move the highlight to the other choice.
    SwitchFocus,
    Ignore,
}

/// A clipboard request waiting for the user, with what the display shows about it.
///
/// It has no `Debug`, which would print the text.
pub struct ClipboardPrompt {
    request: Request,
    /// Whether the confirming choice is highlighted; the refusing one starts highlighted, so a
    /// reflexive Enter never pastes.
    confirm_focused: bool,
    /// The text's first lines, with control characters made visible.
    preview: Vec<String>,
    /// Lines in the text; a final line break does not start another.
    lines: usize,
    /// Line breaks in the text, a CR LF pair counting once.
    line_breaks: usize,
    /// Control characters other than line breaks and tabs.
    controls: usize,
}

impl ClipboardPrompt {
    pub fn new(request: Request) -> Self {
        let text = match &request {
            Request::Paste(text) | Request::Read { text, .. } | Request::Write { text, .. } => text,
        };

        let mut preview = Vec::new();
        let mut current = String::new();
        let mut current_chars = 0;
        let mut at_line_start = true;
        let (mut lines, mut line_breaks, mut controls) = (0, 0, 0);
        let mut characters = text.chars().peekable();
        while let Some(character) = characters.next() {
            if character == '\n' || character == '\r' {
                // Pasting turns CR LF into one Enter, so it is one break here too.
                if character == '\r' && characters.peek() == Some(&'\n') {
                    characters.next();
                }
                line_breaks += 1;
                lines += 1;
                if preview.len() < PREVIEW_LINES {
                    preview.push(std::mem::take(&mut current));
                }
                current_chars = 0;
                at_line_start = true;
                continue;
            }

            at_line_start = false;
            if character.is_control() && character != '\t' {
                controls += 1;
            }
            if preview.len() < PREVIEW_LINES && current_chars < PREVIEW_LINE_CHARS {
                push_visible(&mut current, character);
                current_chars += 1;
            }
        }
        if !at_line_start {
            lines += 1;
            if preview.len() < PREVIEW_LINES {
                preview.push(current);
            }
        }

        Self { request, confirm_focused: false, preview, lines, line_breaks, controls }
    }

    pub fn into_request(self) -> Request {
        self.request
    }

    /// The request's kind as automation reports it.
    pub fn kind(&self) -> &'static str {
        match self.request {
            Request::Paste(_) => "paste",
            Request::Read { .. } => "read",
            Request::Write { .. } => "write",
        }
    }

    /// The question, one line.
    pub fn title(&self) -> String {
        match &self.request {
            Request::Paste(_) => String::from("Paste text that could run commands?"),
            Request::Read { clipboard, .. } => {
                format!("Let a program read the {}?", clipboard_name(*clipboard))
            },
            Request::Write { clipboard, .. } => {
                format!("Let a program replace the {}?", clipboard_name(*clipboard))
            },
        }
    }

    /// Why it is asked, one line.
    pub fn detail(&self) -> String {
        match &self.request {
            Request::Paste(_) => {
                let breaks = count(self.line_breaks, "line break", "line breaks");
                let controls = count(self.controls, "control character", "control characters");
                let sentence = match (self.line_breaks, self.controls) {
                    (_, 0) => format!("{breaks} would press Enter."),
                    (0, _) => format!("{controls} would act as keystrokes."),
                    _ => format!("{breaks} would press Enter; {controls} would act as keystrokes."),
                };
                let mut characters = sentence.chars();
                characters.next().map_or_else(String::new, |first| {
                    first.to_uppercase().chain(characters).collect()
                })
            },
            Request::Read { clipboard, .. } if self.lines == 0 => {
                format!("The {} is empty.", clipboard_name(*clipboard))
            },
            Request::Read { .. } => String::from("The program would receive this text:"),
            Request::Write { clipboard, .. } if self.lines == 0 => {
                format!("The program would empty the {}.", clipboard_name(*clipboard))
            },
            Request::Write { .. } => String::from("The program would copy this text:"),
        }
    }

    /// The text's first lines, control characters shown as their symbols.
    pub fn preview(&self) -> &[String] {
        &self.preview
    }

    /// Lines of the text that the preview leaves out.
    pub fn hidden_lines(&self) -> usize {
        self.lines.saturating_sub(self.preview.len())
    }

    /// The confirming and refusing choices' labels.
    pub fn choices(&self) -> (&'static str, &'static str) {
        match self.request {
            Request::Paste(_) => ("Paste", "Cancel"),
            Request::Read { .. } | Request::Write { .. } => ("Allow", "Deny"),
        }
    }

    pub fn confirm_focused(&self) -> bool {
        self.confirm_focused
    }

    pub fn switch_focus(&mut self) {
        self.confirm_focused = !self.confirm_focused;
    }

    /// Decide what a key press does.
    ///
    /// `Enter` takes the highlighted choice and `Tab` or the arrows move the highlight; each
    /// choice's initial, `y`, and `n` pick directly, and `Escape` or `Ctrl+C` refuse. Other
    /// `Control` or `Super` chords are ignored; `Ctrl+Alt` is AltGr on Windows and still types.
    pub fn answer_for_key(&self, key: &Key, text: &str, mods: ModifiersState) -> KeyAnswer {
        let control = mods.control_key() && !mods.alt_key();
        match key {
            Key::Named(NamedKey::Escape) => KeyAnswer::Answer(false),
            Key::Named(NamedKey::Enter) => KeyAnswer::Answer(self.confirm_focused),
            Key::Named(NamedKey::Tab | NamedKey::ArrowLeft | NamedKey::ArrowRight) => {
                KeyAnswer::SwitchFocus
            },
            Key::Character(character) if control && character.eq_ignore_ascii_case("c") => {
                KeyAnswer::Answer(false)
            },
            _ if control || mods.super_key() => KeyAnswer::Ignore,
            _ => {
                let (confirm, refuse) = self.choices();
                let initial = |label: &str| label[..1].to_ascii_lowercase();
                match text.to_lowercase().as_str() {
                    typed if typed == "y" || typed == initial(confirm) => KeyAnswer::Answer(true),
                    typed if typed == "n" || typed == initial(refuse) => KeyAnswer::Answer(false),
                    _ => KeyAnswer::Ignore,
                }
            },
        }
    }
}

fn clipboard_name(clipboard: ClipboardType) -> &'static str {
    match clipboard {
        ClipboardType::Clipboard => "clipboard",
        ClipboardType::Selection => "primary selection",
    }
}

/// `n` things, spelled `a line break` for one.
fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("a {one}") } else { format!("{n} {many}") }
}

/// Append `character` so that it shows exactly what it is.
///
/// Control characters become their Unicode control pictures (`␛` for Escape) and a tab becomes
/// spaces. Directional formatting characters, which could reorder the preview so it reads as
/// something harmless, become `�`.
fn push_visible(line: &mut String, character: char) {
    match character {
        '\t' => line.push_str("    "),
        '\u{0}'..='\u{1f}' => {
            line.push(char::from_u32(0x2400 + character as u32).unwrap_or('\u{fffd}'));
        },
        '\u{7f}' => line.push('\u{2421}'),
        '\u{80}'..='\u{9f}'
        | '\u{61c}'
        | '\u{200e}'
        | '\u{200f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2066}'..='\u{2069}' => line.push('\u{fffd}'),
        _ => line.push(character),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paste(text: &str) -> ClipboardPrompt {
        ClipboardPrompt::new(Request::Paste(text.to_owned()))
    }

    #[test]
    fn only_text_that_could_run_a_command_is_unsafe() {
        assert!(!is_unsafe_paste("git status"));
        assert!(!is_unsafe_paste("a\tb 今 🦀"));
        assert!(!is_unsafe_paste(""));
        for unsafe_text in ["ls\n", "ls\r", "rm -rf ~\u{f}", "\u{1b}[201~ls", "a\u{7f}", "\u{9b}"] {
            assert!(is_unsafe_paste(unsafe_text), "{unsafe_text:?}");
        }
    }

    #[test]
    fn the_preview_shows_control_characters_and_counts_what_it_leaves_out() {
        let prompt = paste("echo hi\r\nrm\u{1b}[2J\tx\u{7f}\n\u{202e}gnp.exe\n");
        assert_eq!(prompt.preview(), ["echo hi", "rm␛[2J    x␡", "\u{fffd}gnp.exe"]);
        assert_eq!(prompt.hidden_lines(), 0);
        assert_eq!(
            prompt.detail(),
            "3 line breaks would press Enter; 2 control characters would act as keystrokes."
        );

        let long = (0..20).map(|line| format!("line {line}\n")).collect::<String>();
        let prompt = paste(&long);
        assert_eq!(prompt.preview().len(), PREVIEW_LINES);
        assert_eq!(prompt.hidden_lines(), 20 - PREVIEW_LINES);

        let prompt = paste(&"x".repeat(PREVIEW_LINE_CHARS * 4));
        assert_eq!(prompt.preview()[0].len(), PREVIEW_LINE_CHARS);
    }

    #[test]
    fn requests_are_phrased_for_what_they_would_do() {
        assert_eq!(paste("ls\n").detail(), "A line break would press Enter.");
        assert_eq!(paste("ls\u{f}").detail(), "A control character would act as keystrokes.");

        let read = ClipboardPrompt::new(Request::Read {
            clipboard: ClipboardType::Clipboard,
            text: String::new(),
            reply: Arc::new(|text| text.to_owned()),
        });
        assert_eq!(read.title(), "Let a program read the clipboard?");
        assert_eq!(read.detail(), "The clipboard is empty.");
        assert!(read.preview().is_empty());
        assert_eq!(read.choices(), ("Allow", "Deny"));

        let write = ClipboardPrompt::new(Request::Write {
            clipboard: ClipboardType::Selection,
            text: String::from("token"),
        });
        assert_eq!(write.title(), "Let a program replace the primary selection?");
        assert_eq!(write.preview(), ["token"]);
        assert_eq!(format!("{:?}", write.into_request()), "Write(Selection)", "no text in logs");
    }

    #[test]
    fn enter_takes_the_highlighted_choice_which_starts_on_refusing() {
        let mut prompt = paste("ls\n");
        let none = ModifiersState::empty();
        let enter = Key::Named(NamedKey::Enter);
        assert_eq!(prompt.answer_for_key(&enter, "\r", none), KeyAnswer::Answer(false));

        let tab = Key::Named(NamedKey::Tab);
        assert_eq!(prompt.answer_for_key(&tab, "\t", none), KeyAnswer::SwitchFocus);
        prompt.switch_focus();
        assert_eq!(prompt.answer_for_key(&enter, "\r", none), KeyAnswer::Answer(true));
    }

    #[test]
    fn letters_pick_a_choice_directly_and_chords_do_not() {
        let prompt = paste("ls\n");
        let none = ModifiersState::empty();
        let character = |text: &str| Key::Character(text.into());
        assert_eq!(prompt.answer_for_key(&character("P"), "P", none), KeyAnswer::Answer(true));
        assert_eq!(prompt.answer_for_key(&character("y"), "y", none), KeyAnswer::Answer(true));
        assert_eq!(prompt.answer_for_key(&character("c"), "c", none), KeyAnswer::Answer(false));
        assert_eq!(prompt.answer_for_key(&character("x"), "x", none), KeyAnswer::Ignore);
        assert_eq!(
            prompt.answer_for_key(&character("v"), "v", ModifiersState::CONTROL),
            KeyAnswer::Ignore,
            "a second Ctrl+V does not paste through the prompt"
        );
        assert_eq!(
            prompt.answer_for_key(&character("c"), "c", ModifiersState::CONTROL),
            KeyAnswer::Answer(false)
        );
        assert_eq!(
            prompt.answer_for_key(&Key::Named(NamedKey::Escape), "\u{1b}", none),
            KeyAnswer::Answer(false)
        );

        let read = ClipboardPrompt::new(Request::Write {
            clipboard: ClipboardType::Clipboard,
            text: String::from("x"),
        });
        assert_eq!(read.answer_for_key(&character("a"), "a", none), KeyAnswer::Answer(true));
        assert_eq!(read.answer_for_key(&character("d"), "d", none), KeyAnswer::Answer(false));
    }
}
