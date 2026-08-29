//! macOS virtual keycodes to HID usage codes.
//!
//! Both are *positional* -- they name a physical key, not the character it
//! produces -- so this is a fixed table rather than anything layout-dependent.
//! That is also the reason scurry cannot faithfully forward arbitrary text: we
//! send the key that was pressed, and the target decides what it means under
//! its own keymap. A Dvorak host driving a QWERTY target types QWERTY.

/// HID usage code for a macOS virtual keycode, or `None` for keys we do not
/// forward.
pub fn hid_usage(vk: i64) -> Option<u8> {
    Some(match vk {
        // Letters, in macOS's ANSI ordering rather than alphabetical.
        0x00 => 0x04, // A
        0x0B => 0x05, // B
        0x08 => 0x06, // C
        0x02 => 0x07, // D
        0x0E => 0x08, // E
        0x03 => 0x09, // F
        0x05 => 0x0A, // G
        0x04 => 0x0B, // H
        0x22 => 0x0C, // I
        0x26 => 0x0D, // J
        0x28 => 0x0E, // K
        0x25 => 0x0F, // L
        0x2E => 0x10, // M
        0x2D => 0x11, // N
        0x1F => 0x12, // O
        0x23 => 0x13, // P
        0x0C => 0x14, // Q
        0x0F => 0x15, // R
        0x01 => 0x16, // S
        0x11 => 0x17, // T
        0x20 => 0x18, // U
        0x09 => 0x19, // V
        0x0D => 0x1A, // W
        0x07 => 0x1B, // X
        0x10 => 0x1C, // Y
        0x06 => 0x1D, // Z

        // Digits. HID puts 1..9 at 0x1E..0x26 and zero after them, not before.
        0x12 => 0x1E, // 1
        0x13 => 0x1F, // 2
        0x14 => 0x20, // 3
        0x15 => 0x21, // 4
        0x17 => 0x22, // 5
        0x16 => 0x23, // 6
        0x1A => 0x24, // 7
        0x1C => 0x25, // 8
        0x19 => 0x26, // 9
        0x1D => 0x27, // 0

        0x24 => 0x28, // Return
        0x35 => 0x29, // Escape
        0x33 => 0x2A, // Delete (Backspace)
        0x30 => 0x2B, // Tab
        0x31 => 0x2C, // Space
        0x1B => 0x2D, // Minus
        0x18 => 0x2E, // Equal
        0x21 => 0x2F, // Left bracket
        0x1E => 0x30, // Right bracket
        0x2A => 0x31, // Backslash
        0x29 => 0x33, // Semicolon
        0x27 => 0x34, // Quote
        0x32 => 0x35, // Grave
        0x2B => 0x36, // Comma
        0x2F => 0x37, // Period
        0x2C => 0x38, // Slash
        0x39 => 0x39, // Caps Lock

        0x7A => 0x3A, // F1
        0x78 => 0x3B, // F2
        0x63 => 0x3C, // F3
        0x76 => 0x3D, // F4
        0x60 => 0x3E, // F5
        0x61 => 0x3F, // F6
        0x62 => 0x40, // F7
        0x64 => 0x41, // F8
        0x65 => 0x42, // F9
        0x6D => 0x43, // F10
        0x67 => 0x44, // F11
        0x6F => 0x45, // F12

        0x72 => 0x49, // Insert / Help
        0x73 => 0x4A, // Home
        0x74 => 0x4B, // Page Up
        0x75 => 0x4C, // Forward Delete
        0x77 => 0x4D, // End
        0x79 => 0x4E, // Page Down
        0x7C => 0x4F, // Right arrow
        0x7B => 0x50, // Left arrow
        0x7D => 0x51, // Down arrow
        0x7E => 0x52, // Up arrow

        // Keypad.
        0x47 => 0x53, // Clear / Num Lock
        0x4B => 0x54, // Keypad divide
        0x43 => 0x55, // Keypad multiply
        0x4E => 0x56, // Keypad minus
        0x45 => 0x57, // Keypad plus
        0x4C => 0x58, // Keypad enter
        0x53 => 0x59, // Keypad 1
        0x54 => 0x5A,
        0x55 => 0x5B,
        0x56 => 0x5C,
        0x57 => 0x5D,
        0x58 => 0x5E,
        0x59 => 0x5F,
        0x5B => 0x60,
        0x5C => 0x61, // Keypad 9
        0x52 => 0x62, // Keypad 0
        0x41 => 0x63, // Keypad period

        // Modifiers are not sent as keycodes; they travel in the modifier byte.
        // Listing them here so they are visibly excluded rather than silently
        // falling through as unknown.
        0x37 | 0x36 | 0x38 | 0x3C | 0x3A | 0x3D | 0x3B | 0x3E => return None,

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits_map_to_the_right_usages() {
        assert_eq!(hid_usage(0x00), Some(0x04), "A");
        assert_eq!(hid_usage(0x06), Some(0x1D), "Z");
        // HID orders digits 1..9 then 0, which is the easy one to get wrong.
        assert_eq!(hid_usage(0x12), Some(0x1E), "1");
        assert_eq!(hid_usage(0x1D), Some(0x27), "0");
    }

    #[test]
    fn modifiers_are_excluded() {
        // They belong in the modifier byte. Emitting them as keycodes would
        // send both, and some hosts treat that as a stuck key.
        for vk in [0x37, 0x36, 0x38, 0x3C, 0x3A, 0x3D, 0x3B, 0x3E] {
            assert_eq!(hid_usage(vk), None, "vk {vk:#04x} must not be a keycode");
        }
    }

    #[test]
    fn the_table_has_no_duplicate_usages() {
        // A duplicate would mean two physical keys producing the same code,
        // which is invisible until someone presses the wrong one.
        let mut seen = std::collections::HashMap::new();
        for vk in 0..0x80i64 {
            if let Some(u) = hid_usage(vk) {
                if let Some(prev) = seen.insert(u, vk) {
                    panic!("usage {u:#04x} claimed by both {prev:#04x} and {vk:#04x}");
                }
            }
        }
    }

    #[test]
    fn unknown_keys_are_dropped_not_guessed() {
        assert_eq!(hid_usage(0x7F), None);
        assert_eq!(hid_usage(999), None);
    }
}
