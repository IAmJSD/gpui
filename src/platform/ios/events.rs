//! Hardware keyboard and modifier handling.
//!
//! `UIKey` reports both the HID usage of the physical key and the characters
//! it produced with and without modifiers, which is enough to build a gpui
//! `Keystroke` the same way the macOS backend does: `key` is the unshifted
//! character (or a named key), `key_char` is what would be typed.

use crate::{Capslock, Keystroke, Modifiers};
use objc::{msg_send, sel, sel_impl};
use std::borrow::Cow;

use super::{NSStringExt, id};

/// `UIKeyModifierFlags`.
pub(crate) const UI_KEY_MODIFIER_ALPHA_SHIFT: usize = 1 << 16;
pub(crate) const UI_KEY_MODIFIER_SHIFT: usize = 1 << 17;
pub(crate) const UI_KEY_MODIFIER_CONTROL: usize = 1 << 18;
pub(crate) const UI_KEY_MODIFIER_ALTERNATE: usize = 1 << 19;
pub(crate) const UI_KEY_MODIFIER_COMMAND: usize = 1 << 20;

/// Translates a `UIKeyModifierFlags` bit set.
pub(crate) fn modifiers_from_flags(flags: usize) -> Modifiers {
    Modifiers {
        control: flags & UI_KEY_MODIFIER_CONTROL != 0,
        alt: flags & UI_KEY_MODIFIER_ALTERNATE != 0,
        shift: flags & UI_KEY_MODIFIER_SHIFT != 0,
        platform: flags & UI_KEY_MODIFIER_COMMAND != 0,
        function: false,
    }
}

pub(crate) fn capslock_from_flags(flags: usize) -> Capslock {
    Capslock {
        on: flags & UI_KEY_MODIFIER_ALPHA_SHIFT != 0,
    }
}

/// The modifier flags carried by a `UIEvent` (touch, press or gesture).
pub(crate) unsafe fn event_modifier_flags(event: id) -> usize {
    if event.is_null() {
        return 0;
    }
    unsafe { msg_send![event, modifierFlags] }
}

/// `UIKeyboardHIDUsage` values for the keys gpui names rather than spells.
mod hid {
    pub const ENTER: isize = 0x28;
    pub const ESCAPE: isize = 0x29;
    pub const BACKSPACE: isize = 0x2A;
    pub const TAB: isize = 0x2B;
    pub const SPACE: isize = 0x2C;
    pub const F1: isize = 0x3A;
    pub const F12: isize = 0x45;
    pub const INSERT: isize = 0x49;
    pub const HOME: isize = 0x4A;
    pub const PAGE_UP: isize = 0x4B;
    pub const DELETE_FORWARD: isize = 0x4C;
    pub const END: isize = 0x4D;
    pub const PAGE_DOWN: isize = 0x4E;
    pub const RIGHT: isize = 0x4F;
    pub const LEFT: isize = 0x50;
    pub const DOWN: isize = 0x51;
    pub const UP: isize = 0x52;
    pub const KEYPAD_ENTER: isize = 0x58;
    pub const F13: isize = 0x68;
    pub const F24: isize = 0x73;
    pub const LEFT_CONTROL: isize = 0xE0;
    pub const RIGHT_GUI: isize = 0xE7;
}

/// Whether the HID usage is a modifier key on its own (which UIKit reports
/// as a press with no characters).
pub(crate) fn is_modifier_key(key_code: isize) -> bool {
    (hid::LEFT_CONTROL..=hid::RIGHT_GUI).contains(&key_code)
}

/// The gpui name for a key UIKit reports by HID usage rather than by a
/// printable character, if it has one.
fn named_key(key_code: isize) -> Option<(&'static str, Option<&'static str>)> {
    Some(match key_code {
        hid::ENTER | hid::KEYPAD_ENTER => ("enter", Some("\n")),
        hid::ESCAPE => ("escape", None),
        hid::BACKSPACE => ("backspace", None),
        hid::TAB => ("tab", Some("\t")),
        hid::SPACE => ("space", Some(" ")),
        hid::INSERT => ("insert", None),
        hid::HOME => ("home", None),
        hid::PAGE_UP => ("pageup", None),
        hid::DELETE_FORWARD => ("delete", None),
        hid::END => ("end", None),
        hid::PAGE_DOWN => ("pagedown", None),
        hid::RIGHT => ("right", None),
        hid::LEFT => ("left", None),
        hid::DOWN => ("down", None),
        hid::UP => ("up", None),
        code if (hid::F1..=hid::F12).contains(&code) => {
            (FUNCTION_KEYS[(code - hid::F1) as usize], None)
        }
        code if (hid::F13..=hid::F24).contains(&code) => {
            (FUNCTION_KEYS[(code - hid::F13) as usize + 12], None)
        }
        _ => return None,
    })
}

const FUNCTION_KEYS: [&str; 24] = [
    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14", "f15",
    "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23", "f24",
];

/// Builds the keystroke for a `UIKey`, or `None` for a bare modifier press.
pub(crate) unsafe fn keystroke_from_ui_key(key: id) -> Option<Keystroke> {
    unsafe {
        let key_code: isize = msg_send![key, keyCode];
        if is_modifier_key(key_code) {
            return None;
        }
        let flags: usize = msg_send![key, modifierFlags];
        let mut modifiers = modifiers_from_flags(flags);

        let characters: id = msg_send![key, characters];
        let characters = characters.to_str().to_string();
        let unmodified: id = msg_send![key, charactersIgnoringModifiers];
        let unmodified = unmodified.to_str().to_string();

        if let Some((name, key_char)) = named_key(key_code) {
            let key_char = key_char.filter(|_| !modifiers.control && !modifiers.platform);
            return Some(Keystroke {
                modifiers,
                key: name.to_string(),
                key_char: key_char.map(str::to_string),
            });
        }

        // Anything else is a printable key. Mirror the macOS convention:
        // `key` is the unshifted character for letters (so a binding on
        // shift-a matches), the shifted one for symbols (a binding on "!"
        // rather than shift-1), and `key_char` is the text a text field
        // would receive.
        let base = if unmodified.is_empty() {
            characters.clone()
        } else {
            unmodified
        };
        let key = if modifiers.shift && base.chars().all(|c| c.is_ascii_lowercase()) {
            base
        } else if modifiers.shift && !characters.is_empty() {
            modifiers.shift = false;
            characters.clone()
        } else {
            base
        };
        if key.is_empty() {
            return None;
        }
        let key_char = if !modifiers.control && !modifiers.platform && !characters.is_empty() {
            Some(characters)
        } else {
            None
        };
        Some(Keystroke {
            modifiers,
            key,
            key_char,
        })
    }
}

/// The `input` string of a `UIKeyCommand` for a gpui key name.
pub(crate) fn key_to_ui_key_input(key: &str) -> Cow<'_, str> {
    unsafe {
        let named: Option<id> = match key {
            "up" => Some(UIKeyInputUpArrow),
            "down" => Some(UIKeyInputDownArrow),
            "left" => Some(UIKeyInputLeftArrow),
            "right" => Some(UIKeyInputRightArrow),
            "escape" => Some(UIKeyInputEscape),
            "pageup" => Some(UIKeyInputPageUp),
            "pagedown" => Some(UIKeyInputPageDown),
            "home" => Some(UIKeyInputHome),
            "end" => Some(UIKeyInputEnd),
            "delete" => Some(UIKeyInputDelete),
            "f1" => Some(UIKeyInputF1),
            "f2" => Some(UIKeyInputF2),
            "f3" => Some(UIKeyInputF3),
            "f4" => Some(UIKeyInputF4),
            "f5" => Some(UIKeyInputF5),
            "f6" => Some(UIKeyInputF6),
            "f7" => Some(UIKeyInputF7),
            "f8" => Some(UIKeyInputF8),
            "f9" => Some(UIKeyInputF9),
            "f10" => Some(UIKeyInputF10),
            "f11" => Some(UIKeyInputF11),
            "f12" => Some(UIKeyInputF12),
            _ => None,
        };
        if let Some(named) = named {
            return Cow::Owned(named.to_str().to_string());
        }
    }
    match key {
        "enter" => Cow::Borrowed("\r"),
        "tab" => Cow::Borrowed("\t"),
        "space" => Cow::Borrowed(" "),
        "backspace" => Cow::Borrowed("\u{8}"),
        _ => Cow::Borrowed(key),
    }
}

#[link(name = "UIKit", kind = "framework")]
unsafe extern "C" {
    static UIKeyInputUpArrow: id;
    static UIKeyInputDownArrow: id;
    static UIKeyInputLeftArrow: id;
    static UIKeyInputRightArrow: id;
    static UIKeyInputEscape: id;
    static UIKeyInputPageUp: id;
    static UIKeyInputPageDown: id;
    static UIKeyInputHome: id;
    static UIKeyInputEnd: id;
    static UIKeyInputDelete: id;
    static UIKeyInputF1: id;
    static UIKeyInputF2: id;
    static UIKeyInputF3: id;
    static UIKeyInputF4: id;
    static UIKeyInputF5: id;
    static UIKeyInputF6: id;
    static UIKeyInputF7: id;
    static UIKeyInputF8: id;
    static UIKeyInputF9: id;
    static UIKeyInputF10: id;
    static UIKeyInputF11: id;
    static UIKeyInputF12: id;
}
