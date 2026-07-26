use crate::audio::AudioCapture;
use crate::dictate::DictateState;
use crate::error::RpcError;
use crate::realtime::{ConnectInfo, Emitter, RealtimeSession};
use serde_json::Value;
use std::sync::Arc;
use tauri::{AppHandle, Emitter as _, Manager, State};
use tokio::sync::Mutex;

pub struct AppState {
    /// The one and only owner of the live session. `DictateState` used to keep a
    /// second copy, which drifted out of sync whenever a connect failed midway.
    pub session: Mutex<Option<Arc<RealtimeSession>>>,
    pub info: Mutex<Option<ConnectInfo>>,
    pub dictate: DictateState,
    /// Serializes connect attempts so a startup auto-connect racing the first
    /// hotkey press cannot open two realtime calls.
    pub connect_lock: Mutex<()>,
    /// Held for the duration of a recording so the microphone is released on
    /// stop instead of running until the process exits.
    pub capture: Mutex<Option<AudioCapture>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
            info: Mutex::new(None),
            dictate: DictateState::new(),
            connect_lock: Mutex::new(()),
            capture: Mutex::new(None),
        }
    }

    pub async fn session(&self) -> Option<Arc<RealtimeSession>> {
        self.session.lock().await.clone()
    }

    /// A session worth streaming into: present, connected, and not expiring.
    pub async fn usable_session(&self) -> Option<Arc<RealtimeSession>> {
        let session = self.session.lock().await.clone()?;
        if session.is_usable().await {
            Some(session)
        } else {
            None
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

fn err(e: RpcError) -> String {
    e.to_string()
}

fn make_emitter(app: AppHandle) -> Emitter {
    Arc::new(move |event: &str, payload: Value| {
        let _ = app.emit(event, payload);
    })
}

#[tauri::command]
async fn connect(app: AppHandle, state: State<'_, AppState>) -> Result<ConnectInfo, String> {
    let _guard = state.connect_lock.lock().await;
    if let Some(existing) = state.session.lock().await.take() {
        existing.disconnect().await;
    }
    *state.info.lock().await = None;
    let emitter = make_emitter(app);
    let (session, info) = RealtimeSession::connect(emitter).await.map_err(err)?;
    *state.info.lock().await = Some(info.clone());
    *state.session.lock().await = Some(Arc::new(session));
    Ok(info)
}

#[tauri::command]
async fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(s) = state.session.lock().await.take() {
        s.disconnect().await;
    }
    *state.info.lock().await = None;
    if let Some(mut c) = state.capture.lock().await.take() {
        c.stop();
    }
    Ok(())
}

/// Whether a Codex CLI OAuth login exists. Dictation authenticates with it, so
/// the pill can say `codex login` is needed up front instead of failing on the
/// first hotkey press.
#[tauri::command]
async fn has_oauth() -> Result<bool, String> {
    Ok(crate::auth::has_oauth())
}

#[tauri::command]
async fn get_config() -> Result<crate::config::Config, String> {
    Ok(crate::config::get())
}

/// Persist a new config and apply the parts that need live action: hotkeys are
/// re-registered in place, and a language change drops the realtime session so
/// the next dictation reconnects with fresh instructions.
#[tauri::command]
async fn set_config(
    app: AppHandle,
    state: State<'_, AppState>,
    config: crate::config::Config,
) -> Result<(), String> {
    let previous = crate::config::get();
    crate::config::set(config.clone())?;

    if previous.hotkey != config.hotkey || previous.settings_hotkey != config.settings_hotkey {
        crate::apply_hotkeys(&app)?;
    }
    // Both are baked into `session.update` at connect time, so an open session
    // would keep using the old ones until it aged out.
    if previous.language != config.language
        || previous.transcribe_model != config.transcribe_model
        || previous.realtime_model != config.realtime_model
    {
        if let Some(s) = state.session.lock().await.take() {
            s.disconnect().await;
        }
    }
    let _ = app.emit("config://changed", serde_json::json!({}));
    Ok(())
}

/// Input device names for the settings dropdown.
#[tauri::command]
async fn list_mics() -> Result<Vec<String>, String> {
    Ok(crate::audio::list_input_devices())
}

/// Transcription models for the settings dropdown, each with a one-line note
/// on how it actually behaved when measured.
#[tauri::command]
async fn list_transcribe_models() -> Result<Vec<serde_json::Value>, String> {
    let default = crate::realtime::default_transcription_model();
    Ok(crate::realtime::TRANSCRIPTION_MODELS
        .iter()
        .map(|(id, note)| {
            serde_json::json!({ "id": id, "note": note, "is_default": *id == default })
        })
        .collect())
}

/// Realtime session models for the settings dropdown.
#[tauri::command]
async fn list_realtime_models() -> Result<Vec<serde_json::Value>, String> {
    let default = crate::realtime::default_realtime_model();
    Ok(crate::realtime::REALTIME_MODELS
        .iter()
        .map(|(id, note)| {
            serde_json::json!({ "id": id, "note": note, "is_default": *id == default })
        })
        .collect())
}

#[tauri::command]
async fn close_settings(app: AppHandle) -> Result<(), String> {
    crate::set_settings_open(&app, false);
    Ok(())
}

#[tauri::command]
async fn toggle_settings(app: AppHandle) -> Result<(), String> {
    crate::toggle_settings(&app);
    Ok(())
}

/// Begin an OS-level window drag. Called from the pill's mousedown — the drag
/// has to start while the button is still held.
#[tauri::command]
async fn start_drag(app: AppHandle) -> Result<(), String> {
    app.get_webview_window("main")
        .ok_or_else(|| "no main window".to_string())?
        .start_dragging()
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn quit_app(app: AppHandle) -> Result<(), String> {
    app.exit(0);
    Ok(())
}

/// Settings "injection test": runs the real injection pipeline with sample
/// text so the user can verify typing/paste works in their target app.
#[tauri::command]
async fn test_inject(text: String) -> Result<String, String> {
    crate::dictate::test_inject(&text).await
}

#[tauri::command]
async fn is_listening(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(state.dictate.is_listening().await)
}

#[tauri::command]
async fn buffer(state: State<'_, AppState>) -> Result<String, String> {
    Ok(state.dictate.current_buffer().await)
}

#[tauri::command]
async fn status(state: State<'_, AppState>) -> Result<Option<ConnectInfo>, String> {
    Ok(state.info.lock().await.clone())
}

pub fn builder() -> tauri::Builder<tauri::Wry> {
    tauri::Builder::default()
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            connect,
            disconnect,
            has_oauth,
            get_config,
            set_config,
            list_transcribe_models,
            list_realtime_models,
            list_mics,
            close_settings,
            toggle_settings,
            start_drag,
            quit_app,
            test_inject,
            is_listening,
            buffer,
            status,
        ])
}
