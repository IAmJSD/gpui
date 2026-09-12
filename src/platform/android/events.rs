//! Keyboard events. Android reports a key code and a meta state; the
//! characters a key produces come from the input device's
//! `KeyCharacterMap`, which is enough to build a gpui `Keystroke` with the
//! macOS conventions: `key` is the unshifted character (or a named key),
//! `key_char` is what would be typed.

use crate::{Capslock, Keystroke, Modifiers};
use android_activity::{
    AndroidApp,
    input::{KeyEvent, Keycode, MetaState},
};

/// Translates a meta state's modifier bits.
pub(crate) fn modifiers_from_meta_state(meta: MetaState) -> Modifiers {
    Modifiers {
        control: meta.ctrl_on(),
        alt: meta.alt_on(),
        shift: meta.shift_on(),
        platform: meta.meta_on(),
        function: meta.function_on(),
    }
}

pub(crate) fn capslock_from_meta_state(meta: MetaState) -> Capslock {
    Capslock {
        on: meta.caps_lock_on(),
    }
}

/// Whether the key is a modifier on its own, which changes the meta state
/// but is not a keystroke.
pub(crate) fn is_modifier_key(key_code: Keycode) -> bool {
    matches!(
        key_code,
        Keycode::ShiftLeft
            | Keycode::ShiftRight
            | Keycode::CtrlLeft
            | Keycode::CtrlRight
            | Keycode::AltLeft
            | Keycode::AltRight
            | Keycode::MetaLeft
            | Keycode::MetaRight
            | Keycode::CapsLock
            | Keycode::Function
            | Keycode::Sym
    )
}

/// The gpui name for a key Android reports by code rather than by a
/// printable character, if it has one.
fn named_key(key_code: Keycode) -> Option<(&'static str, Option<&'static str>)> {
    Some(match key_code {
        Keycode::Enter | Keycode::NumpadEnter => ("enter", Some("\n")),
        Keycode::Escape => ("escape", None),
        // The system back button; apps treat it as escape does on a
        // desktop. Left unhandled it still finishes the activity.
        Keycode::Back => ("escape", None),
        Keycode::Del => ("backspace", None),
        Keycode::ForwardDel => ("delete", None),
        Keycode::Tab => ("tab", Some("\t")),
        Keycode::Space => ("space", Some(" ")),
        Keycode::Insert => ("insert", None),
        Keycode::MoveHome => ("home", None),
        Keycode::MoveEnd => ("end", None),
        Keycode::PageUp => ("pageup", None),
        Keycode::PageDown => ("pagedown", None),
        Keycode::DpadUp => ("up", None),
        Keycode::DpadDown => ("down", None),
        Keycode::DpadLeft => ("left", None),
        Keycode::DpadRight => ("right", None),
        Keycode::F1 => ("f1", None),
        Keycode::F2 => ("f2", None),
        Keycode::F3 => ("f3", None),
        Keycode::F4 => ("f4", None),
        Keycode::F5 => ("f5", None),
        Keycode::F6 => ("f6", None),
        Keycode::F7 => ("f7", None),
        Keycode::F8 => ("f8", None),
        Keycode::F9 => ("f9", None),
        Keycode::F10 => ("f10", None),
        Keycode::F11 => ("f11", None),
        Keycode::F12 => ("f12", None),
        _ => return None,
    })
}

/// The character a key produces under `meta`, per the device's map. A
/// dead key reports its accent on its own; composition with the next key
/// is not tracked.
fn key_character(app: &AndroidApp, event: &KeyEvent, meta: MetaState) -> Option<char> {
    let key_code: u32 = event.key_code().into();
    super::jni::key_character(app, event.device_id(), key_code as i32, meta.0 as i32)
        .ok()
        .flatten()
}

/// Builds the keystroke for a key event, or `None` for a bare modifier or
/// a key that produces nothing gpui can name (media keys, volume).
pub(crate) fn keystroke_from_key_event(app: &AndroidApp, event: &KeyEvent) -> Option<Keystroke> {
    let key_code = event.key_code();
    if is_modifier_key(key_code) {
        return None;
    }
    let meta = event.meta_state();
    let mut modifiers = modifiers_from_meta_state(meta);

    if let Some((name, key_char)) = named_key(key_code) {
        let key_char = key_char.filter(|_| !modifiers.control && !modifiers.platform);
        return Some(Keystroke {
            modifiers,
            key: name.to_string(),
            key_char: key_char.map(str::to_string),
        });
    }

    // A printable key. `key` is the unshifted character for letters (so a
    // binding on shift-a matches), the shifted one for symbols (a binding
    // on "!" rather than shift-1), and `key_char` is the text a text field
    // would receive.
    let typed = key_character(app, event, meta);
    // The unshifted character ignores every modifier but caps lock, which
    // does not change what a key is called.
    let unshifted = key_character(app, event, MetaState(0));
    let base = unshifted.or(typed)?;
    let key = if modifiers.shift && base.is_ascii_lowercase() {
        base.to_string()
    } else if modifiers.shift && typed.is_some_and(|c| c != base) {
        modifiers.shift = false;
        typed.unwrap().to_string()
    } else {
        base.to_string()
    };
    let key_char = typed
        .filter(|c| !c.is_control() && !modifiers.control && !modifiers.platform)
        .map(|c| c.to_string());
    Some(Keystroke {
        modifiers,
        key,
        key_char,
    })
}
