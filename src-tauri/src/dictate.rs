use crate::codex::CodexSession;
use crate::error::RpcError;
use crate::jsonrpc::Notification;
use enigo::{Enigo, Keyboard, Settings};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

type SharedBuf = Arc<Mutex<String>>;

pub struct DictateState {
    session: Mutex<Option<Arc<CodexSession>>>,
    buffer: SharedBuf,
    listening: Mutex<bool>,
}

impl DictateState {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
            buffer: Arc::new(Mutex::new(String::new())),
            listening: Mutex::new(false),
        }
    }

    pub async fn set_session(&self, session: Arc<CodexSession>) {
        *self.session.lock().await = Some(session);
    }

    pub async fn has_session(&self) -> bool {
        self.session.lock().await.is_some()
    }

    pub async fn is_listening(&self) -> bool {
        *self.listening.lock().await
    }

    pub async fn start_listening(&self) -> Result<(), String> {
        let session = self
            .session
            .lock()
            .await
            .clone()
            .ok_or_else(|| "not connected".to_string())?;
        *self.buffer.lock().await = String::new();
        *self.listening.lock().await = true;
        let rx = session.subscribe();
        let buf = self.buffer.clone();
        tokio::spawn(accumulate_loop(rx, buf));
        Ok(())
    }

    pub async fn stop_listening(&self) -> Result<String, String> {
        *self.listening.lock().await = false;
        let text = self.buffer.lock().await.clone();
        self.buffer.lock().await.clear();
        if !text.is_empty() {
            type_text(&text).map_err(|e| e.to_string())?;
        }
        Ok(text)
    }

    pub async fn current_buffer(&self) -> String {
        self.buffer.lock().await.clone()
    }
}

impl Default for DictateState {
    fn default() -> Self {
        Self::new()
    }
}

async fn accumulate_loop(mut rx: tokio::sync::broadcast::Receiver<Notification>, buf: SharedBuf) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(n) if is_user_transcript_delta(&n.method, &n.params) => {
                if let Some(delta) = n.params.get("delta").and_then(|d| d.as_str()) {
                    debug!(%delta, "transcript delta");
                    buf.lock().await.push_str(delta);
                }
            }
            Ok(_) => {}
            Err(RecvError::Closed) => break,
            Err(RecvError::Lagged(k)) => warn!(skipped = k, "dictate lagged"),
        }
    }
}

pub fn type_text(text: &str) -> Result<(), RpcError> {
    if text.is_empty() {
        return Ok(());
    }
    let mut enigo = Enigo::new(&Settings::default())
        .map_err(|e| RpcError::Spawn(format!("enigo init: {e}")))?;
    enigo
        .text(text)
        .map_err(|e| RpcError::Spawn(format!("enigo text: {e}")))?;
    info!(len = text.len(), "typed transcript");
    Ok(())
}

pub fn is_user_transcript_delta(method: &str, params: &Value) -> bool {
    method == "thread/realtime/transcript/delta"
        && params.get("role").and_then(|r| r.as_str()) == Some("user")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_delta_passes_filter() {
        assert!(is_user_transcript_delta(
            "thread/realtime/transcript/delta",
            &json!({ "role": "user", "delta": "안녕" })
        ));
    }

    #[test]
    fn assistant_delta_blocked() {
        assert!(!is_user_transcript_delta(
            "thread/realtime/transcript/delta",
            &json!({ "role": "assistant", "delta": "hi" })
        ));
    }

    #[test]
    fn non_transcript_method_blocked() {
        assert!(!is_user_transcript_delta(
            "thread/realtime/sdp",
            &json!({ "role": "user" })
        ));
    }

    #[test]
    fn missing_role_blocked() {
        assert!(!is_user_transcript_delta(
            "thread/realtime/transcript/delta",
            &json!({ "delta": "x" })
        ));
    }
}
