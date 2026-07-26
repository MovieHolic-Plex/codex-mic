//! Dictation lifecycle state machine.
//!
//! Every path that can start or stop a recording — hotkey press, hotkey
//! release, silence auto-stop, realtime failure watchdog, and the "the user let
//! go while we were still connecting" catch-up — funnels through one machine
//! guarded by a plain `Mutex`. Transitions are synchronous and never await, so
//! two paths can never both decide to start (or both decide to stop) the same
//! recording.
//!
//! This replaces the old ad-hoc pile of atomics (`TOGGLE_LOCK`,
//! `DICTATION_KEY_DOWN`, `INTENT_RECORDING`, a 400 ms debounce), whose gaps
//! produced duplicate captures, orphaned PCM pumps, and double commits.

use crate::config::ActivationMode;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri_plugin_global_shortcut::Code;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// Microphone opening / realtime session connecting. Can take seconds.
    Starting,
    Recording,
    /// Draining audio, waiting for the transcript, injecting.
    Stopping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Ignore,
}

/// How long hotkey events are ignored after we inject a synthetic CapsLock tap.
///
/// `RegisterHotKey` sees injected input, so our own caps-restore keystroke
/// comes straight back as a press+release pair. A time window (rather than a
/// counter) self-heals: if the release event never materialises — the release
/// side of `global-hotkey` is a 50 ms `GetAsyncKeyState` poll and can miss a
/// tap entirely — nothing stays stuck.
pub const SYNTHETIC_SUPPRESS: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Machine {
    phase: Phase,
    /// A stop was asked for while we were still in `Starting`. Honored the
    /// moment the recording actually goes live.
    stop_pending: bool,
    /// The activation mode that was in force when the key went down. Reading
    /// the config again on release would misinterpret the release if the user
    /// changed modes mid-press.
    active_mode: Option<ActivationMode>,
}

impl Machine {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            stop_pending: false,
            active_mode: None,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn press(&mut self, mode: ActivationMode) -> Action {
        self.active_mode = Some(mode);
        match (mode, self.phase) {
            (_, Phase::Idle) => {
                self.stop_pending = false;
                self.phase = Phase::Starting;
                Action::Start
            }
            (ActivationMode::Toggle, Phase::Recording) => {
                self.phase = Phase::Stopping;
                Action::Stop
            }
            // Toggle-off pressed before the recording finished coming up.
            (ActivationMode::Toggle, Phase::Starting) => {
                self.stop_pending = true;
                Action::Ignore
            }
            _ => Action::Ignore,
        }
    }

    pub fn release(&mut self) -> Action {
        if self.active_mode != Some(ActivationMode::PushToTalk) {
            return Action::Ignore;
        }
        self.stop_requested()
    }

    /// A stop from something other than the hotkey: silence auto-stop, the
    /// realtime failure watchdog, or an explicit command.
    pub fn stop_requested(&mut self) -> Action {
        match self.phase {
            Phase::Recording => {
                self.phase = Phase::Stopping;
                Action::Stop
            }
            Phase::Starting => {
                self.stop_pending = true;
                Action::Ignore
            }
            _ => Action::Ignore,
        }
    }

    /// The recording is now live. Returns `Stop` if a stop arrived while we
    /// were still starting — a quick tap, which must commit rather than leave a
    /// zombie recording running.
    pub fn started(&mut self) -> Action {
        if self.phase != Phase::Starting {
            return Action::Ignore;
        }
        if self.stop_pending {
            self.stop_pending = false;
            self.phase = Phase::Stopping;
            Action::Stop
        } else {
            self.phase = Phase::Recording;
            Action::Ignore
        }
    }

    /// Startup failed (no microphone, connect error). Back to idle.
    pub fn start_failed(&mut self) {
        self.phase = Phase::Idle;
        self.stop_pending = false;
    }

    pub fn stopped(&mut self) {
        self.phase = Phase::Idle;
        self.stop_pending = false;
    }
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

static MACHINE: Mutex<Machine> = Mutex::new(Machine::new());
static SUPPRESS_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// Run a transition against the process-wide machine.
pub fn with<R>(f: impl FnOnce(&mut Machine) -> R) -> R {
    let mut guard = MACHINE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut guard)
}

/// Ignore hotkey events for `window` — used around our own injected keystrokes.
pub fn suppress_for(window: Duration) {
    let mut guard = SUPPRESS_UNTIL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(Instant::now() + window);
}

pub fn suppressed() -> bool {
    let guard = SUPPRESS_UNTIL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    is_suppressed(*guard, Instant::now())
}

pub fn is_suppressed(until: Option<Instant>, now: Instant) -> bool {
    matches!(until, Some(t) if now < t)
}

/// Whether the configured dictation hotkey is a bare CapsLock.
///
/// Matched on the config string the user actually typed rather than on the
/// `Debug` rendering of a parsed `Shortcut`, which is a dependency-internal
/// format that can change under us.
pub fn hotkey_is_capslock(hotkey: &str) -> bool {
    hotkey.trim().eq_ignore_ascii_case("capslock")
}

/// True when the caps state changed across a dictation and must be put back.
pub fn caps_restore_needed(before: Option<bool>, after: Option<bool>) -> bool {
    matches!((before, after), (Some(a), Some(b)) if a != b)
}

/// Windows virtual-key code for a shortcut's main key.
///
/// Mirrors `global-hotkey`'s own `key_to_vk`, which is private to that crate.
/// We need the same number it hands to `RegisterHotKey` so we can ask Windows
/// directly whether the key is still down — see `winkeys::key_is_down` and the
/// release watcher in `lib.rs`. `None` means "not a key we can poll", which
/// disables the physical check rather than breaking the hotkey.
pub fn code_to_vk(code: &Code) -> Option<u16> {
    Some(match code {
        Code::KeyA => 0x41,
        Code::KeyB => 0x42,
        Code::KeyC => 0x43,
        Code::KeyD => 0x44,
        Code::KeyE => 0x45,
        Code::KeyF => 0x46,
        Code::KeyG => 0x47,
        Code::KeyH => 0x48,
        Code::KeyI => 0x49,
        Code::KeyJ => 0x4A,
        Code::KeyK => 0x4B,
        Code::KeyL => 0x4C,
        Code::KeyM => 0x4D,
        Code::KeyN => 0x4E,
        Code::KeyO => 0x4F,
        Code::KeyP => 0x50,
        Code::KeyQ => 0x51,
        Code::KeyR => 0x52,
        Code::KeyS => 0x53,
        Code::KeyT => 0x54,
        Code::KeyU => 0x55,
        Code::KeyV => 0x56,
        Code::KeyW => 0x57,
        Code::KeyX => 0x58,
        Code::KeyY => 0x59,
        Code::KeyZ => 0x5A,
        Code::Digit0 => 0x30,
        Code::Digit1 => 0x31,
        Code::Digit2 => 0x32,
        Code::Digit3 => 0x33,
        Code::Digit4 => 0x34,
        Code::Digit5 => 0x35,
        Code::Digit6 => 0x36,
        Code::Digit7 => 0x37,
        Code::Digit8 => 0x38,
        Code::Digit9 => 0x39,
        Code::Equal => 0xBB,
        Code::Comma => 0xBC,
        Code::Minus => 0xBD,
        Code::Period => 0xBE,
        Code::Semicolon => 0xBA,
        Code::Slash => 0xBF,
        Code::Backquote => 0xC0,
        Code::BracketLeft => 0xDB,
        Code::Backslash => 0xDC,
        Code::BracketRight => 0xDD,
        Code::Quote => 0xDE,
        Code::Backspace => 0x08,
        Code::Tab => 0x09,
        Code::Space => 0x20,
        Code::Enter | Code::NumpadEnter => 0x0D,
        Code::CapsLock => 0x14,
        Code::Escape => 0x1B,
        Code::PageUp => 0x21,
        Code::PageDown => 0x22,
        Code::End => 0x23,
        Code::Home => 0x24,
        Code::ArrowLeft => 0x25,
        Code::ArrowUp => 0x26,
        Code::ArrowRight => 0x27,
        Code::ArrowDown => 0x28,
        Code::PrintScreen => 0x2C,
        Code::Insert => 0x2D,
        Code::Delete => 0x2E,
        Code::F1 => 0x70,
        Code::F2 => 0x71,
        Code::F3 => 0x72,
        Code::F4 => 0x73,
        Code::F5 => 0x74,
        Code::F6 => 0x75,
        Code::F7 => 0x76,
        Code::F8 => 0x77,
        Code::F9 => 0x78,
        Code::F10 => 0x79,
        Code::F11 => 0x7A,
        Code::F12 => 0x7B,
        Code::F13 => 0x7C,
        Code::F14 => 0x7D,
        Code::F15 => 0x7E,
        Code::F16 => 0x7F,
        Code::F17 => 0x80,
        Code::F18 => 0x81,
        Code::F19 => 0x82,
        Code::F20 => 0x83,
        Code::F21 => 0x84,
        Code::F22 => 0x85,
        Code::F23 => 0x86,
        Code::F24 => 0x87,
        Code::NumLock => 0x90,
        Code::Numpad0 => 0x60,
        Code::Numpad1 => 0x61,
        Code::Numpad2 => 0x62,
        Code::Numpad3 => 0x63,
        Code::Numpad4 => 0x64,
        Code::Numpad5 => 0x65,
        Code::Numpad6 => 0x66,
        Code::Numpad7 => 0x67,
        Code::Numpad8 => 0x68,
        Code::Numpad9 => 0x69,
        Code::NumpadAdd => 0x6B,
        Code::NumpadDecimal => 0x6E,
        Code::NumpadDivide => 0x6F,
        Code::NumpadEqual => 0x45,
        Code::NumpadMultiply => 0x6A,
        Code::NumpadSubtract => 0x6D,
        Code::ScrollLock => 0x91,
        Code::AudioVolumeDown => 0xAE,
        Code::AudioVolumeUp => 0xAF,
        Code::AudioVolumeMute => 0xAD,
        Code::MediaPlay => 0xFA,
        Code::MediaPause | Code::Pause => 0x13,
        Code::MediaPlayPause => 0xB3,
        Code::MediaStop => 0xB2,
        Code::MediaTrackNext => 0xB0,
        Code::MediaTrackPrevious => 0xB1,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ActivationMode::{PushToTalk, Toggle};

    #[test]
    fn ptt_hold_and_release_is_one_start_and_one_stop() {
        let mut m = Machine::new();
        assert_eq!(m.press(PushToTalk), Action::Start);
        assert_eq!(m.started(), Action::Ignore);
        assert_eq!(m.phase(), Phase::Recording);
        assert_eq!(m.release(), Action::Stop);
        m.stopped();
        assert_eq!(m.phase(), Phase::Idle);
    }

    /// The bug this machine exists for: a quick tap releases while the mic is
    /// still opening and the session reconnecting. Exactly one stop must run —
    /// the old code could fire two (release handler + start's catch-up check)
    /// or none.
    #[test]
    fn ptt_tap_during_startup_stops_exactly_once() {
        let mut m = Machine::new();
        assert_eq!(m.press(PushToTalk), Action::Start);
        assert_eq!(m.release(), Action::Ignore, "nothing to stop yet");
        assert_eq!(m.phase(), Phase::Starting);
        assert_eq!(m.started(), Action::Stop, "the release must be honored");
        assert_eq!(m.phase(), Phase::Stopping);
    }

    /// Re-pressing while the previous start is still in flight must not open a
    /// second capture + second PCM pump.
    #[test]
    fn ptt_second_press_during_startup_is_ignored() {
        let mut m = Machine::new();
        assert_eq!(m.press(PushToTalk), Action::Start);
        assert_eq!(m.press(PushToTalk), Action::Ignore);
        assert_eq!(m.press(PushToTalk), Action::Ignore);
        assert_eq!(m.started(), Action::Ignore);
        assert_eq!(m.phase(), Phase::Recording);
    }

    #[test]
    fn ptt_press_while_recording_is_ignored() {
        let mut m = Machine::new();
        m.press(PushToTalk);
        m.started();
        assert_eq!(m.press(PushToTalk), Action::Ignore);
        assert_eq!(m.phase(), Phase::Recording);
    }

    /// While a commit is in flight (transcript wait + injection) further hotkey
    /// traffic is inert until `stopped()`.
    #[test]
    fn presses_while_stopping_are_ignored_until_idle() {
        let mut m = Machine::new();
        m.press(PushToTalk);
        m.started();
        assert_eq!(m.release(), Action::Stop);
        assert_eq!(m.press(PushToTalk), Action::Ignore);
        assert_eq!(m.release(), Action::Ignore);
        m.stopped();
        assert_eq!(m.press(PushToTalk), Action::Start);
    }

    #[test]
    fn toggle_press_starts_then_stops() {
        let mut m = Machine::new();
        assert_eq!(m.press(Toggle), Action::Start);
        m.started();
        assert_eq!(m.press(Toggle), Action::Stop);
        m.stopped();
        assert_eq!(m.press(Toggle), Action::Start);
    }

    /// No 400 ms debounce any more: a fast double tap in toggle mode has to
    /// stop the recording, not be swallowed.
    #[test]
    fn toggle_fast_double_tap_stops_immediately() {
        let mut m = Machine::new();
        assert_eq!(m.press(Toggle), Action::Start);
        assert_eq!(m.press(Toggle), Action::Ignore, "still starting");
        assert_eq!(m.started(), Action::Stop);
    }

    /// Key-up in toggle mode is not a stop.
    #[test]
    fn toggle_release_never_stops() {
        let mut m = Machine::new();
        m.press(Toggle);
        m.started();
        assert_eq!(m.release(), Action::Ignore);
        assert_eq!(m.phase(), Phase::Recording);
    }

    /// Changing the activation mode mid-press must not make the key-up of a
    /// toggle press behave like a push-to-talk release.
    #[test]
    fn release_uses_the_mode_captured_at_press() {
        let mut m = Machine::new();
        m.press(Toggle);
        m.started();
        // config flipped to push-to-talk here; the release still belongs to the
        // toggle press that started this recording.
        assert_eq!(m.release(), Action::Ignore);
    }

    #[test]
    fn release_without_press_is_inert() {
        let mut m = Machine::new();
        assert_eq!(m.release(), Action::Ignore);
        assert_eq!(m.phase(), Phase::Idle);
    }

    #[test]
    fn silence_autostop_stops_once() {
        let mut m = Machine::new();
        m.press(PushToTalk);
        m.started();
        assert_eq!(m.stop_requested(), Action::Stop);
        // The hotkey release lands right after: it must not stop a second time.
        assert_eq!(m.release(), Action::Ignore);
    }

    #[test]
    fn watchdog_during_startup_defers_to_started() {
        let mut m = Machine::new();
        m.press(PushToTalk);
        assert_eq!(m.stop_requested(), Action::Ignore);
        assert_eq!(m.started(), Action::Stop);
    }

    #[test]
    fn failed_start_returns_to_idle() {
        let mut m = Machine::new();
        m.press(PushToTalk);
        m.release(); // queued stop
        m.start_failed();
        assert_eq!(m.phase(), Phase::Idle);
        // The queued stop must not leak into the next recording.
        assert_eq!(m.press(PushToTalk), Action::Start);
        assert_eq!(m.started(), Action::Ignore);
        assert_eq!(m.phase(), Phase::Recording);
    }

    /// The phantom key-up this whole release-watcher exists for: a `Released`
    /// arriving before the recording is live queues a stop, and `started()`
    /// commits an empty dictation. The machine is right to do that — a real tap
    /// must commit — which is why the bogus release has to be rejected at the
    /// hotkey handler, before it ever reaches here.
    #[test]
    fn a_release_during_startup_always_commits() {
        let mut m = Machine::new();
        m.press(PushToTalk);
        assert_eq!(m.release(), Action::Ignore);
        assert_eq!(m.started(), Action::Stop, "an empty commit, by design");
    }

    /// The VK numbers must match what `RegisterHotKey` was given, or
    /// `GetAsyncKeyState` would be polling some unrelated key and the watcher
    /// would either never fire or fire immediately.
    #[test]
    fn hotkey_keys_map_to_their_virtual_key_codes() {
        assert_eq!(code_to_vk(&Code::KeyE), Some(0x45)); // the default hotkey
        assert_eq!(code_to_vk(&Code::KeyA), Some(0x41));
        assert_eq!(code_to_vk(&Code::KeyZ), Some(0x5A));
        assert_eq!(code_to_vk(&Code::Digit0), Some(0x30));
        assert_eq!(code_to_vk(&Code::CapsLock), Some(0x14));
        assert_eq!(code_to_vk(&Code::Space), Some(0x20));
        assert_eq!(code_to_vk(&Code::F9), Some(0x78));
        assert_eq!(code_to_vk(&Code::F24), Some(0x87));
        // Unpollable: the physical check stays off rather than guessing.
        assert_eq!(code_to_vk(&Code::Fn), None);
    }

    /// Every key the settings panel can produce a shortcut for must be
    /// pollable, or push-to-talk silently falls back to the buggy library
    /// release for that key.
    #[test]
    fn every_letter_digit_and_function_key_is_pollable() {
        for c in [
            Code::KeyB, Code::KeyC, Code::KeyD, Code::KeyF, Code::KeyG, Code::KeyH,
            Code::KeyI, Code::KeyJ, Code::KeyK, Code::KeyL, Code::KeyM, Code::KeyN,
            Code::KeyO, Code::KeyP, Code::KeyQ, Code::KeyR, Code::KeyS, Code::KeyT,
            Code::KeyU, Code::KeyV, Code::KeyW, Code::KeyX, Code::KeyY,
            Code::Digit1, Code::Digit5, Code::Digit9,
            Code::F1, Code::F5, Code::F12,
            Code::Enter, Code::Tab, Code::Escape, Code::Backspace,
            Code::ArrowUp, Code::ArrowDown, Code::Home, Code::End,
            Code::Numpad0, Code::Numpad9, Code::ScrollLock, Code::NumLock,
        ] {
            assert!(code_to_vk(&c).is_some(), "{c:?} is not pollable");
        }
    }

    #[test]
    fn suppression_window_expires() {
        let now = Instant::now();
        assert!(!is_suppressed(None, now));
        assert!(is_suppressed(Some(now + Duration::from_millis(50)), now));
        assert!(!is_suppressed(Some(now - Duration::from_millis(1)), now));
    }

    #[test]
    fn capslock_hotkey_detected_by_name() {
        assert!(hotkey_is_capslock("CapsLock"));
        assert!(hotkey_is_capslock(" capslock "));
        assert!(hotkey_is_capslock("CAPSLOCK"));
        assert!(!hotkey_is_capslock("Ctrl+CapsLock"));
        assert!(!hotkey_is_capslock("F9"));
        assert!(!hotkey_is_capslock(""));
        // The default hotkey: the caps machinery must stay completely inert.
        assert!(!hotkey_is_capslock("Ctrl+E"));
    }

    /// Compare-based restore: correct however many times the key toggled caps
    /// (twice in toggle mode, once in push-to-talk, zero if Windows swallowed
    /// it). The old code injected exactly one click unconditionally, which
    /// inverted caps on every toggle-mode dictation.
    #[test]
    fn caps_restore_only_when_state_actually_changed() {
        assert!(caps_restore_needed(Some(false), Some(true)));
        assert!(caps_restore_needed(Some(true), Some(false)));
        assert!(!caps_restore_needed(Some(true), Some(true)));
        assert!(!caps_restore_needed(Some(false), Some(false)));
        assert!(!caps_restore_needed(None, Some(true)));
        assert!(!caps_restore_needed(Some(true), None));
    }
}
