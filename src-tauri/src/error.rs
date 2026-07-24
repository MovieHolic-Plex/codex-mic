use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("rpc error {code}: {message}")]
    Server {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("disconnected: {0}")]
    Disconnected(String),
    #[error("request timed out after {0:?}")]
    Timeout(std::time::Duration),
    #[error("failed to spawn codex: {0}")]
    Spawn(String),
}
