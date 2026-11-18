//! Minimal scrcpy 3.x wire protocol (video/audio packet headers + control messages).
//!
//! Reference: scrcpy v3.3 `app/src/control_msg.c` and
//! `server/.../device/Streamer.java`. All multi-byte fields are big-endian.

use crate::hid::{CanonicalKey, KeyboardModifiers};

pub const VIDEO_CODEC_H264: u32 = 0x6832_3634; // "h264"
pub const AUDIO_CODEC_OPUS: u32 = 0x6f70_7573; // "opus"

/// Set on the 8-byte PTS/flags field when the payload is codec metadata.
pub const PACKET_FLAG_CONFIG: u64 = 1 << 63;
/// Set when the payload is an independent frame (IDR for H.264).
pub const PACKET_FLAG_KEY_FRAME: u64 = 1 << 62;

/// scrcpy's well-known pointer id for a generic finger.
pub const POINTER_ID_FINGER: u64 = u64::MAX - 1;

pub const MAX_TEXT_LEN: usize = 300;

#[derive(Debug, Clone, PartialEq)]
pub enum ControlCommand {
    Keycode {
        action: u8,
        keycode: u32,
        repeat: u32,
        meta_state: u32,
    },
    Text(String),
    Touch {
        action: u8,
        pointer_id: u64,
        x: u32,
        y: u32,
        screen_width: u16,
        screen_height: u16,
        pressure: f32,
        action_button: u32,
        buttons: u32,
    },
    Scroll {
        x: u32,
        y: u32,
        screen_width: u16,
        screen_height: u16,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    },
    ResetVideo,
    RotateDevice,
    SetDisplayPower(bool),
}

impl ControlCommand {
    pub fn serialize(&self) -> Vec<u8> {
        match self {
            Self::Keycode {
                action,
                keycode,
                repeat,
                meta_state,
            } => {
                let mut out = Vec::with_capacity(14);
                out.push(0); // TYPE_INJECT_KEYCODE
                out.push(*action);
                push_u32be(&mut out, *keycode);
                push_u32be(&mut out, *repeat);
                push_u32be(&mut out, *meta_state);
                out
            }
            Self::Text(text) => {
                let text = truncate_utf8(text, MAX_TEXT_LEN);
                let mut out = Vec::with_capacity(5 + text.len());
                out.push(1); // TYPE_INJECT_TEXT
                push_u32be(&mut out, text.len() as u32);
                out.extend_from_slice(text.as_bytes());
                out
            }
            Self::Touch {
                action,
                pointer_id,
                x,
                y,
                screen_width,
                screen_height,
                pressure,
                action_button,
                buttons,
            } => {
                let mut out = Vec::with_capacity(32);
                out.push(2); // TYPE_INJECT_TOUCH_EVENT
                out.push(*action);
                push_u64be(&mut out, *pointer_id);
                push_u32be(&mut out, *x);
                push_u32be(&mut out, *y);
                push_u16be(&mut out, *screen_width);
                push_u16be(&mut out, *screen_height);
                push_u16be(&mut out, float_to_u16fp(*pressure));
                push_u32be(&mut out, *action_button);
                push_u32be(&mut out, *buttons);
                out
            }
            Self::Scroll {
                x,
                y,
                screen_width,
                screen_height,
                hscroll,
                vscroll,
                buttons,
            } => {
                let mut out = Vec::with_capacity(21);
                out.push(3); // TYPE_INJECT_SCROLL_EVENT
                push_u32be(&mut out, *x);
                push_u32be(&mut out, *y);
                push_u16be(&mut out, *screen_width);
                push_u16be(&mut out, *screen_height);
                push_u16be(&mut out, float_to_i16fp(*hscroll) as u16);
                push_u16be(&mut out, float_to_i16fp(*vscroll) as u16);
                push_u32be(&mut out, *buttons);
                out
            }
            Self::ResetVideo => vec![17],
            Self::RotateDevice => vec![11],
            Self::SetDisplayPower(on) => vec![10, u8::from(*on)],
        }
    }
}

fn push_u16be(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u32be(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u64be(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn truncate_utf8(text: &str, max_len: usize) -> &str {
    if text.len() <= max_len {
        return text;
    }
    let mut end = max_len;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// scrcpy's unsigned 16-bit fixed point: `f * 2^16`, clamped to 0xffff.
pub fn float_to_u16fp(value: f32) -> u16 {
    let scaled = (value.clamp(0.0, 1.0) * 65536.0) as u32;
    scaled.min(0xffff) as u16
}

/// scrcpy's signed 16-bit fixed point: `f * 2^15`, clamped to [-0x8000, 0x7fff].
pub fn float_to_i16fp(value: f32) -> i16 {
    let scaled = (value.clamp(-1.0, 1.0) * 32768.0) as i32;
    scaled.clamp(-0x8000, 0x7fff) as i16
}

/// Parse the 12-byte video header: codec id, width, height (big-endian).
pub fn parse_video_header(header: &[u8]) -> Option<(u32, u32, u32)> {
    if header.len() < 12 {
        return None;
    }
    let codec = u32::from_be_bytes(header[0..4].try_into().ok()?);
    let width = u32::from_be_bytes(header[4..8].try_into().ok()?);
    let height = u32::from_be_bytes(header[8..12].try_into().ok()?);
    Some((codec, width, height))
}

/// Parse the 12-byte frame header: 8-byte PTS/flags + 4-byte payload size.
pub fn parse_frame_header(header: &[u8]) -> Option<(u64, u32)> {
    if header.len() < 12 {
        return None;
    }
    let pts_and_flags = u64::from_be_bytes(header[0..8].try_into().ok()?);
    let size = u32::from_be_bytes(header[8..12].try_into().ok()?);
    Some((pts_and_flags, size))
}

pub fn is_config_packet(pts_and_flags: u64) -> bool {
    pts_and_flags & PACKET_FLAG_CONFIG != 0
}

pub fn is_key_frame(pts_and_flags: u64) -> bool {
    pts_and_flags & PACKET_FLAG_KEY_FRAME != 0
}

/// PTS is stored in the low 62 bits, in microseconds.
pub fn packet_pts_us(pts_and_flags: u64) -> i64 {
    (pts_and_flags & ((1u64 << 62) - 1)) as i64
}

/// Android `KeyEvent` meta-state bits used by scrcpy.
pub fn android_meta_state(modifiers: &KeyboardModifiers) -> u32 {
    let mut meta = 0u32;
    if modifiers.left_shift || modifiers.right_shift {
        meta |= 0x0000_0001; // META_SHIFT_ON
    }
    if modifiers.left_alt || modifiers.right_alt {
        meta |= 0x0000_0002; // META_ALT_ON
    }
    if modifiers.left_ctrl || modifiers.right_ctrl {
        meta |= 0x0000_1000; // META_CTRL_ON
    }
    if modifiers.left_meta || modifiers.right_meta {
        meta |= 0x0001_0000; // META_META_ON
    }
    meta
}

/// Map a browser/One-KVM canonical key to an Android `KEYCODE_*`.
/// Modifier keys are mapped too, matching scrcpy's client behavior.
pub fn android_keycode(key: CanonicalKey) -> Option<u32> {
    use CanonicalKey::*;
    Some(match key {
        KeyA => 29,
        KeyB => 30,
        KeyC => 31,
        KeyD => 32,
        KeyE => 33,
        KeyF => 34,
        KeyG => 35,
        KeyH => 36,
        KeyI => 37,
        KeyJ => 38,
        KeyK => 39,
        KeyL => 40,
        KeyM => 41,
        KeyN => 42,
        KeyO => 43,
        KeyP => 44,
        KeyQ => 45,
        KeyR => 46,
        KeyS => 47,
        KeyT => 48,
        KeyU => 49,
        KeyV => 50,
        KeyW => 51,
        KeyX => 52,
        KeyY => 53,
        KeyZ => 54,
        Digit0 => 7,
        Digit1 => 8,
        Digit2 => 9,
        Digit3 => 10,
        Digit4 => 11,
        Digit5 => 12,
        Digit6 => 13,
        Digit7 => 14,
        Digit8 => 15,
        Digit9 => 16,
        Enter => 66,
        Escape => 111,
        Backspace => 67,
        Tab => 61,
        Space => 62,
        Minus => 69,
        Equal => 70,
        BracketLeft => 71,
        BracketRight => 72,
        Backslash | IntlBackslash => 73,
        Semicolon => 74,
        Quote => 75,
        Backquote => 68,
        Comma => 55,
        Period => 56,
        Slash => 76,
        CapsLock => 115,
        F1 => 131,
        F2 => 132,
        F3 => 133,
        F4 => 134,
        F5 => 135,
        F6 => 136,
        F7 => 137,
        F8 => 138,
        F9 => 139,
        F10 => 140,
        F11 => 141,
        F12 => 142,
        PrintScreen => 120,
        ScrollLock => 116,
        Pause => 121,
        Insert => 124,
        // Physical Home/End keys move the caret, they are not the Home button
        // (the toolbar uses `keys::HOME` for that).
        Home => 122,
        PageUp => 92,
        Delete => 112,
        End => 123,
        PageDown => 93,
        ArrowRight => 22,
        ArrowLeft => 21,
        ArrowDown => 20,
        ArrowUp => 19,
        NumLock => 143,
        NumpadDivide => 154,
        NumpadMultiply => 155,
        NumpadSubtract => 156,
        NumpadAdd => 157,
        NumpadEnter => 160,
        Numpad1 => 145,
        Numpad2 => 146,
        Numpad3 => 147,
        Numpad4 => 148,
        Numpad5 => 149,
        Numpad6 => 150,
        Numpad7 => 151,
        Numpad8 => 152,
        Numpad9 => 153,
        Numpad0 => 144,
        NumpadDecimal => 158,
        ContextMenu => 82,
        ControlLeft => 113,
        ShiftLeft => 59,
        AltLeft => 57,
        MetaLeft => 117,
        ControlRight => 114,
        ShiftRight => 60,
        AltRight => 58,
        MetaRight => 118,
        F13 | F14 | F15 | F16 | F17 | F18 | F19 | F20 | F21 | F22 | F23 | F24 => return None,
    })
}

/// Android `KEYCODE_*` used by the phone toolbar.
pub mod keys {
    pub const HOME: u32 = 3;
    pub const BACK: u32 = 4;
    pub const VOLUME_UP: u32 = 24;
    pub const VOLUME_DOWN: u32 = 25;
    pub const POWER: u32 = 26;
    pub const APP_SWITCH: u32 = 187;
    pub const WAKEUP: u32 = 224;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycode_message_is_14_bytes_big_endian() {
        let msg = ControlCommand::Keycode {
            action: 0,
            keycode: keys::HOME,
            repeat: 0,
            meta_state: 0x1000,
        };
        let bytes = msg.serialize();
        assert_eq!(bytes.len(), 14);
        assert_eq!(bytes[0], 0);
        assert_eq!(&bytes[2..6], &3u32.to_be_bytes());
        assert_eq!(&bytes[10..14], &0x1000u32.to_be_bytes());
    }

    #[test]
    fn touch_message_is_32_bytes() {
        let msg = ControlCommand::Touch {
            action: 0,
            pointer_id: POINTER_ID_FINGER,
            x: 10,
            y: 20,
            screen_width: 1600,
            screen_height: 900,
            pressure: 1.0,
            action_button: 0,
            buttons: 0,
        };
        let bytes = msg.serialize();
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[0], 2);
        assert_eq!(bytes[1], 0);
        assert_eq!(&bytes[10..14], &10u32.to_be_bytes());
        assert_eq!(&bytes[14..18], &20u32.to_be_bytes());
        assert_eq!(&bytes[18..20], &1600u16.to_be_bytes());
        assert_eq!(&bytes[20..22], &900u16.to_be_bytes());
        assert_eq!(&bytes[22..24], &0xffffu16.to_be_bytes());
    }

    #[test]
    fn scroll_message_is_21_bytes() {
        let msg = ControlCommand::Scroll {
            x: 1,
            y: 2,
            screen_width: 300,
            screen_height: 400,
            hscroll: 0.0,
            vscroll: 1.0,
            buttons: 0,
        };
        let bytes = msg.serialize();
        assert_eq!(bytes.len(), 21);
        assert_eq!(bytes[0], 3);
        assert_eq!(&bytes[13..15], &0u16.to_be_bytes());
        assert_eq!(&bytes[15..17], &0x7fffu16.to_be_bytes());
    }

    #[test]
    fn parses_video_header() {
        let mut header = Vec::new();
        header.extend_from_slice(&VIDEO_CODEC_H264.to_be_bytes());
        header.extend_from_slice(&1600u32.to_be_bytes());
        header.extend_from_slice(&720u32.to_be_bytes());
        assert_eq!(
            parse_video_header(&header),
            Some((VIDEO_CODEC_H264, 1600, 720))
        );
    }

    #[test]
    fn packet_flags_and_pts() {
        let pts = 1_234_567u64;
        let value = pts | PACKET_FLAG_KEY_FRAME;
        assert!(is_key_frame(value));
        assert!(!is_config_packet(value));
        assert_eq!(packet_pts_us(value), pts as i64);
    }

    #[test]
    fn maps_common_keys() {
        assert_eq!(android_keycode(CanonicalKey::KeyA), Some(29));
        assert_eq!(android_keycode(CanonicalKey::Digit0), Some(7));
        assert_eq!(android_keycode(CanonicalKey::Enter), Some(66));
        assert_eq!(android_keycode(CanonicalKey::ControlLeft), Some(113));
        assert_eq!(android_keycode(CanonicalKey::F24), None);
    }

    #[test]
    fn truncates_text_on_char_boundary() {
        let text = "a".repeat(MAX_TEXT_LEN + 10);
        let msg = ControlCommand::Text(text);
        let bytes = msg.serialize();
        assert_eq!(bytes.len(), 5 + MAX_TEXT_LEN);
    }
}
