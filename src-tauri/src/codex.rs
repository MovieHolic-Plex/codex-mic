use crate::error::RpcError;
use crate::jsonrpc::{Client, Notification};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tracing::{info, warn};

pub type Emitter = Arc<dyn Fn(&str, Value) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectInfo {
    pub thread_id: String,
    pub user_agent: String,
    pub codex_home: String,
    pub auth_mode: String,
}

const DEFAULT_CODEX: &str = r"C:\Users\hyeon\.codex\packages\standalone\current\bin\codex.exe";

fn resolve_codex_bin() -> String {
    if let Ok(p) = std::env::var("CODEX_BIN") {
        return p;
    }
    if PathBuf::from(DEFAULT_CODEX).exists() {
        return DEFAULT_CODEX.to_string();
    }
    "codex".to_string()
}

pub struct CodexSession {
    client: Arc<Client>,
    thread_id: String,
    _reader: JoinHandle<()>,
    _forwarder: JoinHandle<()>,
}

impl CodexSession {
    #[allow(dead_code)]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub async fn connect(emitter: Emitter) -> Result<(Self, ConnectInfo), RpcError> {
        let program = resolve_codex_bin();
        let mut cmd = Command::new(&program);
        cmd.args([
            "app-server",
            "--listen",
            "stdio://",
            "--enable",
            "realtime_conversation",
        ]);
        cmd.arg("-c").arg(r#"suppress_unstable_features_warning=true"#);
        cmd.arg("-c").arg(
            r#"experimental_realtime_webrtc_call_base_url="https://api.openai.com/v1""#,
        );
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child: Child = cmd.spawn().map_err(|e| RpcError::Spawn(format!("{e}")))?;
        info!(program, "codex app-server spawned");

        let (client, reader) = Client::spawn(child)?;
        let rx = client.subscribe();
        let emitter_for_task = emitter.clone();
        let forwarder = tokio::spawn(async move {
            forward_loop(rx, emitter_for_task).await;
        });

        let init = client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "codex_mic",
                        "title": "Codex Mic",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": { "experimentalApi": true },
                }),
            )
            .await?;
        let user_agent = init
            .get("userAgent")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let codex_home = init
            .get("codexHome")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        client.notify("initialized", json!({})).await?;

        let start = client
            .request("thread/start", json!({ "ephemeral": true }))
            .await?;
        let thread_id = start
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(|i| i.as_str())
            .ok_or_else(|| RpcError::Spawn("thread/start returned no thread.id".into()))?
            .to_string();
        info!(thread_id = %thread_id, "ephemeral thread started");

        let auth_mode = "chatgpt".to_string();
        let info = ConnectInfo {
            thread_id: thread_id.clone(),
            user_agent,
            codex_home,
            auth_mode,
        };
        Ok((Self { client, thread_id, _reader: reader, _forwarder: forwarder }, info))
    }

    pub async fn realtime_start(&self) -> Result<(), RpcError> {
        let params = json!({
            "threadId": self.thread_id,
            "outputModality": "text",
            "clientManagedHandoffs": true,
        });
        let _ = self.client.request("thread/realtime/start", params).await?;
        Ok(())
    }

    pub async fn append_audio(&self, base64_pcm: String) -> Result<(), RpcError> {
        self.client
            .request(
                "thread/realtime/appendAudio",
                json!({ "threadId": self.thread_id, "audio": { "data": base64_pcm, "sampleRate": 24000, "numChannels": 1 } }),
            )
            .await?;
        Ok(())
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Notification> {
        self.client.subscribe()
    }

    pub async fn realtime_stop(&self) -> Result<(), RpcError> {
        self.client
            .request(
                "thread/realtime/stop",
                json!({ "threadId": self.thread_id }),
            )
            .await?;
        Ok(())
    }

    pub async fn disconnect(&self) {
        self.client.kill().await;
    }
}

fn map_event(method: &str) -> Option<&'static str> {
    match method {
        "thread/realtime/started" => Some("realtime://started"),
        "thread/realtime/transcript/delta" => Some("realtime://transcript-delta"),
        "thread/realtime/transcript/done" => Some("realtime://transcript-done"),
        "thread/realtime/error" => Some("realtime://error"),
        "thread/realtime/closed" => Some("realtime://closed"),
        "warning" => Some("codex://warning"),
        _ => None,
    }
}

async fn forward_loop(
    mut rx: tokio::sync::broadcast::Receiver<Notification>,
    emitter: Emitter,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(n) => {
                if let Some(event) = map_event(&n.method) {
                    emitter(event, n.params.clone());
                }
                emitter(
                    "codex://notification",
                    json!({ "method": n.method, "params": n.params }),
                );
            }
            Err(RecvError::Closed) => {
                info!("notification stream closed");
                break;
            }
            Err(RecvError::Lagged(k)) => {
                warn!(skipped = k, "notification stream lagged");
            }
        }
    }
}
