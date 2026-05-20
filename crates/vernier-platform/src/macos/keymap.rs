//! macOS virtual keycode <-> Accelerator / XKB keysym mapping.
//!
//! `Accelerator::register_hotkey` arrives on the daemon side as an
//! (modifiers, Key) pair, but Carbon's `RegisterEventHotKey`
//! expects a raw kVK_* virtual keycode + a Carbon-modifier bitmask.
//! Likewise, AppKit `NSEvent::keyCode` returns a raw vkey that we
//! need to surface as an XKB keysym in [`PlatformEvent::KeyboardKey`]
//! to keep the daemon's existing keysym-based dispatch working.

use crate::{Key, Modifiers};

/// Carbon modifier masks (from `Carbon/HIToolbox/Events.h`).
pub(crate) mod carbon_mods {
    pub const CMD: u32 = 1 << 8; // cmdKey
    pub const SHIFT: u32 = 1 << 9; // shiftKey
    pub const OPTION: u32 = 1 << 11; // optionKey (Alt)
    pub const CONTROL: u32 = 1 << 12; // controlKey
}

pub(crate) fn accelerator_to_carbon(modifiers: Modifiers, key: Key) -> Option<(u32, u32)> {
    let mut mods = 0u32;
    if modifiers.contains(Modifiers::SHIFT) {
        mods |= carbon_mods::SHIFT;
    }
    if modifiers.contains(Modifiers::CTRL) {
        mods |= carbon_mods::CONTROL;
    }
    if modifiers.contains(Modifiers::ALT) {
        mods |= carbon_mods::OPTION;
    }
    if modifiers.contains(Modifiers::META) {
        mods |= carbon_mods::CMD;
    }
    let vkey = key_to_vkey(key)?;
    Some((vkey, mods))
}

/// Map our portable [`Key`] to a macOS virtual keycode (kVK_*).
fn key_to_vkey(key: Key) -> Option<u32> {
    Some(match key {
        Key::Char(c) => match c.to_ascii_lowercase() {
            'a' => 0x00,
            's' => 0x01,
            'd' => 0x02,
            'f' => 0x03,
            'h' => 0x04,
            'g' => 0x05,
            'z' => 0x06,
            'x' => 0x07,
            'c' => 0x08,
            'v' => 0x09,
            'b' => 0x0B,
            'q' => 0x0C,
            'w' => 0x0D,
            'e' => 0x0E,
            'r' => 0x0F,
            'y' => 0x10,
            't' => 0x11,
            '1' => 0x12,
            '2' => 0x13,
            '3' => 0x14,
            '4' => 0x15,
            '6' => 0x16,
            '5' => 0x17,
            '=' => 0x18,
            '9' => 0x19,
            '7' => 0x1A,
            '-' => 0x1B,
            '8' => 0x1C,
            '0' => 0x1D,
            ']' => 0x1E,
            'o' => 0x1F,
            'u' => 0x20,
            '[' => 0x21,
            'i' => 0x22,
            'p' => 0x23,
            'l' => 0x25,
            'j' => 0x26,
            '\'' => 0x27,
            'k' => 0x28,
            ';' => 0x29,
            '\\' => 0x2A,
            ',' => 0x2B,
            '/' => 0x2C,
            'n' => 0x2D,
            'm' => 0x2E,
            '.' => 0x2F,
            '`' => 0x32,
            '+' => 0x18, // share '=' (kVK_ANSI_Equal) — Shift turns it into +
            '_' => 0x1B, // share '-' similarly
            _ => return None,
        },
        Key::F(n) => match n {
            1 => 0x7A,
            2 => 0x78,
            3 => 0x63,
            4 => 0x76,
            5 => 0x60,
            6 => 0x61,
            7 => 0x62,
            8 => 0x64,
            9 => 0x65,
            10 => 0x6D,
            11 => 0x67,
            12 => 0x6F,
            13 => 0x69,
            14 => 0x6B,
            15 => 0x71,
            16 => 0x6A,
            17 => 0x40,
            18 => 0x4F,
            19 => 0x50,
            20 => 0x5A,
            _ => return None,
        },
        Key::Escape => 0x35,
        Key::Enter => 0x24,
        Key::Space => 0x31,
        Key::Tab => 0x30,
        Key::Backspace => 0x33,
        Key::Delete => 0x75,
        Key::Up => 0x7E,
        Key::Down => 0x7D,
        Key::Left => 0x7B,
        Key::Right => 0x7C,
    })
}

/// Translate a macOS virtual keycode to the equivalent XKB keysym
/// the daemon's keyboard handlers already understand. Returns 0
/// for keys we don't map (the daemon treats unknown keysyms as
/// inert).
pub(crate) fn vkey_to_xkb_keysym(vkey: u16) -> u32 {
    // Subset covers everything the daemon's keyboard handlers
    // dispatch on. XKB symbol values are from
    // `/usr/include/X11/keysymdef.h`.
    match vkey {
        // Letters use ASCII codepoints — matches XKB latin.
        0x00 => b'a' as u32,
        0x01 => b's' as u32,
        0x02 => b'd' as u32,
        0x03 => b'f' as u32,
        0x04 => b'h' as u32,
        0x05 => b'g' as u32,
        0x06 => b'z' as u32,
        0x07 => b'x' as u32,
        0x08 => b'c' as u32,
        0x09 => b'v' as u32,
        0x0B => b'b' as u32,
        0x0C => b'q' as u32,
        0x0D => b'w' as u32,
        0x0E => b'e' as u32,
        0x0F => b'r' as u32,
        0x10 => b'y' as u32,
        0x11 => b't' as u32,
        0x1F => b'o' as u32,
        0x20 => b'u' as u32,
        0x22 => b'i' as u32,
        0x23 => b'p' as u32,
        0x25 => b'l' as u32,
        0x26 => b'j' as u32,
        0x28 => b'k' as u32,
        0x2D => b'n' as u32,
        0x2E => b'm' as u32,
        // Digits.
        0x12 => b'1' as u32,
        0x13 => b'2' as u32,
        0x14 => b'3' as u32,
        0x15 => b'4' as u32,
        0x16 => b'6' as u32,
        0x17 => b'5' as u32,
        0x19 => b'9' as u32,
        0x1A => b'7' as u32,
        0x1C => b'8' as u32,
        0x1D => b'0' as u32,
        // Punctuation.
        0x18 => b'=' as u32,
        0x1B => b'-' as u32,
        0x1E => b']' as u32,
        0x21 => b'[' as u32,
        0x27 => b'\'' as u32,
        0x29 => b';' as u32,
        0x2A => b'\\' as u32,
        0x2B => b',' as u32,
        0x2C => b'/' as u32,
        0x2F => b'.' as u32,
        0x32 => b'`' as u32,
        // Whitespace + control.
        0x24 => 0xFF0D, // Return
        0x30 => 0xFF09, // Tab
        0x31 => 0x0020, // space
        0x33 => 0xFF08, // BackSpace
        0x35 => 0xFF1B, // Escape
        0x75 => 0xFFFF, // Delete
        // Arrows.
        0x7B => 0xFF51, // Left
        0x7C => 0xFF53, // Right
        0x7D => 0xFF54, // Down
        0x7E => 0xFF52, // Up
        // F1..F12 — matches XK_F1 (0xFFBE) onwards.
        0x7A => 0xFFBE,
        0x78 => 0xFFBF,
        0x63 => 0xFFC0,
        0x76 => 0xFFC1,
        0x60 => 0xFFC2,
        0x61 => 0xFFC3,
        0x62 => 0xFFC4,
        0x64 => 0xFFC5,
        0x65 => 0xFFC6,
        0x6D => 0xFFC7,
        0x67 => 0xFFC8,
        0x6F => 0xFFC9,
        _ => 0,
    }
}
