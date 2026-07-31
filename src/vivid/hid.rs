//! Physical key to USB HID keyboard-page usage.
//!
//! Desktop §7 carries keys as HID keyboard-page usages in `0x04..=0xe7`, and requires physical
//! transitions with synthetic repeat suppressed. Winit's `KeyCode` is already a physical position
//! in the same sense as a browser's `KeyboardEvent.code`, so the two presenters map from
//! equivalent inputs onto one shared numbering — which is why a producer can accept either
//! without knowing where the event came from.

use winit::keyboard::KeyCode;

/// The HID keyboard-page usage for a physical key, or `None` for one the page does not name.
pub fn usage(code: KeyCode) -> Option<u16> {
    let usage = match code {
        KeyCode::KeyA => 0x04,
        KeyCode::KeyB => 0x05,
        KeyCode::KeyC => 0x06,
        KeyCode::KeyD => 0x07,
        KeyCode::KeyE => 0x08,
        KeyCode::KeyF => 0x09,
        KeyCode::KeyG => 0x0a,
        KeyCode::KeyH => 0x0b,
        KeyCode::KeyI => 0x0c,
        KeyCode::KeyJ => 0x0d,
        KeyCode::KeyK => 0x0e,
        KeyCode::KeyL => 0x0f,
        KeyCode::KeyM => 0x10,
        KeyCode::KeyN => 0x11,
        KeyCode::KeyO => 0x12,
        KeyCode::KeyP => 0x13,
        KeyCode::KeyQ => 0x14,
        KeyCode::KeyR => 0x15,
        KeyCode::KeyS => 0x16,
        KeyCode::KeyT => 0x17,
        KeyCode::KeyU => 0x18,
        KeyCode::KeyV => 0x19,
        KeyCode::KeyW => 0x1a,
        KeyCode::KeyX => 0x1b,
        KeyCode::KeyY => 0x1c,
        KeyCode::KeyZ => 0x1d,
        KeyCode::Digit1 => 0x1e,
        KeyCode::Digit2 => 0x1f,
        KeyCode::Digit3 => 0x20,
        KeyCode::Digit4 => 0x21,
        KeyCode::Digit5 => 0x22,
        KeyCode::Digit6 => 0x23,
        KeyCode::Digit7 => 0x24,
        KeyCode::Digit8 => 0x25,
        KeyCode::Digit9 => 0x26,
        KeyCode::Digit0 => 0x27,
        KeyCode::Enter => 0x28,
        KeyCode::Escape => 0x29,
        KeyCode::Backspace => 0x2a,
        KeyCode::Tab => 0x2b,
        KeyCode::Space => 0x2c,
        KeyCode::Minus => 0x2d,
        KeyCode::Equal => 0x2e,
        KeyCode::BracketLeft => 0x2f,
        KeyCode::BracketRight => 0x30,
        KeyCode::Backslash => 0x31,
        KeyCode::Semicolon => 0x33,
        KeyCode::Quote => 0x34,
        KeyCode::Backquote => 0x35,
        KeyCode::Comma => 0x36,
        KeyCode::Period => 0x37,
        KeyCode::Slash => 0x38,
        KeyCode::CapsLock => 0x39,
        KeyCode::F1 => 0x3a,
        KeyCode::F2 => 0x3b,
        KeyCode::F3 => 0x3c,
        KeyCode::F4 => 0x3d,
        KeyCode::F5 => 0x3e,
        KeyCode::F6 => 0x3f,
        KeyCode::F7 => 0x40,
        KeyCode::F8 => 0x41,
        KeyCode::F9 => 0x42,
        KeyCode::F10 => 0x43,
        KeyCode::F11 => 0x44,
        KeyCode::F12 => 0x45,
        KeyCode::PrintScreen => 0x46,
        KeyCode::ScrollLock => 0x47,
        KeyCode::Pause => 0x48,
        KeyCode::Insert => 0x49,
        KeyCode::Home => 0x4a,
        KeyCode::PageUp => 0x4b,
        KeyCode::Delete => 0x4c,
        KeyCode::End => 0x4d,
        KeyCode::PageDown => 0x4e,
        KeyCode::ArrowRight => 0x4f,
        KeyCode::ArrowLeft => 0x50,
        KeyCode::ArrowDown => 0x51,
        KeyCode::ArrowUp => 0x52,
        KeyCode::NumLock => 0x53,
        KeyCode::NumpadDivide => 0x54,
        KeyCode::NumpadMultiply => 0x55,
        KeyCode::NumpadSubtract => 0x56,
        KeyCode::NumpadAdd => 0x57,
        KeyCode::NumpadEnter => 0x58,
        KeyCode::Numpad1 => 0x59,
        KeyCode::Numpad2 => 0x5a,
        KeyCode::Numpad3 => 0x5b,
        KeyCode::Numpad4 => 0x5c,
        KeyCode::Numpad5 => 0x5d,
        KeyCode::Numpad6 => 0x5e,
        KeyCode::Numpad7 => 0x5f,
        KeyCode::Numpad8 => 0x60,
        KeyCode::Numpad9 => 0x61,
        KeyCode::Numpad0 => 0x62,
        KeyCode::NumpadDecimal => 0x63,
        KeyCode::ContextMenu => 0x65,
        KeyCode::ControlLeft => 0xe0,
        KeyCode::ShiftLeft => 0xe1,
        KeyCode::AltLeft => 0xe2,
        KeyCode::SuperLeft => 0xe3,
        KeyCode::ControlRight => 0xe4,
        KeyCode::ShiftRight => 0xe5,
        KeyCode::AltRight => 0xe6,
        KeyCode::SuperRight => 0xe7,
        _ => return None,
    };
    Some(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mapped_range_matches_the_keyboard_page() {
        // Desktop §7 accepts `0x04..=0xe7`, so nothing may map outside it.
        for code in [
            KeyCode::KeyA,
            KeyCode::Digit0,
            KeyCode::Escape,
            KeyCode::F12,
            KeyCode::Numpad0,
            KeyCode::SuperRight,
        ] {
            let usage = usage(code).expect("a named key maps");
            assert!((0x04..=0xe7).contains(&usage), "{code:?} mapped outside the page");
        }
    }

    #[test]
    fn the_mapping_is_injective() {
        // Two physical keys sharing a usage would make the producer's held-key set unbalanced.
        let codes = [
            KeyCode::KeyA,
            KeyCode::KeyZ,
            KeyCode::Digit1,
            KeyCode::Digit0,
            KeyCode::Enter,
            KeyCode::Escape,
            KeyCode::Backspace,
            KeyCode::Tab,
            KeyCode::Space,
            KeyCode::Minus,
            KeyCode::Equal,
            KeyCode::BracketLeft,
            KeyCode::BracketRight,
            KeyCode::Backslash,
            KeyCode::Semicolon,
            KeyCode::Quote,
            KeyCode::Backquote,
            KeyCode::Comma,
            KeyCode::Period,
            KeyCode::Slash,
            KeyCode::CapsLock,
            KeyCode::F1,
            KeyCode::F12,
            KeyCode::PrintScreen,
            KeyCode::ScrollLock,
            KeyCode::Pause,
            KeyCode::Insert,
            KeyCode::Home,
            KeyCode::PageUp,
            KeyCode::Delete,
            KeyCode::End,
            KeyCode::PageDown,
            KeyCode::ArrowRight,
            KeyCode::ArrowLeft,
            KeyCode::ArrowDown,
            KeyCode::ArrowUp,
            KeyCode::NumLock,
            KeyCode::NumpadDivide,
            KeyCode::NumpadMultiply,
            KeyCode::NumpadSubtract,
            KeyCode::NumpadAdd,
            KeyCode::NumpadEnter,
            KeyCode::Numpad0,
            KeyCode::Numpad9,
            KeyCode::NumpadDecimal,
            KeyCode::ContextMenu,
            KeyCode::ControlLeft,
            KeyCode::ShiftLeft,
            KeyCode::AltLeft,
            KeyCode::SuperLeft,
            KeyCode::ControlRight,
            KeyCode::ShiftRight,
            KeyCode::AltRight,
            KeyCode::SuperRight,
        ];
        let mut seen = Vec::new();
        for code in codes {
            let usage = usage(code).expect("every listed key maps");
            assert!(!seen.contains(&usage), "{code:?} reuses usage {usage:#04x}");
            seen.push(usage);
        }
    }

    #[test]
    fn an_unnamed_key_maps_to_nothing() {
        // A key with no keyboard-page usage is dropped rather than guessed at.
        assert!(usage(KeyCode::Fn).is_none());
    }
}
