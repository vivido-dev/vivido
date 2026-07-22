use std::borrow::Cow;

use winit::event::{ElementState, KeyEvent};
#[cfg(target_os = "macos")]
use winit::keyboard::ModifiersKeyState;
use winit::keyboard::{Key, KeyLocation, ModifiersState, NamedKey};
#[cfg(target_os = "macos")]
use winit::platform::macos::OptionAsAlt;

use crate::terminal::event::EventListener;
use crate::terminal::term::TermMode;
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

use crate::config::{Action, BindingKey, BindingMode, KeyBinding};
use crate::event::TYPING_SEARCH_DELAY;
use crate::input::{ActionContext, Execute, Processor};
use crate::scheduler::{TimerId, Topic};

impl<T: EventListener, A: ActionContext<T>> Processor<T, A> {
    /// Process key input.
    pub fn key_input(&mut self, key: KeyEvent) {
        // IME input will be applied on commit and shouldn't trigger key bindings.
        if self.ctx.display().ime.preedit().is_some() {
            return;
        }

        let mode = *self.ctx.terminal().mode();
        let mods = self.ctx.modifiers().state();

        if key.state == ElementState::Released {
            self.key_release(key, mode, mods);
            return;
        }

        let text = key.text_with_all_modifiers().unwrap_or_default();

        // All key bindings are disabled while a hint is being selected.
        if self.ctx.display().hint_state.active() {
            for character in text.chars() {
                self.ctx.hint_input(character);
            }
            return;
        }

        // Reset search delay when the user is still typing.
        self.reset_search_delay();

        // Key bindings suppress the character input.
        if self.process_key_bindings(&key) {
            return;
        }

        if self.ctx.search_active() {
            for character in text.chars() {
                self.ctx.search_input(character);
            }

            return;
        }

        // Mask `Alt` modifier from input when we won't send esc.
        let mods = if self.alt_send_esc(&key, text) { mods } else { mods & !ModifiersState::ALT };

        let build_key_sequence = Self::should_build_sequence(&key, text, mode, mods);
        let is_modifier_key = Self::is_modifier_key(&key);

        let bytes = if build_key_sequence {
            build_sequence(key, mods, mode)
        } else {
            let mut bytes = Vec::with_capacity(text.len() + 1);
            if mods.alt_key() {
                bytes.push(b'\x1b');
            }

            bytes.extend_from_slice(text.as_bytes());
            bytes
        };

        // Write only if we have something to write.
        if !bytes.is_empty() {
            // Don't clear selection/scroll down when writing escaped modifier keys.
            if !is_modifier_key {
                self.ctx.on_terminal_input_start();
            }
            self.ctx.write_to_pty(bytes);
        }
    }

    fn alt_send_esc(&mut self, key: &KeyEvent, text: &str) -> bool {
        #[cfg(not(target_os = "macos"))]
        let alt_send_esc = self.ctx.modifiers().state().alt_key();

        #[cfg(target_os = "macos")]
        let alt_send_esc = {
            let option_as_alt = self.ctx.config().window.option_as_alt();
            self.ctx.modifiers().state().alt_key()
                && (option_as_alt == OptionAsAlt::Both
                    || (option_as_alt == OptionAsAlt::OnlyLeft
                        && self.ctx.modifiers().lalt_state() == ModifiersKeyState::Pressed)
                    || (option_as_alt == OptionAsAlt::OnlyRight
                        && self.ctx.modifiers().ralt_state() == ModifiersKeyState::Pressed))
        };

        match key.logical_key {
            Key::Named(named) => {
                if named.to_text().is_some() {
                    alt_send_esc
                } else {
                    // Treat `Alt` as modifier for named keys without text, like ArrowUp.
                    self.ctx.modifiers().state().alt_key()
                }
            },
            _ => alt_send_esc && text.chars().count() == 1,
        }
    }

    fn is_modifier_key(key: &KeyEvent) -> bool {
        matches!(
            key.logical_key.as_ref(),
            Key::Named(NamedKey::Shift)
                | Key::Named(NamedKey::Control)
                | Key::Named(NamedKey::Alt)
                | Key::Named(NamedKey::Super)
        )
    }

    /// Check whether we should try to build escape sequence for the [`KeyEvent`].
    fn should_build_sequence(
        key: &KeyEvent,
        text: &str,
        mode: TermMode,
        mods: ModifiersState,
    ) -> bool {
        if mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
            return true;
        }

        let disambiguate = mode.contains(TermMode::DISAMBIGUATE_ESC_CODES)
            && (key.logical_key == Key::Named(NamedKey::Escape)
                || key.location == KeyLocation::Numpad
                || (!mods.is_empty()
                    && (mods != ModifiersState::SHIFT
                        || matches!(
                            key.logical_key,
                            Key::Named(NamedKey::Tab)
                                | Key::Named(NamedKey::Enter)
                                | Key::Named(NamedKey::Backspace)
                        ))));

        match key.logical_key {
            _ if disambiguate => true,
            // Exclude all the named keys unless they have textual representation.
            Key::Named(named) => named.to_text().is_none(),
            _ => text.is_empty(),
        }
    }

    /// Attempt to find a binding and execute its action.
    ///
    /// The provided mode, mods, and key must match what is allowed by a binding
    /// for its action to be executed.
    fn process_key_bindings(&mut self, key: &KeyEvent) -> bool {
        let mode = BindingMode::new(self.ctx.terminal().mode(), self.ctx.search_active());
        let mods = self.ctx.modifiers().state();

        // Don't suppress char if no bindings were triggered.
        let mut suppress_chars = None;

        // We don't want the key without modifier, because it means something else most of
        // the time. However what we want is to manually lowercase the character to account
        // for both small and capital letters on regular characters at the same time.
        let logical_key = if let Key::Character(ch) = key.logical_key.as_ref() {
            // Match `Alt` bindings without `Alt` being applied, otherwise they use the
            // composed chars, which are not intuitive to bind.
            //
            // On Windows, the `Ctrl + Alt` mangles `logical_key` to unidentified values, thus
            // preventing them from being used in bindings
            //
            // For more see https://github.com/rust-windowing/winit/issues/2945.
            if (cfg!(target_os = "macos") || (cfg!(windows) && mods.control_key()))
                && mods.alt_key()
            {
                key.key_without_modifiers()
            } else {
                Key::Character(ch.to_lowercase().into())
            }
        } else {
            key.logical_key.clone()
        };

        // Get the action of a key binding.
        let mut binding_action = |binding: &KeyBinding| {
            let key = match (&binding.trigger, &logical_key) {
                (BindingKey::Scancode(_), _) => BindingKey::Scancode(key.physical_key),
                (_, code) => {
                    BindingKey::Keycode { key: code.clone(), location: key.location.into() }
                },
            };

            if binding.is_triggered_by(mode, mods, &key) {
                // Pass through the key if any of the bindings has the `ReceiveChar` action.
                *suppress_chars.get_or_insert(true) &= binding.action != Action::ReceiveChar;

                // Binding was triggered; run the action.
                Some(binding.action.clone())
            } else {
                None
            }
        };

        // Trigger matching key bindings.
        for i in 0..self.ctx.config().key_bindings().len() {
            let binding = &self.ctx.config().key_bindings()[i];
            if let Some(action) = binding_action(binding) {
                action.execute(&mut self.ctx);
            }
        }

        // Trigger key bindings for hints.
        for i in 0..self.ctx.config().hints.enabled.len() {
            let hint = &self.ctx.config().hints.enabled[i];
            let binding = match hint.binding.as_ref() {
                Some(binding) => binding.key_binding(hint),
                None => continue,
            };

            if let Some(action) = binding_action(binding) {
                action.execute(&mut self.ctx);
            }
        }

        suppress_chars.unwrap_or(false)
    }

    /// Handle key release.
    fn key_release(&mut self, key: KeyEvent, mode: TermMode, mods: ModifiersState) {
        if !mode.contains(TermMode::REPORT_EVENT_TYPES)
            || self.ctx.search_active()
            || self.ctx.display().hint_state.active()
        {
            return;
        }

        // Mask `Alt` modifier from input when we won't send esc.
        let text = key.text_with_all_modifiers().unwrap_or_default();
        let mods = if self.alt_send_esc(&key, text) { mods } else { mods & !ModifiersState::ALT };

        let bytes = match key.logical_key.as_ref() {
            Key::Named(NamedKey::Enter)
            | Key::Named(NamedKey::Tab)
            | Key::Named(NamedKey::Backspace)
                if !mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) =>
            {
                return;
            },
            _ => build_sequence(key, mods, mode),
        };

        self.ctx.write_to_pty(bytes);
    }

    /// Reset search delay.
    fn reset_search_delay(&mut self) {
        if self.ctx.search_active() {
            let timer_id = TimerId::new(Topic::DelayedSearch, self.ctx.window().id());
            let scheduler = self.ctx.scheduler_mut();
            if let Some(timer) = scheduler.unschedule(timer_id) {
                scheduler.schedule(timer.event, TYPING_SEARCH_DELAY, false, timer.id);
            }
        }
    }

    /// Process a neutral IPC key through Vivido bindings/search/hints without mutating the
    /// persistent physical modifier state. Bytes that fall through are returned for tagged PTY
    /// delivery by the automation layer.
    #[cfg(unix)]
    pub fn ipc_key_input(
        &mut self,
        key: &str,
        modifiers: &[String],
        repeated: bool,
    ) -> Result<Option<Vec<u8>>, crate::polling::ipc::IpcError> {
        let mods = ipc_modifier_state(modifiers)?;
        let (logical_key, location) = ipc_logical_key(key)?;
        let mode = *self.ctx.terminal().mode();
        let text = match &logical_key {
            Key::Character(text) => text.to_string(),
            Key::Named(named) => named.to_text().unwrap_or_default().to_owned(),
            _ => String::new(),
        };

        if self.ctx.display().hint_state.active() {
            for character in text.chars() {
                self.ctx.hint_input(character);
            }
            return Ok(None);
        }

        self.reset_search_delay();
        let binding_mode = BindingMode::new(self.ctx.terminal().mode(), self.ctx.search_active());
        let binding_key = match logical_key {
            Key::Character(character) => Key::Character(character.to_lowercase().into()),
            logical_key => logical_key,
        };
        let trigger = BindingKey::Keycode { key: binding_key, location: location.into() };
        let bindings = self.ctx.config().key_bindings().to_vec();
        let hint_bindings: Vec<_> = self
            .ctx
            .config()
            .hints
            .enabled
            .iter()
            .filter_map(|hint| {
                hint.binding.as_ref().map(|binding| binding.key_binding(hint).clone())
            })
            .collect();
        let mut suppress = None;
        for binding in bindings.into_iter().chain(hint_bindings) {
            if binding.is_triggered_by(binding_mode, mods, &trigger) {
                *suppress.get_or_insert(true) &= binding.action != Action::ReceiveChar;
                binding.action.execute(&mut self.ctx);
            }
        }
        if suppress == Some(true) {
            return Ok(None);
        }

        if self.ctx.search_active() {
            for character in text.chars() {
                self.ctx.search_input(character);
            }
            return Ok(None);
        }

        let bytes = encode_ipc_key_event(key, modifiers, mode, repeated)?;
        if !bytes.is_empty() {
            self.ctx.on_terminal_input_start();
        }
        Ok(Some(bytes))
    }
}

/// Build a key's keyboard escape sequence based on the given `key`, `mods`, and `mode`.
///
/// The key sequences for `APP_KEYPAD` and alike are handled inside the bindings.
#[inline(never)]
fn build_sequence(key: KeyEvent, mods: ModifiersState, mode: TermMode) -> Vec<u8> {
    let mut modifiers = mods.into();

    let kitty_seq = mode.intersects(
        TermMode::REPORT_ALL_KEYS_AS_ESC
            | TermMode::DISAMBIGUATE_ESC_CODES
            | TermMode::REPORT_EVENT_TYPES,
    );

    let kitty_encode_all = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    // The default parameter is 1, so we can omit it.
    let kitty_event_type = mode.contains(TermMode::REPORT_EVENT_TYPES)
        && (key.repeat || key.state == ElementState::Released);

    let context =
        SequenceBuilder { mode, modifiers, kitty_seq, kitty_encode_all, kitty_event_type };

    let associated_text = key.text_with_all_modifiers().filter(|text| {
        mode.contains(TermMode::REPORT_ASSOCIATED_TEXT)
            && key.state != ElementState::Released
            && !text.is_empty()
            && !is_control_character(text)
    });

    let sequence_base = context
        .try_build_numpad(&key)
        .or_else(|| context.try_build_named_kitty(&key))
        .or_else(|| context.try_build_named_normal(&key, associated_text.is_some()))
        .or_else(|| context.try_build_control_char_or_mod(&key, &mut modifiers))
        .or_else(|| context.try_build_textual(&key, associated_text));

    let (payload, terminator) = match sequence_base {
        Some(SequenceBase { payload, terminator }) => (payload, terminator),
        _ => return Vec::new(),
    };

    let mut payload = format!("\x1b[{payload}");

    // Add modifiers information.
    if kitty_event_type || !modifiers.is_empty() || associated_text.is_some() {
        payload.push_str(&format!(";{}", modifiers.encode_esc_sequence()));
    }

    // Push event type.
    if kitty_event_type {
        payload.push(':');
        let event_type = match key.state {
            _ if key.repeat => '2',
            ElementState::Pressed => '1',
            ElementState::Released => '3',
        };
        payload.push(event_type);
    }

    if let Some(text) = associated_text {
        let mut codepoints = text.chars().map(u32::from);
        if let Some(codepoint) = codepoints.next() {
            payload.push_str(&format!(";{codepoint}"));
        }
        for codepoint in codepoints {
            payload.push_str(&format!(":{codepoint}"));
        }
    }

    payload.push(terminator.encode_esc_sequence());

    payload.into_bytes()
}

/// Helper to build escape sequence payloads from [`KeyEvent`].
pub struct SequenceBuilder {
    mode: TermMode,
    /// The emitted sequence should follow the kitty keyboard protocol.
    kitty_seq: bool,
    /// Encode all the keys according to the protocol.
    kitty_encode_all: bool,
    /// Report event types.
    kitty_event_type: bool,
    modifiers: SequenceModifiers,
}

impl SequenceBuilder {
    /// Try building sequence from the event's emitting text.
    fn try_build_textual(
        &self,
        key: &KeyEvent,
        associated_text: Option<&str>,
    ) -> Option<SequenceBase> {
        let character = match key.logical_key.as_ref() {
            Key::Character(character) if self.kitty_seq => character,
            _ => return None,
        };

        if character.chars().count() == 1 {
            let shift = self.modifiers.contains(SequenceModifiers::SHIFT);

            let ch = character.chars().next().unwrap();
            let unshifted_ch = if shift { ch.to_lowercase().next().unwrap() } else { ch };

            let alternate_key_code = u32::from(ch);
            let mut unicode_key_code = u32::from(unshifted_ch);

            // Try to get the base for keys which change based on modifier, like `1` for `!`.
            //
            // However it should only be performed when `SHIFT` is pressed.
            if shift
                && alternate_key_code == unicode_key_code
                && let Key::Character(unmodded) = key.key_without_modifiers().as_ref()
            {
                unicode_key_code = u32::from(unmodded.chars().next().unwrap_or(unshifted_ch));
            }

            // NOTE: Base layouts are ignored, since winit doesn't expose this information
            // yet.
            let payload = if self.mode.contains(TermMode::REPORT_ALTERNATE_KEYS)
                && alternate_key_code != unicode_key_code
            {
                format!("{unicode_key_code}:{alternate_key_code}")
            } else {
                unicode_key_code.to_string()
            };

            Some(SequenceBase::new(payload.into(), SequenceTerminator::Kitty))
        } else if self.kitty_encode_all && associated_text.is_some() {
            // Fallback when need to report text, but we don't have any key associated with this
            // text.
            Some(SequenceBase::new("0".into(), SequenceTerminator::Kitty))
        } else {
            None
        }
    }

    /// Try building from numpad key.
    ///
    /// `None` is returned when the key is neither known nor numpad.
    fn try_build_numpad(&self, key: &KeyEvent) -> Option<SequenceBase> {
        if !self.kitty_seq || key.location != KeyLocation::Numpad {
            return None;
        }

        let base = match key.logical_key.as_ref() {
            Key::Character("0") => "57399",
            Key::Character("1") => "57400",
            Key::Character("2") => "57401",
            Key::Character("3") => "57402",
            Key::Character("4") => "57403",
            Key::Character("5") => "57404",
            Key::Character("6") => "57405",
            Key::Character("7") => "57406",
            Key::Character("8") => "57407",
            Key::Character("9") => "57408",
            Key::Character(".") => "57409",
            Key::Character("/") => "57410",
            Key::Character("*") => "57411",
            Key::Character("-") => "57412",
            Key::Character("+") => "57413",
            Key::Character("=") => "57415",
            Key::Named(named) => match named {
                NamedKey::Enter => "57414",
                NamedKey::ArrowLeft => "57417",
                NamedKey::ArrowRight => "57418",
                NamedKey::ArrowUp => "57419",
                NamedKey::ArrowDown => "57420",
                NamedKey::PageUp => "57421",
                NamedKey::PageDown => "57422",
                NamedKey::Home => "57423",
                NamedKey::End => "57424",
                NamedKey::Insert => "57425",
                NamedKey::Delete => "57426",
                _ => return None,
            },
            _ => return None,
        };

        Some(SequenceBase::new(base.into(), SequenceTerminator::Kitty))
    }

    /// Try building from [`NamedKey`] using the kitty keyboard protocol encoding
    /// for functional keys.
    fn try_build_named_kitty(&self, key: &KeyEvent) -> Option<SequenceBase> {
        let named = match key.logical_key {
            Key::Named(named) if self.kitty_seq => named,
            _ => return None,
        };

        let (base, terminator) = match named {
            // F3 in kitty protocol diverges from vivido's terminfo.
            NamedKey::F3 => ("13", SequenceTerminator::Normal('~')),
            NamedKey::F13 => ("57376", SequenceTerminator::Kitty),
            NamedKey::F14 => ("57377", SequenceTerminator::Kitty),
            NamedKey::F15 => ("57378", SequenceTerminator::Kitty),
            NamedKey::F16 => ("57379", SequenceTerminator::Kitty),
            NamedKey::F17 => ("57380", SequenceTerminator::Kitty),
            NamedKey::F18 => ("57381", SequenceTerminator::Kitty),
            NamedKey::F19 => ("57382", SequenceTerminator::Kitty),
            NamedKey::F20 => ("57383", SequenceTerminator::Kitty),
            NamedKey::F21 => ("57384", SequenceTerminator::Kitty),
            NamedKey::F22 => ("57385", SequenceTerminator::Kitty),
            NamedKey::F23 => ("57386", SequenceTerminator::Kitty),
            NamedKey::F24 => ("57387", SequenceTerminator::Kitty),
            NamedKey::F25 => ("57388", SequenceTerminator::Kitty),
            NamedKey::F26 => ("57389", SequenceTerminator::Kitty),
            NamedKey::F27 => ("57390", SequenceTerminator::Kitty),
            NamedKey::F28 => ("57391", SequenceTerminator::Kitty),
            NamedKey::F29 => ("57392", SequenceTerminator::Kitty),
            NamedKey::F30 => ("57393", SequenceTerminator::Kitty),
            NamedKey::F31 => ("57394", SequenceTerminator::Kitty),
            NamedKey::F32 => ("57395", SequenceTerminator::Kitty),
            NamedKey::F33 => ("57396", SequenceTerminator::Kitty),
            NamedKey::F34 => ("57397", SequenceTerminator::Kitty),
            NamedKey::F35 => ("57398", SequenceTerminator::Kitty),
            NamedKey::ScrollLock => ("57359", SequenceTerminator::Kitty),
            NamedKey::PrintScreen => ("57361", SequenceTerminator::Kitty),
            NamedKey::Pause => ("57362", SequenceTerminator::Kitty),
            NamedKey::ContextMenu => ("57363", SequenceTerminator::Kitty),
            NamedKey::MediaPlay => ("57428", SequenceTerminator::Kitty),
            NamedKey::MediaPause => ("57429", SequenceTerminator::Kitty),
            NamedKey::MediaPlayPause => ("57430", SequenceTerminator::Kitty),
            NamedKey::MediaStop => ("57432", SequenceTerminator::Kitty),
            NamedKey::MediaFastForward => ("57433", SequenceTerminator::Kitty),
            NamedKey::MediaRewind => ("57434", SequenceTerminator::Kitty),
            NamedKey::MediaTrackNext => ("57435", SequenceTerminator::Kitty),
            NamedKey::MediaTrackPrevious => ("57436", SequenceTerminator::Kitty),
            NamedKey::MediaRecord => ("57437", SequenceTerminator::Kitty),
            NamedKey::AudioVolumeDown => ("57438", SequenceTerminator::Kitty),
            NamedKey::AudioVolumeUp => ("57439", SequenceTerminator::Kitty),
            NamedKey::AudioVolumeMute => ("57440", SequenceTerminator::Kitty),
            _ => return None,
        };

        Some(SequenceBase::new(base.into(), terminator))
    }

    /// Try building from [`NamedKey`].
    fn try_build_named_normal(
        &self,
        key: &KeyEvent,
        has_associated_text: bool,
    ) -> Option<SequenceBase> {
        let named = match key.logical_key {
            Key::Named(named) => named,
            _ => return None,
        };

        // The default parameter is 1, so we can omit it.
        let one_based =
            if self.modifiers.is_empty() && !self.kitty_event_type && !has_associated_text {
                ""
            } else {
                "1"
            };
        let (base, terminator) = match named {
            NamedKey::PageUp => ("5", SequenceTerminator::Normal('~')),
            NamedKey::PageDown => ("6", SequenceTerminator::Normal('~')),
            NamedKey::Insert => ("2", SequenceTerminator::Normal('~')),
            NamedKey::Delete => ("3", SequenceTerminator::Normal('~')),
            NamedKey::Home => (one_based, SequenceTerminator::Normal('H')),
            NamedKey::End => (one_based, SequenceTerminator::Normal('F')),
            NamedKey::ArrowLeft => (one_based, SequenceTerminator::Normal('D')),
            NamedKey::ArrowRight => (one_based, SequenceTerminator::Normal('C')),
            NamedKey::ArrowUp => (one_based, SequenceTerminator::Normal('A')),
            NamedKey::ArrowDown => (one_based, SequenceTerminator::Normal('B')),
            NamedKey::F1 => (one_based, SequenceTerminator::Normal('P')),
            NamedKey::F2 => (one_based, SequenceTerminator::Normal('Q')),
            NamedKey::F3 => (one_based, SequenceTerminator::Normal('R')),
            NamedKey::F4 => (one_based, SequenceTerminator::Normal('S')),
            NamedKey::F5 => ("15", SequenceTerminator::Normal('~')),
            NamedKey::F6 => ("17", SequenceTerminator::Normal('~')),
            NamedKey::F7 => ("18", SequenceTerminator::Normal('~')),
            NamedKey::F8 => ("19", SequenceTerminator::Normal('~')),
            NamedKey::F9 => ("20", SequenceTerminator::Normal('~')),
            NamedKey::F10 => ("21", SequenceTerminator::Normal('~')),
            NamedKey::F11 => ("23", SequenceTerminator::Normal('~')),
            NamedKey::F12 => ("24", SequenceTerminator::Normal('~')),
            NamedKey::F13 => ("25", SequenceTerminator::Normal('~')),
            NamedKey::F14 => ("26", SequenceTerminator::Normal('~')),
            NamedKey::F15 => ("28", SequenceTerminator::Normal('~')),
            NamedKey::F16 => ("29", SequenceTerminator::Normal('~')),
            NamedKey::F17 => ("31", SequenceTerminator::Normal('~')),
            NamedKey::F18 => ("32", SequenceTerminator::Normal('~')),
            NamedKey::F19 => ("33", SequenceTerminator::Normal('~')),
            NamedKey::F20 => ("34", SequenceTerminator::Normal('~')),
            _ => return None,
        };

        Some(SequenceBase::new(base.into(), terminator))
    }

    /// Try building escape from control characters (e.g. Enter) and modifiers.
    fn try_build_control_char_or_mod(
        &self,
        key: &KeyEvent,
        mods: &mut SequenceModifiers,
    ) -> Option<SequenceBase> {
        if !self.kitty_encode_all && !self.kitty_seq {
            return None;
        }

        let named = match key.logical_key {
            Key::Named(named) => named,
            _ => return None,
        };

        let base = match named {
            NamedKey::Tab => "9",
            NamedKey::Enter => "13",
            NamedKey::Escape => "27",
            NamedKey::Space => "32",
            NamedKey::Backspace => "127",
            _ => "",
        };

        // Fail when the key is not a named control character and the active mode prohibits us
        // from encoding modifier keys.
        if !self.kitty_encode_all && base.is_empty() {
            return None;
        }

        let base = match (named, key.location) {
            (NamedKey::Shift, KeyLocation::Left) => "57441",
            (NamedKey::Control, KeyLocation::Left) => "57442",
            (NamedKey::Alt, KeyLocation::Left) => "57443",
            (NamedKey::Super, KeyLocation::Left) => "57444",
            (NamedKey::Hyper, KeyLocation::Left) => "57445",
            (NamedKey::Meta, KeyLocation::Left) => "57446",
            (NamedKey::Shift, _) => "57447",
            (NamedKey::Control, _) => "57448",
            (NamedKey::Alt, _) => "57449",
            (NamedKey::Super, _) => "57450",
            (NamedKey::Hyper, _) => "57451",
            (NamedKey::Meta, _) => "57452",
            (NamedKey::CapsLock, _) => "57358",
            (NamedKey::NumLock, _) => "57360",
            _ => base,
        };

        // NOTE: Kitty's protocol mandates that the modifier state is applied before
        // key press, however winit sends them after the key press, so for modifiers
        // itself apply the state based on keysyms and not the _actual_ modifiers
        // state, which is how kitty is doing so and what is suggested in such case.
        let press = key.state.is_pressed();
        match named {
            NamedKey::Shift => mods.set(SequenceModifiers::SHIFT, press),
            NamedKey::Control => mods.set(SequenceModifiers::CONTROL, press),
            NamedKey::Alt => mods.set(SequenceModifiers::ALT, press),
            NamedKey::Super => mods.set(SequenceModifiers::SUPER, press),
            _ => (),
        }

        if base.is_empty() {
            None
        } else {
            Some(SequenceBase::new(base.into(), SequenceTerminator::Kitty))
        }
    }
}

pub struct SequenceBase {
    /// The base of the payload, which is the `number` and optionally an alt base from the kitty
    /// spec.
    payload: Cow<'static, str>,
    terminator: SequenceTerminator,
}

impl SequenceBase {
    fn new(payload: Cow<'static, str>, terminator: SequenceTerminator) -> Self {
        Self { payload, terminator }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceTerminator {
    /// The normal key esc sequence terminator defined by xterm/dec.
    Normal(char),
    /// The terminator is for kitty escape sequence.
    Kitty,
}

impl SequenceTerminator {
    fn encode_esc_sequence(self) -> char {
        match self {
            SequenceTerminator::Normal(char) => char,
            SequenceTerminator::Kitty => 'u',
        }
    }
}

bitflags::bitflags! {
    /// The modifiers encoding for escape sequence.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct SequenceModifiers : u8 {
        const SHIFT   = 0b0000_0001;
        const ALT     = 0b0000_0010;
        const CONTROL = 0b0000_0100;
        const SUPER   = 0b0000_1000;
        // NOTE: Kitty protocol defines additional modifiers to what is present here, like
        // Capslock, but it's not a modifier as per winit.
    }
}

impl SequenceModifiers {
    /// Get the value which should be passed to escape sequence.
    pub fn encode_esc_sequence(self) -> u8 {
        self.bits() + 1
    }
}

impl From<ModifiersState> for SequenceModifiers {
    fn from(mods: ModifiersState) -> Self {
        let mut modifiers = Self::empty();
        modifiers.set(Self::SHIFT, mods.shift_key());
        modifiers.set(Self::ALT, mods.alt_key());
        modifiers.set(Self::CONTROL, mods.control_key());
        modifiers.set(Self::SUPER, mods.super_key());
        modifiers
    }
}

/// Check whether the `text` is `0x7f`, `C0` or `C1` control code.
fn is_control_character(text: &str) -> bool {
    // 0x7f (DEL) is included here since it has a dedicated control code (`^?`) which generally
    // does not match the reported text (`^H`), despite not technically being part of C0 or C1.
    let codepoint = text.bytes().next().unwrap();
    text.len() == 1 && (codepoint < 0x20 || (0x7f..=0x9f).contains(&codepoint))
}

/// Encode a protocol-neutral IPC key for the current terminal keyboard modes.
#[cfg(unix)]
pub fn encode_ipc_key_event(
    key: &str,
    modifiers: &[String],
    mode: TermMode,
    repeated: bool,
) -> Result<Vec<u8>, crate::polling::ipc::IpcError> {
    let mods = ipc_modifiers(modifiers)?;
    let modifier_parameter = mods.bits() + 1;
    let kitty = mode.intersects(
        TermMode::REPORT_ALL_KEYS_AS_ESC
            | TermMode::DISAMBIGUATE_ESC_CODES
            | TermMode::REPORT_EVENT_TYPES,
    );

    let character = {
        let mut chars = key.chars();
        match (chars.next(), chars.next()) {
            (Some(character), None) => Some(character),
            _ => None,
        }
    };
    if let Some(mut character) = character {
        if mods.contains(SequenceModifiers::CONTROL) {
            character = character.to_ascii_lowercase();
            let control = match character {
                '@' | ' ' => Some(0),
                'a'..='z' => Some(character as u8 - b'a' + 1),
                '[' => Some(27),
                '\\' => Some(28),
                ']' => Some(29),
                '^' => Some(30),
                '_' | '?' => Some(31),
                _ => None,
            };
            if let Some(control) = control
                && !kitty
            {
                let mut bytes = Vec::with_capacity(2);
                if mods.contains(SequenceModifiers::ALT) {
                    bytes.push(b'\x1b');
                }
                bytes.push(control);
                return Ok(bytes);
            }
        }
        if kitty && (!mods.is_empty() || mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC)) {
            let modifiers = kitty_modifiers(modifier_parameter, mode, repeated);
            return Ok(format!("\x1b[{};{modifiers}u", u32::from(character)).into_bytes());
        }
        let mut bytes = Vec::with_capacity(character.len_utf8() + 1);
        if mods.contains(SequenceModifiers::ALT) {
            bytes.push(b'\x1b');
        }
        let mut encoded = [0; 4];
        bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        return Ok(bytes);
    }

    let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
    let control = match normalized.as_str() {
        "enter" | "return" => Some((13, b'\r')),
        "escape" | "esc" => Some((27, b'\x1b')),
        "tab" => Some((9, b'\t')),
        "backspace" => Some((127, b'\x7f')),
        _ => None,
    };
    if let Some((codepoint, byte)) = control {
        let disambiguate = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC)
            || (mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) && !mods.is_empty());
        if kitty && (disambiguate || repeated) {
            let modifiers = kitty_modifiers(modifier_parameter, mode, repeated);
            return Ok(format!("\x1b[{codepoint};{modifiers}u").into_bytes());
        }
        if normalized == "tab" && mods.contains(SequenceModifiers::SHIFT) {
            let mut bytes = Vec::with_capacity(4);
            if mods.contains(SequenceModifiers::ALT) {
                bytes.push(b'\x1b');
            }
            bytes.extend_from_slice(b"\x1b[Z");
            return Ok(bytes);
        }
        let mut bytes = Vec::with_capacity(2);
        if mods.contains(SequenceModifiers::ALT) {
            bytes.push(b'\x1b');
        }
        bytes.push(byte);
        return Ok(bytes);
    }

    let cursor_final = match normalized.as_str() {
        "arrowup" | "up" => Some('A'),
        "arrowdown" | "down" => Some('B'),
        "arrowright" | "right" => Some('C'),
        "arrowleft" | "left" => Some('D'),
        "home" => Some('H'),
        "end" => Some('F'),
        _ => None,
    };
    if let Some(final_byte) = cursor_final {
        if repeated && mode.contains(TermMode::REPORT_EVENT_TYPES) {
            let modifiers = kitty_modifiers(modifier_parameter, mode, true);
            return Ok(format!("\x1b[1;{modifiers}{final_byte}").into_bytes());
        }
        if mods.is_empty() {
            let introducer = if mode.contains(TermMode::APP_CURSOR) { "\x1bO" } else { "\x1b[" };
            return Ok(format!("{introducer}{final_byte}").into_bytes());
        }
        return Ok(format!("\x1b[1;{modifier_parameter}{final_byte}").into_bytes());
    }

    let tilde_code = match normalized.as_str() {
        "insert" => Some(2),
        "delete" | "del" => Some(3),
        "pageup" => Some(5),
        "pagedown" => Some(6),
        _ => None,
    };
    if let Some(code) = tilde_code {
        let modifiers = kitty_modifiers(modifier_parameter, mode, repeated);
        return if mods.is_empty() {
            if repeated && mode.contains(TermMode::REPORT_EVENT_TYPES) {
                Ok(format!("\x1b[{code};{modifiers}~").into_bytes())
            } else {
                Ok(format!("\x1b[{code}~").into_bytes())
            }
        } else {
            Ok(format!("\x1b[{code};{modifiers}~").into_bytes())
        };
    }

    if let Some(number) = normalized.strip_prefix('f').and_then(|number| number.parse::<u8>().ok())
        && (1..=35).contains(&number)
    {
        if number <= 4 {
            let final_byte = char::from(b'P' + number - 1);
            let modifiers = kitty_modifiers(modifier_parameter, mode, repeated);
            return if mods.is_empty() {
                if repeated && mode.contains(TermMode::REPORT_EVENT_TYPES) {
                    Ok(format!("\x1b[1;{modifiers}{final_byte}").into_bytes())
                } else {
                    Ok(format!("\x1bO{final_byte}").into_bytes())
                }
            } else {
                Ok(format!("\x1b[1;{modifiers}{final_byte}").into_bytes())
            };
        }
        let normal_codes = [15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34];
        if number <= 20 {
            let code = normal_codes[usize::from(number - 5)];
            let modifiers = kitty_modifiers(modifier_parameter, mode, repeated);
            return if mods.is_empty() {
                if repeated && mode.contains(TermMode::REPORT_EVENT_TYPES) {
                    Ok(format!("\x1b[{code};{modifiers}~").into_bytes())
                } else {
                    Ok(format!("\x1b[{code}~").into_bytes())
                }
            } else {
                Ok(format!("\x1b[{code};{modifiers}~").into_bytes())
            };
        }
        if kitty {
            let code = 57375 + u32::from(number - 12);
            let modifiers = kitty_modifiers(modifier_parameter, mode, repeated);
            return Ok(format!("\x1b[{code};{modifiers}u").into_bytes());
        }
        let code = 42 + u32::from(number - 21);
        return if mods.is_empty() {
            Ok(format!("\x1b[{code}~").into_bytes())
        } else {
            Ok(format!("\x1b[{code};{modifier_parameter}~").into_bytes())
        };
    }

    let keypad = match normalized.as_str() {
        "keypad0" => Some((57399, b'0', 'p')),
        "keypad1" => Some((57400, b'1', 'q')),
        "keypad2" => Some((57401, b'2', 'r')),
        "keypad3" => Some((57402, b'3', 's')),
        "keypad4" => Some((57403, b'4', 't')),
        "keypad5" => Some((57404, b'5', 'u')),
        "keypad6" => Some((57405, b'6', 'v')),
        "keypad7" => Some((57406, b'7', 'w')),
        "keypad8" => Some((57407, b'8', 'x')),
        "keypad9" => Some((57408, b'9', 'y')),
        "keypaddecimal" => Some((57409, b'.', 'n')),
        "keypaddivide" => Some((57410, b'/', 'o')),
        "keypadmultiply" => Some((57411, b'*', 'j')),
        "keypadsubtract" => Some((57412, b'-', 'm')),
        "keypadadd" => Some((57413, b'+', 'k')),
        "keypadenter" => Some((57414, b'\r', 'M')),
        "keypadequal" => Some((57415, b'=', 'X')),
        _ => None,
    };
    if let Some((code, literal, application_final)) = keypad {
        if kitty {
            let modifiers = kitty_modifiers(modifier_parameter, mode, repeated);
            return Ok(format!("\x1b[{code};{modifiers}u").into_bytes());
        }
        if mode.contains(TermMode::APP_KEYPAD) {
            return if mods.is_empty() {
                Ok(format!("\x1bO{application_final}").into_bytes())
            } else {
                Ok(format!("\x1b[1;{modifier_parameter}{application_final}").into_bytes())
            };
        }
        let mut bytes = Vec::with_capacity(2);
        if mods.contains(SequenceModifiers::ALT) {
            bytes.push(b'\x1b');
        }
        bytes.push(literal);
        return Ok(bytes);
    }

    Err(crate::polling::ipc::IpcError::new("invalid_params", format!("unknown key {key:?}")))
}

#[cfg(unix)]
fn kitty_modifiers(modifier_parameter: u8, mode: TermMode, repeated: bool) -> String {
    if repeated && mode.contains(TermMode::REPORT_EVENT_TYPES) {
        format!("{modifier_parameter}:2")
    } else {
        modifier_parameter.to_string()
    }
}

#[cfg(unix)]
fn ipc_modifiers(modifiers: &[String]) -> Result<SequenceModifiers, crate::polling::ipc::IpcError> {
    let mut result = SequenceModifiers::empty();
    for modifier in modifiers {
        match modifier.to_ascii_lowercase().as_str() {
            "shift" => result.insert(SequenceModifiers::SHIFT),
            "alt" | "option" => result.insert(SequenceModifiers::ALT),
            "ctrl" | "control" => result.insert(SequenceModifiers::CONTROL),
            "super" | "command" | "cmd" => result.insert(SequenceModifiers::SUPER),
            _ => {
                return Err(crate::polling::ipc::IpcError::new(
                    "invalid_params",
                    format!("unknown modifier {modifier:?}"),
                ));
            },
        }
    }
    Ok(result)
}

#[cfg(unix)]
pub fn ipc_modifier_state(
    modifiers: &[String],
) -> Result<ModifiersState, crate::polling::ipc::IpcError> {
    let mut result = ModifiersState::empty();
    for modifier in modifiers {
        match modifier.to_ascii_lowercase().as_str() {
            "shift" => result.insert(ModifiersState::SHIFT),
            "alt" | "option" => result.insert(ModifiersState::ALT),
            "ctrl" | "control" => result.insert(ModifiersState::CONTROL),
            "super" | "command" | "cmd" => result.insert(ModifiersState::SUPER),
            _ => {
                return Err(crate::polling::ipc::IpcError::new(
                    "invalid_params",
                    format!("unknown modifier {modifier:?}"),
                ));
            },
        }
    }
    Ok(result)
}

#[cfg(unix)]
fn ipc_logical_key(key: &str) -> Result<(Key, KeyLocation), crate::polling::ipc::IpcError> {
    let mut characters = key.chars();
    if let (Some(character), None) = (characters.next(), characters.next()) {
        return Ok((Key::Character(character.to_string().into()), KeyLocation::Standard));
    }

    let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
    let named = match normalized.as_str() {
        "enter" | "return" | "keypadenter" => NamedKey::Enter,
        "escape" | "esc" => NamedKey::Escape,
        "tab" => NamedKey::Tab,
        "backspace" => NamedKey::Backspace,
        "arrowup" | "up" => NamedKey::ArrowUp,
        "arrowdown" | "down" => NamedKey::ArrowDown,
        "arrowleft" | "left" => NamedKey::ArrowLeft,
        "arrowright" | "right" => NamedKey::ArrowRight,
        "home" => NamedKey::Home,
        "end" => NamedKey::End,
        "insert" => NamedKey::Insert,
        "delete" | "del" => NamedKey::Delete,
        "pageup" => NamedKey::PageUp,
        "pagedown" => NamedKey::PageDown,
        "f1" => NamedKey::F1,
        "f2" => NamedKey::F2,
        "f3" => NamedKey::F3,
        "f4" => NamedKey::F4,
        "f5" => NamedKey::F5,
        "f6" => NamedKey::F6,
        "f7" => NamedKey::F7,
        "f8" => NamedKey::F8,
        "f9" => NamedKey::F9,
        "f10" => NamedKey::F10,
        "f11" => NamedKey::F11,
        "f12" => NamedKey::F12,
        "f13" => NamedKey::F13,
        "f14" => NamedKey::F14,
        "f15" => NamedKey::F15,
        "f16" => NamedKey::F16,
        "f17" => NamedKey::F17,
        "f18" => NamedKey::F18,
        "f19" => NamedKey::F19,
        "f20" => NamedKey::F20,
        "f21" => NamedKey::F21,
        "f22" => NamedKey::F22,
        "f23" => NamedKey::F23,
        "f24" => NamedKey::F24,
        "f25" => NamedKey::F25,
        "f26" => NamedKey::F26,
        "f27" => NamedKey::F27,
        "f28" => NamedKey::F28,
        "f29" => NamedKey::F29,
        "f30" => NamedKey::F30,
        "f31" => NamedKey::F31,
        "f32" => NamedKey::F32,
        "f33" => NamedKey::F33,
        "f34" => NamedKey::F34,
        "f35" => NamedKey::F35,
        _ if normalized.starts_with("keypad") => {
            let character = match normalized.strip_prefix("keypad").unwrap_or_default() {
                "0" => "0",
                "1" => "1",
                "2" => "2",
                "3" => "3",
                "4" => "4",
                "5" => "5",
                "6" => "6",
                "7" => "7",
                "8" => "8",
                "9" => "9",
                "decimal" => ".",
                "divide" => "/",
                "multiply" => "*",
                "subtract" => "-",
                "add" => "+",
                "equal" => "=",
                _ => {
                    return Err(crate::polling::ipc::IpcError::new(
                        "invalid_params",
                        format!("unknown key {key:?}"),
                    ));
                },
            };
            return Ok((Key::Character(character.into()), KeyLocation::Numpad));
        },
        _ => {
            return Err(crate::polling::ipc::IpcError::new(
                "invalid_params",
                format!("unknown key {key:?}"),
            ));
        },
    };
    let location =
        if normalized.starts_with("keypad") { KeyLocation::Numpad } else { KeyLocation::Standard };
    Ok((Key::Named(named), location))
}

#[cfg(all(test, unix))]
mod ipc_tests {
    use super::encode_ipc_key_event;
    use crate::terminal::term::TermMode;

    fn encode_ipc_key(
        key: &str,
        modifiers: &[String],
        mode: TermMode,
    ) -> Result<Vec<u8>, crate::polling::ipc::IpcError> {
        encode_ipc_key_event(key, modifiers, mode, false)
    }

    #[test]
    fn encodes_application_cursor_and_keypad_modes() {
        assert_eq!(encode_ipc_key("ArrowUp", &[], TermMode::APP_CURSOR).unwrap(), b"\x1bOA");
        assert_eq!(encode_ipc_key("Keypad7", &[], TermMode::APP_KEYPAD).unwrap(), b"\x1bOw");
        assert_eq!(encode_ipc_key("Keypad7", &[], TermMode::empty()).unwrap(), b"7");
    }

    #[test]
    fn encodes_shift_tab_and_high_function_keys() {
        assert_eq!(
            encode_ipc_key("Tab", &[String::from("Shift")], TermMode::empty()).unwrap(),
            b"\x1b[Z"
        );
        assert_eq!(encode_ipc_key("F35", &[], TermMode::empty()).unwrap(), b"\x1b[56~");
    }

    #[test]
    fn encodes_control_unicode_and_kitty_keys() {
        assert_eq!(
            encode_ipc_key("c", &[String::from("Ctrl")], TermMode::empty()).unwrap(),
            b"\x03"
        );
        assert_eq!(encode_ipc_key("界", &[], TermMode::empty()).unwrap(), "界".as_bytes());
        assert_eq!(
            encode_ipc_key("F21", &[], TermMode::REPORT_ALL_KEYS_AS_ESC).unwrap(),
            b"\x1b[57384;1u"
        );
        assert_eq!(
            encode_ipc_key_event("F5", &[], TermMode::REPORT_EVENT_TYPES, true).unwrap(),
            b"\x1b[15;1:2~"
        );
    }
}
