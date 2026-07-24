use crate::codex::{CodexSession, ConnectInfo, Emitter};
use crate::dictate::DictateState;
use crate::error::RpcError;
use serde_json::Value;
use std::sync::Arc;
use tauri::{AppHandle, Emitter as _, State};
use tokio::sync::Mutex;

pub struct AppState {
    pub session: Mutex<Option<Arc<CodexSession>>>,
    pub info: Mutex<Option<ConnectInfo>>,
    pub dictate: DictateState,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
            info: Mutex::new(None),
            dictate: DictateState::new(),
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
    if let Some(existing) = state.session.lock().await.take() {
        existing.disconnect().await;
    }
    let emitter = make_emitter(app);
    let (session, info) = CodexSession::connect(emitter).await.map_err(err)?;
    let session = Arc::new(session);
    state.dictate.set_session(session.clone()).await;
    *state.info.lock().await = Some(info.clone());
    *state.session.lock().await = Some(session);
    Ok(info)
}

#[tauri::command]
async fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(s) = state.session.lock().await.take() {
        s.disconnect().await;
    }
    *state.info.lock().await = None;
    Ok(())
}

#[tauri::command]
async fn realtime_start(state: State<'_, AppState>, sdp_offer: String) -> Result<(), String> {
    let s = state
        .session
        .lock()
        .await
        .clone()
        .ok_or_else(|| "not connected".to_string())?;
    s.realtime_start(sdp_offer).await.map_err(err)
}

#[tauri::command]
async fn realtime_stop(state: State<'_, AppState>) -> Result<(), String> {
    let s = state
        .session
        .lock()
        .await
        .clone()
        .ok_or_else(|| "not connected".to_string())?;
    s.realtime_stop().await.map_err(err)
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
            realtime_start,
            realtime_stop,
            is_listening,
            buffer,
            status,
        ])
}
