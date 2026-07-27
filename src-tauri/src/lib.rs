mod audio;
mod auth;
mod commands;
mod config;
mod dictate;
mod error;
mod g711;
mod keystate;
mod realtime;
mod winkeys;

use commands::AppState;
use config::ActivationMode;
use dictate::Committed;
use keystate::Action;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager, WebviewWindow};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tracing::{error, info, warn};

const PCM_PUMP_INTERVAL: Duration = Duration::from_millis(50);
/// Digital silence wrapped around the captured audio.
///
/// A recording that begins and ends the instant speech does gives the
/// transcriber no run-up: its own windowing eats the first and last syllable.
/// Two seconds on each side is generous — it costs no wall-clock time, since
/// the padding is bytes appended to a buffer rather than time spent waiting.
const SILENCE_LEAD: Duration = Duration::from_secs(2);
const SILENCE_TAIL: Duration = Duration::from_secs(2);
/// How long the session model gets to write down the utterance. Measured at
/// 0.5-1.0 s on real speech; past this the side-channel transcript is used.
const MODEL_TRANSCRIBE_BUDGET: Duration = Duration::from_secs(6);
/// Pill geometry. The window is sized to the pill exactly — a transparent
/// window is still hit-testable, so any slack around the pill would swallow
/// clicks meant for the app underneath.
const PILL_HEIGHT: f64 = 34.0;
const PILL_WIDTH: f64 = 300.0;
/// The settings panel needs room the pill does not, so opening it grows the
/// window in both directions.
const SETTINGS_HEIGHT: f64 = 620.0;
const SETTINGS_WIDTH: f64 = 420.0;

fn configured_hotkeys() -> Option<(Shortcut, Shortcut)> {
    let cfg = config::get();
    let dictation = cfg.hotkey.trim().parse::<Shortcut>().ok()?;
    let settings = cfg.settings_hotkey.trim().parse::<Shortcut>().ok()?;
    if dictation == settings {
        return None;
    }
    Some((dictation, settings))
}

/// (Re)register both hotkeys from the current config. Called at startup and
/// after every settings save, so hotkey changes apply without a restart.
pub fn apply_hotkeys(app: &tauri::AppHandle) -> Result<(), String> {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let (dictation, settings) = configured_hotkeys()
        .ok_or_else(|| "hotkeys are invalid or identical".to_string())?;
    gs.register(dictation).map_err(|e| format!("register dictation hotkey: {e}"))?;
    gs.register(settings).map_err(|e| format!("register settings hotkey: {e}"))?;
    // Hotkey changes happen from the settings panel, i.e. between dictations —
    // the only moment the caps baseline can be sampled honestly.
    init_caps_baseline(app);
    Ok(())
}

static SETTINGS_OPEN: AtomicBool = AtomicBool::new(false);

pub fn settings_is_open() -> bool {
    SETTINGS_OPEN.load(Ordering::SeqCst)
}

pub fn toggle_settings(app: &tauri::AppHandle) {
    set_settings_open(app, !settings_is_open());
}

/// Expand the pill into the settings panel or collapse it back. The window is
/// always clickable now (drag handle, buttons); what changes is focus: the
/// panel takes keyboard focus while open and gives it up when closed, so the
/// pill never steals the dictation target's focus in pill mode.
pub fn set_settings_open(app: &tauri::AppHandle, open: bool) {
    let Some(window) = app.get_webview_window("main") else { return };
    SETTINGS_OPEN.store(open, Ordering::SeqCst);
    if open {
        let _ = window.set_focusable(true);
        let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize {
            width: SETTINGS_WIDTH,
            height: SETTINGS_HEIGHT,
        }));
        let _ = window.set_focus();
        let _ = app.emit("settings://open", serde_json::json!({}));
    } else {
        let _ = app.emit("settings://close", serde_json::json!({}));
        let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize {
            width: PILL_WIDTH,
            height: PILL_HEIGHT,
        }));
        let _ = window.set_focusable(false);
    }
}

/// The pill is a transparent always-on-top overlay. It is clickable (drag,
/// buttons) but refuses keyboard focus in pill mode (WS_EX_NOACTIVATE), so
/// interacting with it never steals the dictation target's focus.
fn configure_overlay(window: &WebviewWindow) {
    let _ = window.set_always_on_top(true);
    if !SETTINGS_OPEN.load(Ordering::SeqCst) {
        let _ = window.set_focusable(false);
    }
}

async fn ensure_connected(app: &tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    // Serialize: the setup-time auto-connect and the first hotkey press race
    // otherwise, and each would open its own realtime call.
    let _guard = state.connect_lock.lock().await;
    if state.usable_session().await.is_some() {
        return Ok(());
    }
    // A stale session (dropped connection, expiring call) is torn down first so
    // only one call is ever open.
    if let Some(stale) = state.session.lock().await.take() {
        stale.disconnect().await;
    }
    match crate::realtime::RealtimeSession::connect(make_emitter(app)).await {
        Ok((session, info)) => {
            let session = Arc::new(session);
            spawn_realtime_failure_watchdog(app.clone(), &session);
            *state.session.lock().await = Some(session);
            *state.info.lock().await = Some(info);
            info!("realtime session connected");
            Ok(())
        }
        Err(e) => {
            warn!(error = %e, "connect failed");
            Err(e.to_string())
        }
    }
}

fn make_emitter(app: &tauri::AppHandle) -> crate::realtime::Emitter {
    let app = app.clone();
    std::sync::Arc::new(move |event: &str, payload: serde_json::Value| {
        let _ = app.emit(event, payload);
    })
}

/// The caps state the user is entitled to have, sampled while *not* dictating.
///
/// It has to be sampled outside a dictation: by the time our hotkey handler
/// runs, the key-down that triggered it may already have flipped the toggle
/// bit, so a snapshot taken at the start of a recording can read the flipped
/// value and the comparison at commit time would see "nothing changed".
///
/// Comparing against this baseline is right whatever Windows does with a
/// swallowed CapsLock: one flip per press (push-to-talk) is corrected, two
/// flips (toggle mode's start and stop presses) cancel out and are left alone,
/// and zero flips need nothing. The old code sent exactly one compensating
/// click unconditionally, which inverted caps on every toggle-mode dictation
/// and on every aborted one.
///
/// `None` means "not our business" — the dictation hotkey is not CapsLock.
static CAPS_BASELINE: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

fn caps_baseline() -> &'static std::sync::Mutex<Option<bool>> {
    &CAPS_BASELINE
}

/// Run a closure on the UI thread.
///
/// `GetKeyState` reports the keyboard state as of the last message the calling
/// thread pulled from its queue, so a tokio worker — which pumps no messages —
/// can read a stale caps toggle bit. The main thread runs the event loop, so
/// both the read and the corrective keystroke belong there.
fn on_main_thread(app: &tauri::AppHandle, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = app.run_on_main_thread(f) {
        warn!(error = %e, "could not reach the main thread for caps handling");
    }
}

/// (Re)sample the baseline. Safe only between dictations — called at startup
/// and whenever the hotkey changes.
fn init_caps_baseline(app: &tauri::AppHandle) {
    let is_capslock = keystate::hotkey_is_capslock(&config::get().hotkey);
    on_main_thread(app, move || {
        let sampled = if is_capslock { winkeys::caps_on() } else { None };
        info!(?sampled, "caps baseline sampled");
        *caps_baseline()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = sampled;
    });
}

/// Put CapsLock back the way the user had it.
///
/// Called at the very top of the commit — microseconds after the physical key
/// came up, long before the transcript wait and the injection. Doing it last
/// (as this used to) opened a window hundreds of milliseconds wide in which the
/// user's next press was eaten by the guard that hides our own injected tap.
fn restore_caps_state(app: &tauri::AppHandle) {
    on_main_thread(app, || {
        // The lock is held across the tap so two commits cannot both decide to
        // correct the same drift.
        let baseline = caps_baseline()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = winkeys::caps_on();
        if !keystate::caps_restore_needed(*baseline, now) {
            return;
        }
        info!(?baseline, ?now, "restoring caps lock state");
        // RegisterHotKey sees injected input, so the tap comes straight back as
        // a press+release pair on our own hotkey. Ignore hotkey traffic
        // briefly; the window is short enough that a real key press cannot fit
        // inside it.
        keystate::suppress_for(keystate::SYNTHETIC_SUPPRESS);
        winkeys::tap_capslock();
        // The baseline stands: the tap is what makes reality match it again. If
        // it somehow does not land, the next commit simply tries again.
    });
}

/// How often we ask Windows whether the push-to-talk key is still held.
const KEY_POLL: Duration = Duration::from_millis(15);
/// If the key is never *observed* down within this window, key polling is not
/// telling us anything useful on this machine. The watcher stands down and the
/// hotkey library's own `Released` event drives the stop, as before.
const KEY_DOWN_GRACE: Duration = Duration::from_millis(1500);

/// Generation counter for the push-to-talk release watcher.
///
/// One watcher exists per recording. Bumping this retires whichever watcher is
/// running, so a thread left over from the previous dictation can never commit
/// the next one.
static WATCH_GEN: AtomicU64 = AtomicU64::new(0);

fn cancel_release_watcher() {
    WATCH_GEN.fetch_add(1, Ordering::SeqCst);
}

/// Drive push-to-talk from the physical key state instead of trusting the
/// hotkey library's release event.
///
/// `global-hotkey` detects release by spawning a thread inside the `WM_HOTKEY`
/// window procedure that calls `GetAsyncKeyState` **immediately, with no
/// initial delay**, and declares the key released if that single first read
/// comes back zero. When it does — and it does — the release lands while the
/// user is still holding the key, `stop_pending` is set before the recording
/// is even live, and `started()` commits an empty dictation on the spot. That
/// is the "start_dictation: hotkey already released; committing" /
/// "committed: EMPTY peak_rms=0" pair in the logs: not a transcription
/// problem, a phantom key-up.
///
/// This watcher only reports a release it has actually watched happen: the key
/// must be seen down first, then seen up. Anything it cannot confirm it leaves
/// alone.
fn spawn_release_watcher(app: tauri::AppHandle, vk: u16) {
    let generation = WATCH_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        let deadline = Instant::now() + KEY_DOWN_GRACE;
        let mut seen_down = false;
        loop {
            // A newer press (or a commit) retired us.
            if WATCH_GEN.load(Ordering::SeqCst) != generation {
                return;
            }
            match winkeys::key_is_down(vk) {
                Some(true) => seen_down = true,
                Some(false) if seen_down => break,
                Some(false) => {
                    if Instant::now() >= deadline {
                        warn!(vk, "release watcher: key never read as down; standing down");
                        return;
                    }
                }
                // Not Windows: no key state to poll.
                None => return,
            }
            std::thread::sleep(KEY_POLL);
        }
        info!("hotkey physically released");
        if keystate::with(|m| m.stop_requested()) == Action::Stop {
            tauri::async_runtime::spawn(async move { stop_dictation(&app).await });
        }
        // Otherwise the recording is still coming up; the stop is queued and
        // `started()` will honor it.
    });
}

async fn start_dictation(app: &tauri::AppHandle, window: &WebviewWindow) {
    info!("start_dictation: opening microphone");
    let state = app.state::<AppState>();

    // Open the microphone FIRST: cpal buffers into a channel while a (possibly
    // stale) session reconnects, so the user's first words survive the
    // reconnect instead of being clipped. Sessions idle for a few minutes go
    // stale server-side (events silently stop), so ensure_connected recreates
    // them — that takes ~2s and this ordering is what hides it.
    let cfg = config::get();
    let capture = match crate::audio::AudioCapture::start(cfg.mic_device.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "audio capture failed");
            return fail_start(app, e);
        }
    };
    *state.capture.lock().await = Some(capture);

    // Feedback must be instant: the user starts talking the moment the key
    // goes down, and the mic is already buffering — show it NOW, not after
    // the (possibly multi-second) session reconnect.
    let _ = app.emit("dictate://started", serde_json::json!({}));
    configure_overlay(window);

    info!("start_dictation: ensuring session");
    if let Err(e) = ensure_connected(app).await {
        release_capture(app).await;
        return fail_start(app, e);
    }
    info!("start_dictation: session ready");
    let session = match state.session().await {
        Some(s) => s,
        None => {
            release_capture(app).await;
            return fail_start(app, "not connected".to_string());
        }
    };

    if let Err(e) = state.dictate.start_listening(session.subscribe()).await {
        release_capture(app).await;
        return fail_start(app, e);
    }

    // Lead-in silence, before any captured audio reaches the buffer.
    if let Err(e) = session.append_silence(SILENCE_LEAD).await {
        warn!(error = %e, "lead-in silence failed");
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    PEAK_RMS.store(0, Ordering::SeqCst);
    let join = spawn_pcm_pump(app.clone(), session, stop_flag.clone());
    *PUMP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Pump { stop: stop_flag, join });

    // The user may have let go while we were still preparing (capture +
    // reconnect can take seconds). The machine remembered that release; honor
    // it now instead of leaving a zombie recording running.
    if keystate::with(|m| m.started()) == Action::Stop {
        info!("start_dictation: a stop was queued during startup; committing");
        stop_dictation(app).await;
    }
}

/// Roll the lifecycle back to idle and show why the recording never started.
///
/// The press that got us here may already have flipped caps, so that is undone
/// too — a failed start used to leave CapsLock stuck on.
fn fail_start(app: &tauri::AppHandle, message: String) {
    cancel_release_watcher();
    keystate::with(|m| m.start_failed());
    restore_caps_state(app);
    let _ = app.emit("dictate://error", serde_json::json!({ "message": message }));
}

async fn release_capture(app: &tauri::AppHandle) {
    if let Some(mut c) = app.state::<AppState>().capture.lock().await.take() {
        c.stop();
    }
}

/// The currently running PCM pump. Without this, every recording left a pump
/// task alive for the process lifetime, and after N dictations N pumps would
/// emit the same audio N times over.
struct Pump {
    stop: Arc<AtomicBool>,
    join: tokio::task::JoinHandle<()>,
}

static PUMP: std::sync::Mutex<Option<Pump>> = std::sync::Mutex::new(None);

fn take_pump() -> Option<Pump> {
    PUMP.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

/// Stop the pump and wait for it to actually be gone, so nothing else is
/// writing to the audio track while the tail is flushed.
async fn halt_pump() {
    if let Some(pump) = take_pump() {
        pump.stop.store(true, Ordering::SeqCst);
        let _ = pump.join.await;
    }
}

/// Peak mic RMS of the current recording, updated by the pump. Read on stop to
/// distinguish "you said nothing" from "your microphone delivered silence" —
/// the classic wrong-default-device failure, which must surface as a warning,
/// not an empty commit.
static PEAK_RMS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
/// Below this peak RMS the mic delivered effectively nothing (room noise).
const NO_SIGNAL_RMS: i32 = 300;

/// Stream captured PCM straight from Rust into the realtime call.
///
/// The audio never touches the webview: cpal captures it, the pump feeds it to
/// the Opus encoder, and encoded frames go out on the WebRTC audio track.
/// When silence auto-stop is enabled, sustained quiet after the first loud
/// chunk commits the dictation on its own — the VoiceInk behavior.
///
/// Debug: CODEX_MIC_DEBUG_PCM_FILE points the pump at a 48kHz mono PCM16LE
/// file instead of the microphone, looping it — splits "capture content" from
/// "session lifecycle" bugs in the live app path.
fn spawn_pcm_pump(
    app: tauri::AppHandle,
    session: Arc<crate::realtime::RealtimeSession>,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let cfg = config::get();
        let autostop = if cfg.silence_autostop_ms > 0 {
            Some(Duration::from_millis(cfg.silence_autostop_ms))
        } else {
            None
        };
        let debug_pcm = std::env::var("CODEX_MIC_DEBUG_PCM_FILE")
            .ok()
            .and_then(|p| std::fs::read(p).ok());
        if debug_pcm.is_some() {
            info!("DEBUG: pump sourced from PCM file, looping");
        }
        let mut debug_offset = 0usize;
        let mut last_loud: Option<Instant> = None;
        let mut last_rms_log = Instant::now() - Duration::from_secs(10);
        let mut interval = tokio::time::interval(PCM_PUMP_INTERVAL);
        interval.tick().await;
        // Set when the stream died under us: there is no usable transcript, so
        // the recording is discarded rather than committed.
        let mut fatal = false;
        while !stop.load(Ordering::SeqCst) {
            interval.tick().await;
            let chunk = if let Some(pcm) = &debug_pcm {
                let n = (crate::audio::TARGET_SAMPLE_RATE as usize) / 20 * 2; // 50ms
                let end = (debug_offset + n).min(pcm.len());
                let slice = pcm[debug_offset..end].to_vec();
                debug_offset = if end >= pcm.len() { 0 } else { end };
                Some(slice)
            } else {
                let state = app.state::<AppState>();
                let guard = state.capture.lock().await;
                let Some(capture) = guard.as_ref() else { break };
                capture.read_pending_bytes()
            };
            let Some(bytes) = chunk else { continue };
            // Mic level for the pill meter — the difference between "the mic
            // hears you" and "you're dictating into a void" should be visible.
            let rms = config::pcm_rms(&bytes);
            let _ = app.emit("dictate://level", serde_json::json!({ "rms": rms }));
            PEAK_RMS.fetch_max(rms as i32, Ordering::SeqCst);
            if last_rms_log.elapsed() >= Duration::from_secs(1) {
                last_rms_log = Instant::now();
                info!(rms, "pump audio level");
            }
            if let Some(limit) = autostop {
                if rms >= cfg.silence_threshold {
                    last_loud = Some(Instant::now());
                } else if let Some(t) = last_loud {
                    if t.elapsed() >= limit {
                        info!("silence auto-stop fired");
                        break;
                    }
                }
            }
            if let Err(e) = session.append_pcm(&bytes).await {
                warn!(error = %e, "append_pcm failed; stopping stream");
                let _ = app.emit("dictate://error", serde_json::json!({ "message": e.to_string() }));
                fatal = true;
                break;
            }
        }
        // Asked to stop by the commit path: it owns the teardown from here.
        if stop.load(Ordering::SeqCst) {
            return;
        }
        // We left on our own — silence auto-stop, a capture that vanished, or a
        // dead stream. Retire our own handle first, or the teardown would await
        // this very task from inside it, and drop the microphone either way: a
        // failed append used to return here and leave it live indefinitely.
        if let Some(pump) = take_pump() {
            pump.stop.store(true, Ordering::SeqCst);
        }
        if keystate::with(|m| m.stop_requested()) == Action::Stop {
            if fatal {
                abort_dictation(&app).await;
            } else {
                stop_dictation(&app).await;
            }
        }
    })
}

/// If the realtime session fails server-side (quota exhausted, auth expired,
/// transport dropped), the recording can never produce a transcript. Release
/// the microphone immediately instead of leaving it live until the user
/// happens to notice and press the hotkey again.
fn spawn_realtime_failure_watchdog(app: tauri::AppHandle, session: &Arc<crate::realtime::RealtimeSession>) {
    use tokio::sync::broadcast::error::RecvError;
    let mut rx = session.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(n) if n.method == "thread/realtime/error" => {
                    let (phase, action) =
                        keystate::with(|m| (m.phase(), m.stop_requested()));
                    if action == Action::Stop {
                        warn!("realtime session failed; releasing microphone");
                        abort_dictation(&app).await;
                    } else if phase == keystate::Phase::Starting {
                        // The stop is queued and `started()` will act on it.
                        // Logged because this path used to be silent and looked
                        // exactly like a phantom key-up in the logs.
                        warn!("realtime session failed during startup; stop queued");
                    }
                }
                Ok(_) => {}
                Err(RecvError::Closed) => break,
                Err(RecvError::Lagged(_)) => {}
            }
        }
    });
}

/// Stop recording and discard the buffer without typing or emitting a
/// `stopped` event — the error already on screen must stay visible.
async fn abort_dictation(app: &tauri::AppHandle) {
    cancel_release_watcher();
    restore_caps_state(app);
    halt_pump().await;
    release_capture(app).await;
    app.state::<AppState>().dictate.abort().await;
    keystate::with(|m| m.stopped());
}

async fn stop_dictation(app: &tauri::AppHandle) {
    cancel_release_watcher();
    let state = app.state::<AppState>();
    let _ = app.emit("dictate://processing", serde_json::json!({}));
    // First thing, while the key has only just come up: anything later races
    // the user's next press.
    restore_caps_state(app);

    halt_pump().await;

    // Hand over the audio the pump never got to: everything cpal buffered
    // since its last 50 ms tick, plus the partial Opus frame the encoder is
    // holding, plus a beat of silence to close the turn. Without this the last
    // syllable of every dictation was captured and then thrown away.
    let session = state.session().await;
    if let Some(mut capture) = state.capture.lock().await.take() {
        if let Some(s) = &session {
            if let Some(bytes) = capture.read_pending_bytes() {
                let _ = s.append_pcm(&bytes).await;
            }
        }
        // Release the microphone: the user pressed stop, the mic indicator
        // should go out now regardless of what the server does next.
        capture.stop();
        if let Some(s) = &session {
            if let Some(bytes) = capture.read_pending_bytes() {
                let _ = s.append_pcm(&bytes).await;
            }
        }
    }
    if let Some(s) = &session {
        if let Err(e) = s.flush_tail(SILENCE_TAIL).await {
            warn!(error = %e, "tail flush failed");
        }
        // With VAD off our commit is the only thing that opens a turn, and
        // exactly one transcript always follows it. Under VAD the server's own
        // `speech_started` already reports this, and claiming a turn that has
        // already been transcribed would just stall the commit.
        if crate::realtime::manual_turns() {
            state.dictate.expect_turn().await;
        }
    }

    // The session model's own reading of the audio. It follows instructions,
    // which the transcription side channel's `prompt` does not, so it is the
    // only thing that reliably keeps a developer's English terms in English.
    // The side channel still runs underneath as the fallback.
    let spoken = match (&session, crate::realtime::model_transcription()) {
        (Some(s), true) => {
            match crate::realtime::transcribe_committed_audio(s, MODEL_TRANSCRIBE_BUDGET).await {
                Ok(t) if !t.trim().is_empty() => Some(t),
                Ok(_) => None,
                Err(e) => {
                    warn!(error = %e, "model transcription failed; using the side channel");
                    None
                }
            }
        }
        _ => None,
    };

    match state.dictate.stop_listening_with(spoken).await {
        Ok(Committed::Typed(text)) => {
            info!(len = text.len(), "dictation committed: typed");
            let _ = app.emit("dictate://stopped", serde_json::json!({ "text": text }));
        }
        Ok(Committed::Empty) => {
            let peak = PEAK_RMS.load(Ordering::SeqCst);
            info!(peak_rms = peak, "dictation committed: EMPTY (no transcript arrived)");
            if peak < NO_SIGNAL_RMS {
                // The mic delivered room noise at best. Almost always a wrong
                // default device or a blocked/muted mic — say so explicitly
                // instead of pretending the user was just quiet.
                let _ = app.emit("dictate://error", serde_json::json!({
                    "message": "마이크 신호가 없습니다 — 설정(Ctrl+Shift+E)에서 마이크 장치를 확인하세요"
                }));
            } else {
                let _ = app.emit("dictate://stopped", serde_json::json!({ "text": "" }));
            }
        }
        Ok(Committed::Filtered(text)) => {
            info!(len = text.len(), "dictation committed: filtered");
            let _ = app.emit(
                "dictate://filtered",
                serde_json::json!({ "text": text }),
            );
        }
        Err(e) => {
            warn!(error = %e, "stop failed");
            let _ = app.emit("dictate://error", serde_json::json!({ "message": e }));
        }
    }
    keystate::with(|m| m.stopped());
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "codex_mic=info".into()),
        )
        .init();

    let shortcut_plugin = tauri_plugin_global_shortcut::Builder::new()
        .with_handler(move |app, shortcut, event| {
            let cfg = config::get();
            let Ok(dictation) = cfg.hotkey.trim().parse::<Shortcut>() else { return };
            let Ok(settings) = cfg.settings_hotkey.trim().parse::<Shortcut>() else { return };

            if shortcut == &settings {
                if event.state == ShortcutState::Pressed {
                    let app = app.clone();
                    let open = !SETTINGS_OPEN.load(Ordering::SeqCst);
                    tauri::async_runtime::spawn(async move {
                        set_settings_open(&app, open);
                    });
                }
                return;
            }
            if shortcut != &dictation {
                return;
            }
            // Our own caps-restore tap comes back through RegisterHotKey.
            if keystate::suppressed() {
                return;
            }

            // The virtual key `RegisterHotKey` is watching, so we can ask
            // Windows about it ourselves.
            let vk = keystate::code_to_vk(&dictation.key);

            // Every hotkey event, with the physical key state at that instant.
            // The race this guards against is invisible without the pair: the
            // library's "released" against what the keyboard actually says.
            let physical = vk.and_then(winkeys::key_is_down);

            // One decision, made synchronously. Doing this inside the spawned
            // task instead let a press and its release be reordered, and let
            // two presses both decide to start.
            let action = match event.state {
                ShortcutState::Pressed => {
                    let action = keystate::with(|m| m.press(cfg.activation_mode));
                    info!(?physical, ?action, "hotkey PRESSED");
                    action
                }
                _ => {
                    // `global-hotkey` can report a release while the key is
                    // demonstrably still down (see `spawn_release_watcher`).
                    // Refuse only a release we can positively disprove: if key
                    // state is unreadable we honor the event as before, so this
                    // check can drop a phantom stop but never swallow a real
                    // one.
                    if physical == Some(true) {
                        warn!("hotkey RELEASED (bogus) — the key is still physically down; ignored");
                        return;
                    }
                    let action = keystate::with(|m| m.release());
                    info!(?physical, ?action, "hotkey RELEASED");
                    action
                }
            };
            if action == Action::Ignore {
                return;
            }

            // Push-to-talk stops on the physical key-up, watched by us. Armed
            // here rather than inside `start_dictation` so the key is still
            // down when the watcher takes its first reading — a fast tap must
            // be seen as down-then-up, not as never-down.
            if action == Action::Start && cfg.activation_mode == ActivationMode::PushToTalk {
                if let Some(vk) = vk {
                    spawn_release_watcher(app.clone(), vk);
                }
            }

            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let Some(window) = app.get_webview_window("main") else {
                    warn!("main window not found");
                    // Never strand the machine mid-transition.
                    keystate::with(|m| m.stopped());
                    return;
                };
                match action {
                    Action::Start => start_dictation(&app, &window).await,
                    Action::Stop => stop_dictation(&app).await,
                    Action::Ignore => {}
                }
            });
        })
        .build();

    let builder = commands::builder().plugin(shortcut_plugin);

    let builder = builder.setup(move |app| {
        if let Some(window) = app.get_webview_window("main") {
            configure_overlay(&window);
            // Restore the dragged-to position from last session, and persist
            // new positions (throttled) as the user drags.
            let cfg = config::get();
            if let (Some(x), Some(y)) = (cfg.window_x, cfg.window_y) {
                let _ = window.set_position(tauri::Position::Physical(
                    tauri::PhysicalPosition { x, y },
                ));
            }
            window.on_window_event(|event| {
                if let tauri::WindowEvent::Moved(pos) = event {
                    static LAST_SAVE: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    if now.saturating_sub(LAST_SAVE.load(Ordering::SeqCst)) < 500 {
                        return;
                    }
                    LAST_SAVE.store(now, Ordering::SeqCst);
                    let (x, y) = (pos.x, pos.y);
                    let _ = config::update(|c| {
                        c.window_x = Some(x);
                        c.window_y = Some(y);
                    });
                }
            });
        }
        let app_handle = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = apply_hotkeys(&app_handle) {
                error!("{e}");
                let _ = app_handle.emit("dictate://error", serde_json::json!({ "message": e }));
            }
            if let Err(e) = ensure_connected(&app_handle).await {
                let _ = app_handle.emit("dictate://error", serde_json::json!({ "message": e }));
            }
        });
        Ok(())
    });

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod integration {
    use super::{MODEL_TRANSCRIBE_BUDGET, SILENCE_LEAD, SILENCE_TAIL};
    use crate::dictate::Committed;
    use crate::realtime::{Emitter, RealtimeSession};
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn enabled() -> bool {
        std::env::var("CODEX_MIC_INTEGRATION").is_ok()
    }

    /// End-to-end over OAuth WebRTC: open a call, stream real speech PCM
    /// (48 kHz mono PCM16LE from CODEX_MIC_TEST_PCM), and require a user-role
    /// transcript back. This exercises the exact production path — auth,
    /// call-create, ICE, Opus encode, RTP — against the live service.
    #[tokio::test]
    async fn realtime_oauth_transcribes_streamed_speech() {
        if !enabled() {
            eprintln!("skipping; set CODEX_MIC_INTEGRATION=1 to run");
            return;
        }
        let pcm_path = match std::env::var("CODEX_MIC_TEST_PCM") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("skipping; set CODEX_MIC_TEST_PCM to a 48kHz mono pcm16le file");
                return;
            }
        };
        let pcm = std::fs::read(&pcm_path).expect("read test pcm");
        assert!(pcm.len() > 48_000, "need at least ~0.5s of speech");

        let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(vec![]));
        let sink = events.clone();
        let emitter: Emitter = Arc::new(move |name: &str, payload: Value| {
            sink.lock().unwrap().push((name.to_string(), payload));
        });

        let (session, info) = RealtimeSession::connect(emitter).await.expect("connect");
        assert!(!info.call_id.is_empty(), "call id must be populated");
        assert_eq!(info.auth_mode, "chatgpt-oauth-ws");

        // Test files are 48 kHz; the session wants 24 kHz. 2:1 boxcar is fine
        // for a fixture.
        let pcm: Vec<u8> = pcm
            .chunks_exact(4)
            .flat_map(|c| {
                let a = i16::from_le_bytes([c[0], c[1]]) as i32;
                let b = i16::from_le_bytes([c[2], c[3]]) as i32;
                (((a + b) / 2) as i16).to_le_bytes()
            })
            .collect();

        // Stream at real-time pace: 20 ms of PCM (1920 bytes) per tick, while a
        // collector timestamps every transcript event — the timeline proves the
        // transcript streams WHILE audio is still being sent, not after.
        let stream_start = Instant::now();
        let timeline: Arc<Mutex<Vec<(u128, String)>>> = Arc::new(Mutex::new(vec![]));
        let tl = timeline.clone();
        let ev = events.clone();
        let collector = tokio::spawn(async move {
            loop {
                for (name, params) in ev.lock().unwrap().iter() {
                    let text = if name == "realtime://transcript-delta" {
                        params.get("delta").and_then(|d| d.as_str()).map(str::to_string)
                    } else if name == "realtime://transcript-done" {
                        params.get("text").and_then(|d| d.as_str()).map(|t| format!("[done] {t}"))
                    } else {
                        None
                    };
                    if let Some(t) = text {
                        let mut tl = tl.lock().unwrap();
                        if tl.last().map(|(_, last)| last) != Some(&t) {
                            tl.push((stream_start.elapsed().as_millis(), t));
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        let frame_bytes = (crate::realtime::SESSION_SAMPLE_RATE as usize / 50) * 2; // 20ms
        let mut ticker = tokio::time::interval(Duration::from_millis(20));
        for chunk in pcm.chunks(frame_bytes) {
            ticker.tick().await;
            session.append_pcm(chunk).await.expect("append_pcm");
        }
        let audio_ms = stream_start.elapsed().as_millis();
        eprintln!("[e2e] audio fully sent at {audio_ms}ms");

        // Wait for transcript events (VAD needs a beat after the last frame).
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut transcript = String::new();
        let mut turns: Vec<String> = vec![];
        let mut seen_turns = 0usize;
        let mut last_turn = Instant::now();
        while Instant::now() < deadline {
            for (name, params) in events.lock().unwrap().iter() {
                if name == "realtime://transcript-delta" {
                    if let Some(d) = params.get("delta").and_then(|d| d.as_str()) {
                        if !transcript.contains(d) {
                            transcript.push_str(d);
                        }
                    }
                }
                if name == "realtime://transcript-done" {
                    if let Some(t) = params.get("text").and_then(|t| t.as_str()) {
                        // Accumulate: one hold is many turns. Assigning here —
                        // which this used to do, mirroring the production bug —
                        // keeps only the last sentence.
                        if !turns.contains(&t.to_string()) {
                            turns.push(t.to_string());
                        }
                        transcript = turns.join(" ");
                    }
                }
            }
            // Do NOT stop at the first turn: VAD splits the utterance, and
            // leaving early is precisely how the last-turn-only bug hid here.
            // Settle instead — quiet for 2s once at least one turn has landed.
            if !turns.is_empty() && last_turn.elapsed() >= Duration::from_secs(2) {
                break;
            }
            if turns.len() != seen_turns {
                seen_turns = turns.len();
                last_turn = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        collector.abort();
        eprintln!("[e2e] transcript timeline (ms since stream start):");
        for (ms, text) in timeline.lock().unwrap().iter() {
            let marker = if *ms <= audio_ms { "DURING-STREAM" } else { "after-stream" };
            eprintln!("[e2e]   {ms:>6}ms [{marker}] {text:?}");
        }
        eprintln!("[e2e] final transcript: {transcript:?}");
        eprintln!("[e2e] server produced {} turn(s)", turns.len());
        assert!(
            !transcript.trim().is_empty(),
            "no user transcript arrived within 30s of streaming speech"
        );
        session.disconnect().await;
    }

    /// The commit path, end to end, over the live service.
    ///
    /// The test above watches raw server events; this one runs the real
    /// `DictateState` — the thing that decides what gets typed. Server VAD
    /// splits a multi-sentence dictation into several turns, and every turn has
    /// to survive into the committed text. When each `transcript/done`
    /// overwrote the last instead of appending, this is what came out:
    ///
    /// ```text
    /// spoken:    안녕하세요. 지금 마이크 테스트를 하고 있습니다. 오늘 점심은 …
    /// committed: 지금 마이크 테스트를 하고 있습니다.
    /// ```
    ///
    /// Every sentence but one silently dropped, which reads as the transcriber
    /// returning something else entirely. Set CODEX_MIC_TEST_PHRASES to a
    /// `|`-separated list of fragments the commit must contain.
    #[tokio::test]
    async fn dictate_commit_keeps_every_turn_of_one_hold() {
        if !enabled() {
            eprintln!("skipping; set CODEX_MIC_INTEGRATION=1 to run");
            return;
        }
        let Ok(pcm_path) = std::env::var("CODEX_MIC_TEST_PCM") else {
            eprintln!("skipping; set CODEX_MIC_TEST_PCM to a 48kHz mono pcm16le file");
            return;
        };
        let pcm = std::fs::read(&pcm_path).expect("read test pcm");

        let emitter: Emitter = Arc::new(|_: &str, _: Value| {});
        let (session, _info) = RealtimeSession::connect(emitter).await.expect("connect");

        // The real accumulator, fed by the real session.
        let dictate = crate::dictate::DictateState::new();
        dictate
            .start_listening(session.subscribe())
            .await
            .expect("start listening");

        session
            .append_silence(SILENCE_LEAD)
            .await
            .expect("lead-in silence");

        // 48 kHz fixture down to the session's 24 kHz, streamed at speaking pace.
        let pcm: Vec<u8> = pcm
            .chunks_exact(4)
            .flat_map(|c| {
                let a = i16::from_le_bytes([c[0], c[1]]) as i32;
                let b = i16::from_le_bytes([c[2], c[3]]) as i32;
                (((a + b) / 2) as i16).to_le_bytes()
            })
            .collect();
        let frame_bytes = (crate::realtime::SESSION_SAMPLE_RATE as usize / 50) * 2;
        let mut ticker = tokio::time::interval(Duration::from_millis(20));
        for chunk in pcm.chunks(frame_bytes) {
            ticker.tick().await;
            session.append_pcm(chunk).await.expect("append_pcm");
        }

        // Exactly what `stop_dictation` does at key-up. The clock starts here:
        // this is the delay the user actually feels, between letting go of the
        // hotkey and seeing text.
        let key_up = Instant::now();
        session.flush_tail(SILENCE_TAIL).await.expect("flush tail");
        if crate::realtime::manual_turns() {
            dictate.expect_turn().await;
        }
        // Mirror `stop_dictation` exactly: the session model's transcript when
        // there is one, the side channel otherwise. Injection is deliberately
        // not called — this must not type into the developer's screen.
        let spoken = if crate::realtime::model_transcription() {
            crate::realtime::transcribe_committed_audio(&session, MODEL_TRANSCRIBE_BUDGET)
                .await
                .ok()
                .filter(|t| !t.trim().is_empty())
        } else {
            None
        };
        let fallback = dictate
            .finish(crate::dictate::Timings::default())
            .await
            .expect("finish");
        let committed = match spoken {
            Some(t) => crate::dictate::classify(&t),
            None => fallback,
        };
        let commit_ms = key_up.elapsed().as_millis();
        session.disconnect().await;
        eprintln!(
            "[commit] mode={} latency={commit_ms}ms after key-up",
            if crate::realtime::manual_turns() { "manual-turns" } else { "server-vad" }
        );

        let text = match &committed {
            Committed::Typed(t) => t.clone(),
            other => panic!("expected a typed commit, got {other:?}"),
        };
        eprintln!("[commit] {text:?}");

        let expected = std::env::var("CODEX_MIC_TEST_PHRASES").unwrap_or_default();
        let fragments: Vec<&str> = expected
            .split('|')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if fragments.is_empty() {
            eprintln!("[commit] set CODEX_MIC_TEST_PHRASES to assert on content");
            return;
        }
        let missing: Vec<&&str> = fragments.iter().filter(|f| !text.contains(**f)).collect();
        assert!(
            missing.is_empty(),
            "the commit dropped {missing:?}\n  committed: {text:?}"
        );
        eprintln!("[commit] all {} expected fragments survived", fragments.len());
    }

    /// Does asking the session model degrade as a session goes on?
    ///
    /// Every `response.create` appends to the conversation, and the model reads
    /// the whole thing each time — unlike the transcription side channel, which
    /// treats each utterance independently. A dictation session lives up to an
    /// hour, so this checks the thing that would make the model path unusable:
    /// latency creeping up, or an earlier dictation bleeding into a later one.
    #[tokio::test]
    async fn probe_model_transcription_over_a_long_session() {
        if !enabled() {
            eprintln!("skipping; set CODEX_MIC_INTEGRATION=1 to run");
            return;
        }
        // Several different utterances, cycled: repeating one recording could
        // never reveal an earlier dictation bleeding into a later one.
        let paths = std::env::var("CODEX_MIC_TEST_PCMS")
            .or_else(|_| std::env::var("CODEX_MIC_TEST_PCM"))
            .unwrap_or_default();
        let clips: Vec<(String, Vec<u8>)> = paths
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .filter_map(|p| {
                let raw = std::fs::read(p).ok()?;
                let pcm = raw
                    .chunks_exact(4)
                    .flat_map(|c| {
                        let a = i16::from_le_bytes([c[0], c[1]]) as i32;
                        let b = i16::from_le_bytes([c[2], c[3]]) as i32;
                        (((a + b) / 2) as i16).to_le_bytes()
                    })
                    .collect();
                let name = std::path::Path::new(p)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.to_string());
                Some((name, pcm))
            })
            .collect();
        if clips.is_empty() {
            eprintln!("skipping; set CODEX_MIC_TEST_PCMS to comma-separated pcm paths");
            return;
        }
        let rounds: usize = std::env::var("CODEX_MIC_ROUNDS")
            .ok()
            .and_then(|r| r.parse().ok())
            .unwrap_or(5);

        let emitter: Emitter = Arc::new(|_: &str, _: Value| {});
        let (session, _) = RealtimeSession::connect(emitter).await.expect("connect");
        let frame = (crate::realtime::SESSION_SAMPLE_RATE as usize / 50) * 2;

        for round in 1..=rounds {
            let (name, pcm) = &clips[(round - 1) % clips.len()];
            // Faster than real time: this is about conversation growth, not
            // capture pacing.
            for chunk in pcm.chunks(frame) {
                session.append_pcm(chunk).await.expect("append_pcm");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let key_up = Instant::now();
            session.flush_tail(SILENCE_TAIL).await.expect("flush tail");
            match crate::realtime::transcribe_committed_audio(&session, Duration::from_secs(20))
                .await
            {
                Ok(t) => eprintln!(
                    "[round {round}] {name:<10} {:>5}ms  {:?}",
                    key_up.elapsed().as_millis(),
                    t.trim()
                ),
                Err(e) => eprintln!("[round {round}] {name:<10} FAILED  {e}"),
            }
        }
        session.disconnect().await;
    }


    /// Which session models does this endpoint accept, and which
    /// (session × transcription) pairs actually work?
    ///
    /// Both halves are server-side facts that change without notice, so they
    /// are measured rather than assumed. Every pair transcribes the same
    /// recording, so the output column is directly comparable.
    #[tokio::test]
    async fn probe_model_pairs() {
        if !enabled() {
            eprintln!("skipping; set CODEX_MIC_INTEGRATION=1 to run");
            return;
        }
        let Ok(pcm_path) = std::env::var("CODEX_MIC_TEST_PCM") else {
            eprintln!("skipping; set CODEX_MIC_TEST_PCM");
            return;
        };
        let raw = std::fs::read(&pcm_path).expect("read test pcm");
        let pcm: Vec<u8> = raw
            .chunks_exact(4)
            .flat_map(|c| {
                let a = i16::from_le_bytes([c[0], c[1]]) as i32;
                let b = i16::from_le_bytes([c[2], c[3]]) as i32;
                (((a + b) / 2) as i16).to_le_bytes()
            })
            .collect();

        let sessions = std::env::var("CODEX_MIC_REALTIME_CANDIDATES")
            .unwrap_or_else(|_| crate::realtime::default_realtime_model().to_string());
        let transcribers = std::env::var("CODEX_MIC_TRANSCRIBE_CANDIDATES")
            .unwrap_or_else(|_| crate::realtime::default_transcription_model().to_string());

        for session_model in sessions.split(',').map(str::trim).filter(|m| !m.is_empty()) {
            std::env::set_var("CODEX_MIC_REALTIME_MODEL", session_model);
            for transcriber in transcribers.split(',').map(str::trim).filter(|m| !m.is_empty()) {
                std::env::set_var("CODEX_MIC_TRANSCRIBE_MODEL", transcriber);
                let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
                let sink = errors.clone();
                let emitter: Emitter = Arc::new(move |name: &str, params: Value| {
                    if name == "realtime://error" {
                        if let Some(m) = params.get("message").and_then(|m| m.as_str()) {
                            sink.lock().unwrap().push(m.to_string());
                        }
                    }
                });
                let session = match RealtimeSession::connect(emitter).await {
                    Ok((s, _)) => s,
                    Err(e) => {
                        // Print the reason: a refusal often names what the
                        // endpoint would have accepted instead.
                        eprintln!("[pair] {session_model:<24} {transcriber:<34}   ---  {e}");
                        // A session the endpoint will not open fails for every
                        // transcriber; no point trying the rest.
                        break;
                    }
                };
                let dictate = crate::dictate::DictateState::new();
                let _ = dictate.start_listening(session.subscribe()).await;
                let frame = (crate::realtime::SESSION_SAMPLE_RATE as usize / 50) * 2;
                for chunk in pcm.chunks(frame) {
                    if session.append_pcm(chunk).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                let started = Instant::now();
                let _ = session.flush_tail(SILENCE_TAIL).await;
                dictate.expect_turn().await;
                let out = dictate
                    .finish(crate::dictate::Timings {
                        open_turn: Duration::from_secs(15),
                        deadline: Duration::from_secs(20),
                        ..Default::default()
                    })
                    .await;
                let ms = started.elapsed().as_millis();
                session.disconnect().await;
                match out {
                    Ok(Committed::Typed(t)) => {
                        eprintln!("[pair] {session_model:<24} {transcriber:<34} {ms:>5}ms {t:?}")
                    }
                    other => {
                        let why = errors
                            .lock()
                            .unwrap()
                            .first()
                            .cloned()
                            .unwrap_or_else(|| format!("{other:?}"));
                        eprintln!("[pair] {session_model:<24} {transcriber:<34}   ---  {why}")
                    }
                }
            }
        }
        std::env::remove_var("CODEX_MIC_REALTIME_MODEL");
        std::env::remove_var("CODEX_MIC_TRANSCRIBE_MODEL");
    }

    /// Ask the live endpoint which transcription models it actually accepts,
    /// and what each one makes of identical audio.
    ///
    /// Not an assertion — a report. Which models a deployment will take is a
    /// server-side fact that changes without notice, so it is worth measuring
    /// rather than hardcoding from memory. Candidates come from
    /// `CODEX_MIC_TRANSCRIBE_CANDIDATES` (comma-separated) or the list below.
    #[tokio::test]
    async fn probe_transcription_models() {
        if !enabled() {
            eprintln!("skipping; set CODEX_MIC_INTEGRATION=1 to run");
            return;
        }
        let Ok(pcm_path) = std::env::var("CODEX_MIC_TEST_PCM") else {
            eprintln!("skipping; set CODEX_MIC_TEST_PCM to a 48kHz mono pcm16le file");
            return;
        };
        let raw = std::fs::read(&pcm_path).expect("read test pcm");
        let pcm: Vec<u8> = raw
            .chunks_exact(4)
            .flat_map(|c| {
                let a = i16::from_le_bytes([c[0], c[1]]) as i32;
                let b = i16::from_le_bytes([c[2], c[3]]) as i32;
                (((a + b) / 2) as i16).to_le_bytes()
            })
            .collect();

        let candidates = std::env::var("CODEX_MIC_TRANSCRIBE_CANDIDATES").unwrap_or_else(|_| {
            "gpt-4o-transcribe,gpt-4o-mini-transcribe,whisper-1,\
             gpt-4o-transcribe-latest,gpt-4o-transcribe-diarize"
                .to_string()
        });

        for model in candidates.split(',').map(str::trim).filter(|m| !m.is_empty()) {
            std::env::set_var("CODEX_MIC_TRANSCRIBE_MODEL", model);
            let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
            let sink = errors.clone();
            let emitter: Emitter = Arc::new(move |name: &str, params: Value| {
                if name == "realtime://error" {
                    if let Some(m) = params.get("message").and_then(|m| m.as_str()) {
                        sink.lock().unwrap().push(m.to_string());
                    }
                }
            });

            let Ok((session, _)) = RealtimeSession::connect(emitter).await else {
                eprintln!("[probe] {model:<28} CONNECT FAILED");
                continue;
            };
            let dictate = crate::dictate::DictateState::new();
            let _ = dictate.start_listening(session.subscribe()).await;

            let frame = (crate::realtime::SESSION_SAMPLE_RATE as usize / 50) * 2;
            let mut ticker = tokio::time::interval(Duration::from_millis(20));
            let mut send_failed = false;
            for chunk in pcm.chunks(frame) {
                ticker.tick().await;
                if session.append_pcm(chunk).await.is_err() {
                    send_failed = true;
                    break;
                }
            }
            let started = Instant::now();
            let _ = session.flush_tail(SILENCE_TAIL).await;
            dictate.expect_turn().await;
            // Generous windows: the point is to measure how long each model
            // really takes, not to re-impose the production deadline on models
            // that may be slower than it.
            let out = dictate
                .finish(crate::dictate::Timings {
                    deadline: Duration::from_secs(25),
                    quiet: Duration::from_secs(3),
                    open_turn: Duration::from_secs(20),
                    settle: Duration::from_secs(1),
                    no_event: Duration::from_secs(10),
                    poll: Duration::from_millis(25),
                })
                .await;
            let ms = started.elapsed().as_millis();
            session.disconnect().await;

            let errs = errors.lock().unwrap().clone();
            match out {
                Ok(Committed::Typed(t)) if !send_failed => {
                    eprintln!("[probe] {model:<28} OK {ms:>5}ms  {t:?}");
                }
                other => {
                    let why = errs.first().cloned().unwrap_or_else(|| format!("{other:?}"));
                    eprintln!("[probe] {model:<28} REJECTED  {why}");
                }
            }
        }
        std::env::remove_var("CODEX_MIC_TRANSCRIBE_MODEL");
    }
}
