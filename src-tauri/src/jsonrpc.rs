use crate::error::RpcError;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{broadcast, Mutex, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

pub type Id = i64;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub method: String,
    pub id: Id,
    pub params: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

#[derive(Debug, Clone)]
pub enum Incoming {
    Response {
        id: Id,
        result: Option<Value>,
        error: Option<ErrorObject>,
    },
    Notification {
        method: String,
        params: Value,
    },
    ServerRequest {
        id: Id,
        method: String,
        params: Value,
    },
}

pub fn parse_incoming(value: Value) -> Option<Incoming> {
    let has_result = value.get("result").is_some();
    let has_error = value.get("error").is_some();
    let id = value.get("id").and_then(|v| v.as_i64());
    if let Some(id) = id {
        if has_result || has_error {
            let result = value.get("result").cloned();
            let error = value.get("error").map(|e| ErrorObject {
                code: e.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                message: e
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string(),
                data: e.get("data").cloned(),
            });
            return Some(Incoming::Response {
                id,
                result,
                error,
            });
        }
    }
    let method = value.get("method").and_then(|m| m.as_str())?.to_string();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    if let Some(id) = id {
        Some(Incoming::ServerRequest {
            id,
            method,
            params,
        })
    } else {
        Some(Incoming::Notification { method, params })
    }
}

pub struct Client {
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<Id, oneshot::Sender<Result<Value, RpcError>>>>,
    child: Mutex<Option<Child>>,
    notifications: broadcast::Sender<Notification>,
}

impl Client {
    pub fn spawn(mut child: Child) -> Result<(Arc<Self>, JoinHandle<()>), RpcError> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RpcError::Spawn("codex stdin not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RpcError::Spawn("codex stdout not piped".into()))?;
        let (tx, _rx) = broadcast::channel::<Notification>(512);
        let client = Arc::new(Self {
            stdin: Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            child: Mutex::new(Some(child)),
            notifications: tx,
        });
        let c = client.clone();
        let handle = tokio::spawn(async move { c.read_loop(stdout).await });
        Ok((client, handle))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.notifications.subscribe()
    }

    async fn write_line(&self, line: &str) -> Result<(), RpcError> {
        let mut guard = self.stdin.lock().await;
        guard.write_all(line.as_bytes()).await?;
        guard.write_all(b"\n").await?;
        guard.flush().await?;
        Ok(())
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        let msg = serde_json::to_string(&Notification {
            method: method.to_string(),
            params,
        })?;
        debug!(%method, "notify ->");
        self.write_line(&msg).await
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let msg = serde_json::to_string(&Request {
            method: method.to_string(),
            id,
            params,
        })?;
        debug!(%method, id, "request ->");
        if let Err(e) = self.write_line(&msg).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                Err(RpcError::Disconnected(format!("request {method} dropped")))
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(RpcError::Timeout(REQUEST_TIMEOUT))
            }
        }
    }

    async fn read_loop(self: Arc<Self>, stdout: ChildStdout) {
        let mut reader = BufReader::new(stdout);
        let mut buf = String::new();
        loop {
            buf.clear();
            match reader.read_line(&mut buf).await {
                Ok(0) => {
                    debug!("codex stdout EOF");
                    self.fail_all("stdout closed".into()).await;
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "codex stdout read error");
                    self.fail_all(format!("read error: {e}")).await;
                    break;
                }
            }
            let line = buf.trim();
            if line.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    debug!(error = %e, "non-json line: {line}");
                    continue;
                }
            };
            match parse_incoming(value) {
                Some(Incoming::Response { id, result, error }) => {
                    if let Some(sender) = self.pending.lock().await.remove(&id) {
                        let res = match error {
                            Some(e) => Err(RpcError::Server {
                                code: e.code,
                                message: e.message,
                                data: e.data,
                            }),
                            None => Ok(result.unwrap_or(Value::Null)),
                        };
                        let _ = sender.send(res);
                    } else {
                        warn!(id, "response without pending request");
                    }
                }
                Some(Incoming::ServerRequest { id, method, params }) => {
                    debug!(%method, id, "server request <-");
                    let resp = serde_json::to_string(&json!({ "id": id, "result": {} }))
                        .unwrap_or_else(|_| format!("{{\"id\":{id},\"result\":{{}}}}"));
                    if let Err(e) = self.write_line(&resp).await {
                        warn!(error = %e, "failed to answer server request");
                    }
                    let _ = self.notifications.send(Notification { method, params });
                }
                Some(Incoming::Notification { method, params }) => {
                    debug!(%method, "<- notify");
                    let _ = self.notifications.send(Notification { method, params });
                }
                None => {
                    debug!("unclassifiable message: {line}");
                }
            }
        }
    }

    async fn fail_all(&self, reason: String) {
        let pending: Vec<(Id, oneshot::Sender<Result<Value, RpcError>>)> =
            self.pending.lock().await.drain().collect();
        for (_, tx) in pending {
            let _ = tx.send(Err(RpcError::Disconnected(reason.clone())));
        }
    }
    pub async fn kill(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_omits_jsonrpc_field() {
        let req = Request {
            method: "thread/realtime/listVoices".into(),
            id: 3,
            params: json!({ "threadId": "thr_1" }),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(!s.contains("jsonrpc"));
        assert_eq!(
            s,
            r#"{"method":"thread/realtime/listVoices","id":3,"params":{"threadId":"thr_1"}}"#
        );
    }

    #[test]
    fn notification_has_no_id() {
        let n = Notification {
            method: "initialized".into(),
            params: json!({}),
        };
        let s = serde_json::to_string(&n).unwrap();
        assert!(!s.contains("id"));
        assert_eq!(s, r#"{"method":"initialized","params":{}}"#);
    }

    #[test]
    fn parse_response_with_result() {
        let v = json!({ "id": 5, "result": { "voices": { "v1": [], "v2": [] } } });
        match parse_incoming(v).unwrap() {
            Incoming::Response { id, result, error } => {
                assert_eq!(id, 5);
                assert!(error.is_none());
                assert!(result.unwrap().get("voices").is_some());
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn parse_response_with_error() {
        let v = json!({ "id": 4, "error": { "code": -32600, "message": "nope" } });
        match parse_incoming(v).unwrap() {
            Incoming::Response { id, result, error } => {
                assert_eq!(id, 4);
                assert!(result.is_none());
                let e = error.unwrap();
                assert_eq!(e.code, -32600);
                assert_eq!(e.message, "nope");
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn parse_notification() {
        let v = json!({ "method": "thread/realtime/sdp", "params": { "sdp": "v=0" } });
        match parse_incoming(v).unwrap() {
            Incoming::Notification { method, params } => {
                assert_eq!(method, "thread/realtime/sdp");
                assert_eq!(params["sdp"], "v=0");
            }
            _ => panic!("expected Notification"),
        }
    }

    #[test]
    fn parse_server_request_has_id_and_method() {
        let v = json!({ "id": 9, "method": "tool/requestUserInput", "params": {} });
        match parse_incoming(v).unwrap() {
            Incoming::ServerRequest { id, method, .. } => {
                assert_eq!(id, 9);
                assert_eq!(method, "tool/requestUserInput");
            }
            _ => panic!("expected ServerRequest"),
        }
    }

    #[test]
    fn parse_empty_object_is_none() {
        assert!(parse_incoming(json!({})).is_none());
    }
}
