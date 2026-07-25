//! Thin Win32 keyboard-state helpers.
//!
//! Two things the dictation loop needs and no crate in the tree exposes:
//!
//! * reading the CapsLock *toggle* state, so it can be put back by comparison
//!   instead of by injecting a fixed number of compensating taps;
//! * knowing whether Ctrl/Shift/Alt/Win are still physically held, so text is
//!   never injected into a modifier-armed keyboard.
//!
//! The second one matters because `global-hotkey` polls only the hotkey's main
//! key for release: with `Ctrl+E`, letting go of `E` alone reports "released"
//! while Ctrl is still down. Typing then arrives as Ctrl+letter shortcuts, and
//! a clipboard paste becomes Ctrl+Shift+V.

use std::time::{Duration, Instant};

/// Poll interval while waiting for modifiers to come up.
const MODIFIER_POLL: Duration = Duration::from_millis(10);

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, GetKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
        KEYEVENTF_KEYUP, VK_CAPITAL, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };

    /// Modifiers that change what a keystroke means in the target app.
    const MODIFIERS: [u16; 5] = [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN];

    pub fn caps_on() -> Option<bool> {
        // The low-order bit of GetKeyState is the toggle state for lock keys.
        Some(unsafe { GetKeyState(VK_CAPITAL as i32) } & 1 != 0)
    }

    pub fn modifiers_held() -> bool {
        MODIFIERS
            .iter()
            // The high-order bit means "physically down right now".
            .any(|vk| unsafe { GetAsyncKeyState(*vk as i32) } as u16 & 0x8000 != 0)
    }

    /// Press and release CapsLock in one `SendInput` batch.
    ///
    /// Synchronous and microsecond-cheap — unlike spinning up an `Enigo`, which
    /// took long enough that a user's next real key press could land inside the
    /// window and be mistaken for our own injected one.
    pub fn tap_capslock() {
        let mut inputs = [key_input(VK_CAPITAL, false), key_input(VK_CAPITAL, true)];
        unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_mut_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
    }

    fn key_input(vk: u16, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// Caps state is unknown off Windows, which disables caps restoration
    /// entirely (`caps_restore_needed` treats `None` as "leave it alone").
    pub fn caps_on() -> Option<bool> {
        None
    }
    pub fn modifiers_held() -> bool {
        false
    }
    pub fn tap_capslock() {}
}

pub use imp::{caps_on, modifiers_held, tap_capslock};

/// Block until no modifier is physically held, or `max` elapses.
///
/// Returns true if the keyboard came up clean. Timing out is not fatal — the
/// user may genuinely be leaning on Ctrl — so the caller injects anyway rather
/// than dropping the transcript.
pub fn wait_for_modifiers_release(max: Duration) -> bool {
    let deadline = Instant::now() + max;
    loop {
        if !modifiers_held() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(MODIFIER_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no modifier held this returns immediately; the point of the test is
    /// that it is bounded either way and never blocks the commit forever.
    #[test]
    fn modifier_wait_is_bounded() {
        let start = Instant::now();
        let clean = wait_for_modifiers_release(Duration::from_millis(120));
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(400),
            "waited {elapsed:?} for a 120ms budget"
        );
        if !clean {
            assert!(elapsed >= Duration::from_millis(120));
        }
    }

    #[test]
    fn caps_query_is_consistent() {
        // Reading twice in a row must agree — a smoke test that we are reading
        // the toggle bit and not something that changes under us.
        assert_eq!(caps_on(), caps_on());
    }
}
