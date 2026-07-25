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

    #[cfg(test)]
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
