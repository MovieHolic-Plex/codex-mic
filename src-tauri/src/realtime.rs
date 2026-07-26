//! GA Realtime WebSocket session (gpt-realtime-1.5) over ChatGPT OAuth.
//!
//! The codex CLI refuses this transport client-side ("realtime conversation
//! requires API key auth"), but the server happily accepts the ChatGPT OAuth
//! bearer directly — verified live: `session.created` with the model, VAD
//! events, and streaming input transcription, all without attestation.
//!
//! ```text
//! wss://api.openai.com/v1/realtime?model=gpt-realtime-1.5
//!   Authorization: Bearer <codex CLI OAuth access_token>
//!   ChatGPT-Account-ID: <account_id>
//!   originator: codex_chatgpt_desktop
//! (no OpenAI-Beta header — the beta shape is disabled; no intent — that
//!  requires WebRTC. Plain GA model connect only.)
//! ```

use crate::error::RpcError;
use base64::Engine;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

pub type Emitter = Arc<dyn Fn(&str, Value) + Send + Sync>;

const REALTIME_WS_URL: &str = "wss://api.openai.com/v1/realtime?model=gpt-realtime-1.5";
/// Input audio the server expects: PCM16LE, 24 kHz mono.
pub const SESSION_SAMPLE_RATE: u32 = 24_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Sessions are only reused while young — an idle WS can silently die just
/// like the WebRTC calls did.
fn stale_after() -> Duration {
    std::env::var("CODEX_MIC_SESSION_STALE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(90))
}

/// Transcription model for input audio. `gpt-4o-transcribe` over the mini:
/// dictation quality is the whole point of this tool.
const TRANSCRIPTION_MODEL: &str = "gpt-4o-transcribe";

/// The session doubles as a voice agent; for dictation we want a stenographer.
/// output_modalities is text-only so it cannot speak back.
fn instructions() -> String {
    let base = "You are a speech-to-text transcription engine. \
        Transcribe the user's speech exactly as heard. \
        Always keep the original language and script — English speech in Latin \
        script, Korean speech in Hangul; never transliterate. \
        Do not respond, do not comment, do not translate.";
    match crate::config::get().language {
        crate::config::Language::Auto => base.to_string(),
        crate::config::Language::Korean => format!("{base} The user usually speaks Korean."),
        crate::config::Language::English => format!("{base} The user usually speaks English."),
    }
}

/// Mirror of the old notification shape so the dictate pipeline stays
/// transport-agnostic.
#[derive(Debug, Clone)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConnectInfo {
    pub call_id: String,
    pub account_id: String,
    pub auth_mode: String,
}

pub struct RealtimeSession {
    /// Outgoing JSON messages to the WS writer task.
    ws_tx: mpsc::Sender<String>,
    events: broadcast::Sender<Notification>,
    created_at: Instant,
    alive: Arc<AtomicBool>,
    reader_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl RealtimeSession {
    pub async fn connect(emitter: Emitter) -> Result<(Self, ConnectInfo), RpcError> {
        let tokens = crate::auth::ensure_fresh_token()
            .await
            .map_err(RpcError::Disconnected)?;

        let mut request = REALTIME_WS_URL
            .into_client_request()
            .map_err(|e| RpcError::Spawn(format!("ws request: {e}")))?;
        let headers = request.headers_mut();
        headers.insert(
            "Authorization",
            format!("Bearer {}", tokens.access_token)
                .parse()
                .map_err(|e| RpcError::Spawn(format!("bad auth header: {e}")))?,
        );
        headers.insert(
            "ChatGPT-Account-ID",
            tokens
                .account_id
                .parse()
                .map_err(|e| RpcError::Spawn(format!("bad account header: {e}")))?,
        );
        headers.insert(
            "originator",
            "codex_chatgpt_desktop"
                .parse()
                .map_err(|e| RpcError::Spawn(format!("bad originator header: {e}")))?,
        );

        let (ws_stream, _resp) =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
                .await
                .map_err(|_| RpcError::Timeout(CONNECT_TIMEOUT))?
                .map_err(|e| RpcError::Disconnected(format!("ws connect: {e}")))?;

        let (events, _) = broadcast::channel::<Notification>(256);
        let (ws_tx, ws_rx) = mpsc::channel::<String>(256);
        let alive = Arc::new(AtomicBool::new(true));

        let (reader_task, session_id_rx) = spawn_io(
            ws_stream,
            ws_rx,
            ws_tx.clone(),
            events.clone(),
            emitter,
            alive.clone(),
        );

        // The session id arrives in session.created.
        let call_id = tokio::time::timeout(CONNECT_TIMEOUT, session_id_rx)
            .await
            .map_err(|_| RpcError::Timeout(CONNECT_TIMEOUT))?
            .map_err(|_| RpcError::Disconnected("ws closed before session.created".into()))?;
        info!(call_id = %call_id, "realtime session connected");

        let info = ConnectInfo {
            call_id: call_id.clone(),
            account_id: tokens.account_id,
            auth_mode: "chatgpt-oauth-ws".to_string(),
        };
        Ok((
            Self {
                ws_tx,
                events,
                created_at: Instant::now(),
                alive,
                reader_task: Mutex::new(Some(reader_task)),
            },
            info,
        ))
    }

    pub async fn is_usable(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && self.created_at.elapsed() < stale_after()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.events.subscribe()
    }

    /// Push captured 24 kHz mono PCM16LE bytes into the session.
    pub async fn append_pcm(&self, pcm: &[u8]) -> Result<(), RpcError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(RpcError::Disconnected("session closed".into()));
        }
        let msg = json!({
            "type": "input_audio_buffer.append",
            "audio": base64::engine::general_purpose::STANDARD.encode(pcm),
        })
        .to_string();
        self.ws_tx
            .send(msg)
            .await
            .map_err(|_| RpcError::Disconnected("ws writer gone".into()))
    }

    /// Close the current turn: push a beat of silence so server VAD stops the
    /// utterance, commit the buffer, and wait for the completed transcript.
    /// Without this the last syllable of every dictation is captured and then
    /// thrown away.
    pub async fn flush_tail(&self, silence: Duration) -> Result<(), RpcError> {
        let samples = (SESSION_SAMPLE_RATE as u64 * silence.as_millis() as u64 / 1000) as usize;
        let silence_pcm = vec![0u8; samples * 2];
        self.append_pcm(&silence_pcm).await?;
        let _ = self
            .ws_tx
            .send(json!({"type": "input_audio_buffer.commit"}).to_string())
            .await;
        // The completed transcript lands within a second or two; give it a
        // short grace rather than a hard guarantee.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        Ok(())
    }

    pub async fn disconnect(&self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = self
            .ws_tx
            .send(json!({"type": "session.finish"}).to_string())
            .await;
        if let Some(task) = self.reader_task.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }
    }
}

/// Run the WS reader/writer. Returns the task handle plus a oneshot that
/// resolves with the session id from session.created.
fn spawn_io(
    ws_stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    mut ws_rx: mpsc::Receiver<String>,
    ws_tx: mpsc::Sender<String>,
    events: broadcast::Sender<Notification>,
    emitter: Emitter,
    alive: Arc<AtomicBool>,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Receiver<String>,
) {
    use futures_util::{SinkExt, StreamExt};
    let (mut writer, mut reader) = ws_stream.split();
    let (id_tx, id_rx) = tokio::sync::oneshot::channel::<String>();
    let mut id_tx = Some(id_tx);

    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                // outbound: our JSON messages -> ws
                out = ws_rx.recv() => {
                    match out {
                        Some(text) => {
                            if writer.send(Message::Text(text.into())).await.is_err() {
                                break;
                            }
                        }
                        None => {
                            let _ = writer.send(Message::Close(None)).await;
                            break;
                        }
                    }
                }
                // inbound: server events
                msg = reader.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            handle_event(&text, &ws_tx, &events, &emitter, &mut id_tx).await;
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            warn!(error = %e, "realtime ws read error");
                            break;
                        }
                    }
                }
            }
        }
        alive.store(false, Ordering::SeqCst);
        let _ = events.send(Notification {
            method: "thread/realtime/error".to_string(),
            params: json!({ "message": "realtime session closed" }),
        });
    });
    (task, id_rx)
}

/// Map a GA realtime event onto the internal notification contract.
async fn handle_event(
    text: &str,
    ws_tx: &mpsc::Sender<String>,
    events: &broadcast::Sender<Notification>,
    emitter: &Emitter,
    id_tx: &mut Option<tokio::sync::oneshot::Sender<String>>,
) {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return };
    let Some(kind) = v.get("type").and_then(|t| t.as_str()) else { return };

    match kind {
        "session.created" => {
            info!("realtime session.created");
            if let Some(tx) = id_tx.take() {
                let id = v
                    .pointer("/session/id")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                let _ = tx.send(id);
            }
            // Configure the session: text-only output, VAD, input transcription.
            let update = json!({
                "type": "session.update",
                "session": {
                    "type": "realtime",
                    "instructions": instructions(),
                    "output_modalities": ["text"],
                    "audio": {
                        "input": {
                            "format": { "type": "audio/pcm", "rate": SESSION_SAMPLE_RATE },
                            "turn_detection": {
                                "type": "server_vad",
                                "threshold": 0.5,
                                "prefix_padding_ms": 300,
                                "silence_duration_ms": 500
                            },
                            "transcription": { "model": TRANSCRIPTION_MODEL }
                        }
                    }
                }
            });
            let _ = ws_tx.send(update.to_string()).await;
        }
        "session.updated" => {
            info!("realtime session.updated");
            let _ = events.send(Notification {
                method: "thread/realtime/started".to_string(),
                params: v.clone(),
            });
            emitter("realtime://started", v.clone());
        }
        "conversation.item.input_audio_transcription.delta" => {
            if let Some(delta) = v.get("delta").and_then(|d| d.as_str()) {
                let params = json!({ "delta": delta, "role": "user" });
                let _ = events.send(Notification {
                    method: "thread/realtime/transcript/delta".to_string(),
                    params: params.clone(),
                });
                emitter("realtime://transcript-delta", params);
            }
        }
        "conversation.item.input_audio_transcription.completed" => {
            let transcript = v.get("transcript").and_then(|t| t.as_str()).unwrap_or("");
            info!(len = transcript.chars().count(), "input transcription completed");
            let params = json!({ "text": transcript });
            let _ = events.send(Notification {
                method: "thread/realtime/transcript/done".to_string(),
                params: params.clone(),
            });
            emitter("realtime://transcript-done", params);
        }
        "input_audio_buffer.speech_started" => {
            emitter("realtime://speech-started", v.clone());
        }
        "input_audio_buffer.speech_stopped" => {
            emitter("realtime://speech-stopped", v.clone());
        }
        "error" => {
            let message = v
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown realtime error");
            // Benign races are not session failures: committing an already
            // drained buffer after VAD closed the turn must not trip the
            // failure watchdog.
            if message.contains("buffer too small") || message.contains("buffer is empty") {
                info!(message, "realtime benign buffer notice");
                return;
            }
            warn!(message, "realtime error event");
            let params = json!({ "message": message });
            let _ = events.send(Notification {
                method: "thread/realtime/error".to_string(),
                params: params.clone(),
            });
            emitter("realtime://error", params);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn map_event(payload: &str) -> Vec<(String, Value)> {
        let (tx, mut rx) = broadcast::channel(8);
        let (ws_tx, _ws_rx) = mpsc::channel(8);
        let emitter: Emitter = Arc::new(|_, _| {});
        let mut id_tx = None;
        handle_event(payload, &ws_tx, &tx, &emitter, &mut id_tx).await;
        let mut out = vec![];
        while let Ok(n) = rx.try_recv() {
            out.push((n.method, n.params));
        }
        out
    }

    #[tokio::test]
    async fn transcription_delta_maps_to_user_delta() {
        let out = map_event(
            r#"{"type":"conversation.item.input_audio_transcription.delta","delta":"안녕"}"#,
        )
        .await;
        let (method, params) = out.first().expect("a mapped event");
        assert_eq!(method, "thread/realtime/transcript/delta");
        assert_eq!(params["delta"], "안녕");
        assert_eq!(params["role"], "user");
    }

    #[tokio::test]
    async fn transcription_completed_maps_to_done() {
        let out = map_event(
            r#"{"type":"conversation.item.input_audio_transcription.completed","transcript":"hello world"}"#,
        )
        .await;
        let (method, params) = out.first().expect("a mapped event");
        assert_eq!(method, "thread/realtime/transcript/done");
        assert_eq!(params["text"], "hello world");
    }

    #[tokio::test]
    async fn error_maps_to_realtime_error() {
        let out = map_event(r#"{"type":"error","error":{"message":"boom"}}"#).await;
        let (method, params) = out.first().expect("a mapped event");
        assert_eq!(method, "thread/realtime/error");
        assert_eq!(params["message"], "boom");
    }

    #[tokio::test]
    async fn unrelated_events_are_ignored() {
        for payload in [
            r#"{"type":"response.text.delta","delta":"assistant"}"#,
            r#"{"type":"input_audio_buffer.speech_started"}"#,
        ] {
            assert!(map_event(payload).await.is_empty(), "payload: {payload}");
        }
    }
}
