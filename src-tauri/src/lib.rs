mod audio;
mod codex;
mod commands;
mod dictate;
mod error;
mod jsonrpc;

use commands::AppState;
use tauri::{Emitter, Manager, WebviewWindow};
use tauri_plugin_global_shortcut::{Shortcut, ShortcutState};
use tracing::{info, warn};

const HOTKEY: &str = "Ctrl+E";

fn show_indicator(window: &WebviewWindow) {
    let _ = window.show();
    let _ = window.set_ignore_cursor_events(true);
}

fn hide_indicator(window: &WebviewWindow) {
    let _ = window.set_ignore_cursor_events(false);
    let _ = window.hide();
}

async fn ensure_connected(app: tauri::AppHandle) {
    let state = app.state::<AppState>();
    if state.dictate.has_session().await {
        return;
    }
    let app2 = app.clone();
    match crate::codex::CodexSession::connect(make_emitter(&app2)).await {
        Ok((session, info)) => {
            let session = std::sync::Arc::new(session);
            state.dictate.set_session(session.clone()).await;
            *state.session.lock().await = Some(session);
            *state.info.lock().await = Some(info);
            info!("auto-connected to codex app-server");
        }
        Err(e) => warn!(error = %e, "auto-connect failed"),
    }
}

fn make_emitter(app: &tauri::AppHandle) -> crate::codex::Emitter {
    let app = app.clone();
    std::sync::Arc::new(move |event: &str, payload: serde_json::Value| {
        let _ = app.emit(event, payload);
    })
}

async fn toggle(app: tauri::AppHandle, window: WebviewWindow) {
    let state = app.state::<AppState>();
    if !state.dictate.has_session().await {
        ensure_connected(app.clone()).await;
    }
    if state.dictate.is_listening().await {
        let _ = app.emit("dictate://processing", serde_json::json!({}));
        match state.dictate.stop_listening().await {
            Ok(text) => {
                info!(len = text.len(), "dictation committed");
                let _ = app.emit("dictate://stopped", serde_json::json!({ "text": text }));
            }
            Err(e) => {
                warn!(error = %e, "stop failed");
                let _ = app.emit("dictate://error", serde_json::json!({ "message": e }));
            }
        }
        if let Some(s) = state.session.lock().await.clone() {
            let _ = s.realtime_stop().await;
        }
        hide_indicator(&window);
    } else {
        let session = match state.dictate.start_listening().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "start failed");
                let _ = app.emit("dictate://error", serde_json::json!({ "message": e }));
                return;
            }
        };
        if let Err(e) = session.realtime_start().await {
            warn!(error = %e, "realtime_start failed");
            let _ = app.emit("dictate://error", serde_json::json!({ "message": e.to_string() }));
            return;
        }
        match crate::audio::AudioCapture::start() {
            Ok(capture) => {
                let session_clone = session.clone();
                tokio::spawn(async move {
                    let mut interval =
                        tokio::time::interval(std::time::Duration::from_millis(100));
                    interval.tick().await;
                    loop {
                        interval.tick().await;
                        for b64 in capture.read_all_pending() {
                            if let Err(e) = session_clone.append_audio(b64).await {
                                tracing::warn!(error = %e, "append_audio failed");
                                return;
                            }
                        }
                    }
                });
                let _ = app.emit("dictate://started", serde_json::json!({}));
                show_indicator(&window);
            }
            Err(e) => {
                warn!(error = %e, "audio capture failed");
                let _ = app.emit("dictate://error", serde_json::json!({ "message": e }));
                let _ = session.realtime_stop().await;
            }
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "codex_mic=info".into()),
        )
        .init();

    let shortcut: Shortcut = HOTKEY.parse().expect("valid shortcut");

    let builder = commands::builder().plugin(
        tauri_plugin_global_shortcut::Builder::new()
            .with_shortcut(shortcut)
            .expect("register shortcut")
            .with_handler(move |app, _s, event| {
                if event.state != ShortcutState::Pressed {
                    return;
                }
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let window = match app.get_webview_window("main") {
                        Some(w) => w,
                        None => return,
                    };
                    toggle(app, window).await;
                });
            })
            .build(),
    );

    let builder = builder.setup(|app| {
        let app_handle = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            ensure_connected(app_handle).await;
        });
        Ok(())
    });

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod integration {
    use crate::codex::{CodexSession, Emitter};
    use base64::Engine;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn enabled() -> bool {
        std::env::var("CODEX_MIC_INTEGRATION").is_ok()
    }

    #[tokio::test]
    async fn codex_realtime_native_audio_pipeline() {
        if !enabled() {
            eprintln!("skipping; set CODEX_MIC_INTEGRATION=1 to run");
            return;
        }
        let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(vec![]));
        let sink = events.clone();
        let emitter: Emitter = Arc::new(move |name: &str, payload: Value| {
            sink.lock().unwrap().push((name.to_string(), payload));
        });

        let (session, info) = CodexSession::connect(emitter)
            .await
            .expect("connect");
        assert!(!info.thread_id.is_empty());

        session.realtime_start().await.expect("realtime_start");

        let dummy_pcm =
            base64::engine::general_purpose::STANDARD.encode([0u8; 4800]);
        session.append_audio(dummy_pcm).await.expect("appendAudio");

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut saw_event = false;
        while Instant::now() < deadline {
            let hit = events
                .lock()
                .unwrap()
                .iter()
                .any(|(n, _)| n == "realtime://error" || n == "realtime://closed");
            if hit {
                saw_event = true;
                break;
            }
            drop(events.lock().unwrap());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(saw_event, "expected realtime event after appendAudio");

        let _ = session.realtime_stop().await;
        session.disconnect().await;
    }
}
