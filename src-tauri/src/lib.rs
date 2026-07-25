mod audio;
mod auth;
mod commands;
mod config;
mod dictate;
mod error;
#[allow(dead_code)] // PCMU fallback path kept from the codec probes
mod g711;
mod realtime;

use commands::AppState;
use dictate::Committed;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager, WebviewWindow};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tracing::{error, info, warn};

const DEBOUNCE: Duration = Duration::from_millis(400);
const PCM_PUMP_INTERVAL: Duration = Duration::from_millis(50);
/// Pill height in its collapsed state; the settings panel expands it.
const PILL_HEIGHT: f64 = 56.0;
const SETTINGS_HEIGHT: f64 = 620.0;
const WINDOW_WIDTH: f64 = 420.0;

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
            width: WINDOW_WIDTH,
            height: SETTINGS_HEIGHT,
        }));
        let _ = window.set_focus();
        let _ = app.emit("settings://open", serde_json::json!({}));
    } else {
        let _ = app.emit("settings://close", serde_json::json!({}));
        let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize {
            width: WINDOW_WIDTH,
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

static TOGGLE_LOCK: AtomicBool = AtomicBool::new(false);
static LAST_TOGGLE: std::sync::OnceLock<std::sync::Mutex<Instant>> = std::sync::OnceLock::new();

struct ToggleGuard;
impl Drop for ToggleGuard {
    fn drop(&mut self) {
        TOGGLE_LOCK.store(false, Ordering::SeqCst);
    }
}

fn debounced() -> bool {
    let last = LAST_TOGGLE.get_or_init(|| std::sync::Mutex::new(Instant::now() - Duration::from_secs(10)));
    let mut l = match last.lock() {
        Ok(l) => l,
        Err(poisoned) => poisoned.into_inner(),
    };
    if l.elapsed() < DEBOUNCE {
        return true;
    }
    *l = Instant::now();
    false
}

/// Tracks whether the dictation hotkey is physically held. Windows auto-repeat
/// re-fires global hotkeys while a key is held, which in toggle mode made the
/// recording strobe on/off during a single long press — the "canceled when I
/// let go" bug. Only a fresh press (down after up) may act.
static DICTATION_KEY_DOWN: AtomicBool = AtomicBool::new(false);

async fn toggle(app: tauri::AppHandle, window: WebviewWindow) {
    if TOGGLE_LOCK.swap(true, Ordering::SeqCst) {
        return;
    }
    let _guard = ToggleGuard;
    if debounced() {
        return;
    }

    if app.state::<AppState>().dictate.is_listening().await {
        // Stop must never block on connecting — otherwise a slow or hung
        // connect leaves the user stuck in the recording state.
        stop_dictation(&app).await;
    } else {
        start_dictation(&app, &window).await;
    }
}

async fn start_dictation(app: &tauri::AppHandle, window: &WebviewWindow) {
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
            let _ = app.emit("dictate://error", serde_json::json!({ "message": e }));
            return;
        }
    };
    *state.capture.lock().await = Some(capture);

    if let Err(e) = ensure_connected(app).await {
        if let Some(mut c) = state.capture.lock().await.take() {
            c.stop();
        }
        let _ = app.emit("dictate://error", serde_json::json!({ "message": e }));
        return;
    }
    let session = match state.session().await {
        Some(s) => s,
        None => {
            if let Some(mut c) = state.capture.lock().await.take() {
                c.stop();
            }
            let _ = app.emit("dictate://error", serde_json::json!({ "message": "not connected" }));
            return;
        }
    };

    if let Err(e) = state.dictate.start_listening(&session).await {
        if let Some(mut c) = state.capture.lock().await.take() {
            c.stop();
        }
        let _ = app.emit("dictate://error", serde_json::json!({ "message": e }));
        return;
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    PEAK_RMS.store(0, Ordering::SeqCst);
    spawn_pcm_pump(app.clone(), session, stop_flag.clone());
    PUMP_STOP.lock().unwrap().replace(stop_flag);

    let _ = app.emit("dictate://started", serde_json::json!({}));
    configure_overlay(window);
}

/// Signals the currently running PCM pump to exit. Without this, every
/// recording left a pump task alive for the process lifetime, and after N
/// dictations N pumps would emit the same audio N times over.
static PUMP_STOP: std::sync::Mutex<Option<Arc<AtomicBool>>> = std::sync::Mutex::new(None);

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
) {
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
                return;
            }
        }
        // Out of the loop by silence, not by hotkey: commit like a manual stop.
        if !stop.load(Ordering::SeqCst) {
            stop_dictation(&app).await;
        }
    });
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
                    if app.state::<AppState>().dictate.is_listening().await {
                        warn!("realtime session failed; releasing microphone");
                        abort_dictation(&app).await;
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
    let state = app.state::<AppState>();
    if let Some(flag) = PUMP_STOP.lock().unwrap().take() {
        flag.store(true, Ordering::SeqCst);
    }
    if let Some(mut c) = state.capture.lock().await.take() {
        c.stop();
    }
    state.dictate.abort().await;
}

async fn stop_dictation(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let _ = app.emit("dictate://processing", serde_json::json!({}));

    if let Some(flag) = PUMP_STOP.lock().unwrap().take() {
        flag.store(true, Ordering::SeqCst);
    }
    // Release the microphone first: the user pressed stop, the mic indicator
    // should go out immediately regardless of what the server does next.
    if let Some(mut c) = state.capture.lock().await.take() {
        c.stop();
    }

    match state.dictate.stop_listening().await {
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

            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let Some(window) = app.get_webview_window("main") else {
                    warn!("main window not found");
                    return;
                };
                match cfg.activation_mode {
                    config::ActivationMode::Toggle => match event.state {
                        ShortcutState::Pressed => {
                            if DICTATION_KEY_DOWN.swap(true, Ordering::SeqCst) {
                                return; // auto-repeat while held
                            }
                            toggle(app, window).await;
                        }
                        _ => {
                            DICTATION_KEY_DOWN.store(false, Ordering::SeqCst);
                        }
                    },
                    config::ActivationMode::PushToTalk => match event.state {
                        ShortcutState::Pressed => {
                            if DICTATION_KEY_DOWN.swap(true, Ordering::SeqCst) {
                                return; // auto-repeat while held
                            }
                            if !app.state::<AppState>().dictate.is_listening().await {
                                start_dictation(&app, &window).await;
                            }
                        }
                        _ => {
                            DICTATION_KEY_DOWN.store(false, Ordering::SeqCst);
                            if app.state::<AppState>().dictate.is_listening().await {
                                stop_dictation(&app).await;
                            }
                        }
                    },
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
        assert_eq!(info.auth_mode, "chatgpt-oauth");

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
        let frame_bytes = crate::realtime::OPUS_FRAME_SAMPLES * 2;
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
                        transcript = t.to_string();
                    }
                }
            }
            if !transcript.trim().is_empty() {
                break;
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
        assert!(
            !transcript.trim().is_empty(),
            "no user transcript arrived within 30s of streaming speech"
        );
        session.disconnect().await;
    }
}
