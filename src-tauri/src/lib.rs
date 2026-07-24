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
    let _ = window.set_focus();
}

fn hide_indicator(window: &WebviewWindow) {
    let _ = window.hide();
}

async fn ensure_connected(app: tauri::AppHandle) {
    let state = app.state::<AppState>();
    if state.dictate.has_session().await {
        return;
    }
    let app2 = app.clone();
    match crate::codex::CodexSession::connect(commands_state_emitter(&app2)).await {
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

fn commands_state_emitter(app: &tauri::AppHandle) -> crate::codex::Emitter {
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
        match state.dictate.stop_listening().await {
            Ok(typed) => {
                info!(len = typed.len(), "dictation committed");
                let _ = app.emit("dictate://stopped", serde_json::json!({ "text": typed }));
            }
            Err(e) => warn!(error = %e, "stop failed"),
        }
        hide_indicator(&window);
    } else {
        match state.dictate.start_listening().await {
            Ok(()) => {
                let _ = app.emit("dictate://started", serde_json::json!({}));
                show_indicator(&window);
            }
            Err(e) => warn!(error = %e, "start failed"),
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

    let mut builder = commands::builder();

    builder = builder.plugin(
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

    builder = builder.setup(|app| {
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
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn enabled() -> bool {
        std::env::var("CODEX_MIC_INTEGRATION").is_ok()
    }

    #[tokio::test]
    async fn codex_realtime_lifecycle_against_real_binary() {
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
            .expect("connect to codex app-server");
        assert!(!info.thread_id.is_empty(), "thread id should be set");

        session
            .realtime_start("v=0\r\n".into())
            .await
            .expect("realtime_start must be accepted by the server");

        let deadline = Instant::now() + Duration::from_secs(12);
        let mut saw_error = false;
        while Instant::now() < deadline {
            let hit = events
                .lock()
                .unwrap()
                .iter()
                .any(|(n, _)| n == "realtime://error");
            if hit {
                saw_error = true;
                break;
            }
            drop(events.lock().unwrap());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            saw_error,
            "expected a realtime://error notification to flow back through the client"
        );

        let _ = session.realtime_stop().await;
        session.disconnect().await;
    }
}
