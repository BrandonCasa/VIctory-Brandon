//! Key state delivered to the game's window procedure for GameInput polling.

use std::sync::Mutex;
use windows::Win32::UI::WindowsAndMessaging::{
    WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_NCDESTROY, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

static KEYS: Mutex<(usize, [bool; 256])> = Mutex::new((0, [false; 256]));

/// Called by both the native title window and the fallback presentation window.
pub fn message(window: usize, message: u32, key: usize, details: isize) {
    match message {
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP if key < 256 => {
            let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
            let mut keys = KEYS.lock().unwrap_or_else(|e| e.into_inner());
            if keys.0 != window {
                if !down { return; }
                *keys = (window, [false; 256]);
            }
            // GameInput exposes left and right modifiers individually.
            let key = match key {
                0x10 => 0xa0 + usize::from((details >> 16) & 0xff == 0x36),
                0x11 => 0xa2 + usize::from(details & (1 << 24) != 0),
                0x12 => 0xa4 + usize::from(details & (1 << 24) != 0),
                _ => key,
            };
            keys.1[key] = down;
        }
        WM_KILLFOCUS | WM_NCDESTROY => {
            let mut keys = KEYS.lock().unwrap_or_else(|e| e.into_inner());
            if keys.0 == window { *keys = (0, [false; 256]); }
        }
        _ => {}
    }
}

/// Window-directed key events, cleared on focus loss and destruction.
pub fn held() -> [bool; 256] {
    let keys = KEYS.lock().unwrap_or_else(|e| e.into_inner());
    keys.1
}
