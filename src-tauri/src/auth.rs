//! Codex CLI OAuth (`~/.codex/auth.json`) loader and refresher.
//!
//! The desktop voice path authenticates with the ChatGPT OAuth access token
//! the Codex CLI stores after `codex login` — no API key, no attestation. The
//! access token is a JWT that expires in days; when it does we exchange the
//! refresh token ourselves (same request codex makes) and persist the result,
//! so the user never has to re-login just to dictate.

use base64::Engine;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Shared constant for the Codex OAuth client (mirrors codex
/// `login/src/auth/manager.rs::CLIENT_ID`).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Refresh a token this many seconds before it actually expires — a dictation
/// that starts with 30s of token life should not die mid-sentence.
const EXPIRY_MARGIN_SECS: u64 = 120;

#[derive(Debug, Clone)]
pub struct OAuthTokens {
    pub access_token: String,
    pub account_id: String,
}

fn auth_path() -> Result<PathBuf, String> {
    if let Ok(home) = std::env::var("CODEX_HOME") {
        let home = home.trim();
        if !home.is_empty() {
            return Ok(PathBuf::from(home).join("auth.json"));
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| "could not locate home directory".to_string())?;
    Ok(PathBuf::from(home).join(".codex").join("auth.json"))
}

/// True when an auth.json with an access token exists. Cheap and offline —
/// the pill shows the login hint from this, not from a network probe.
pub fn has_oauth() -> bool {
    let Ok(path) = auth_path() else { return false };
    let Ok(raw) = std::fs::read_to_string(path) else { return false };
    let Ok(doc) = serde_json::from_str::<Value>(&raw) else { return false };
    doc.pointer("/tokens/access_token")
        .and_then(|t| t.as_str())
        .is_some_and(|t| !t.is_empty())
}

fn jwt_exp(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let doc: Value = serde_json::from_slice(&bytes).ok()?;
    doc.get("exp")?.as_u64()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load the OAuth tokens, refreshing first when the access token is expired
/// (or nearly). Errors are phrased for the pill UI: they tell the user exactly
/// what to run.
pub async fn ensure_fresh_token() -> Result<OAuthTokens, String> {
    let path = auth_path()?;
    let raw = std::fs::read_to_string(&path).map_err(|_| {
        "no Codex login found — run `codex login` once, then restart codex-mic".to_string()
    })?;
    let mut doc: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;

    let access_token = doc
        .pointer("/tokens/access_token")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    let account_id = doc
        .pointer("/tokens/account_id")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    if access_token.is_empty() {
        return Err("auth.json has no access token — run `codex login` again".to_string());
    }

    let fresh = jwt_exp(&access_token)
        .map(|exp| exp > now_secs() + EXPIRY_MARGIN_SECS)
        .unwrap_or(false);
    if fresh {
        return Ok(OAuthTokens {
            access_token,
            account_id,
        });
    }

    refresh(&path, &mut doc).await
}

async fn refresh(path: &std::path::Path, doc: &mut Value) -> Result<OAuthTokens, String> {
    let refresh_token = doc
        .pointer("/tokens/refresh_token")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    if refresh_token.is_empty() {
        return Err("access token expired and no refresh token stored — run `codex login`".into());
    }

    tracing::info!("access token expired; refreshing via {TOKEN_URL}");
    let client = reqwest::Client::new();
    let res = client
        .post(TOKEN_URL)
        .json(&serde_json::json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .map_err(|e| format!("token refresh request failed: {e}"))?;
    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        return Err(format!(
            "token refresh failed ({status}): {} — run `codex login`",
            body.chars().take(200).collect::<String>()
        ));
    }
    let body: Value = res
        .json()
        .await
        .map_err(|e| format!("token refresh returned invalid JSON: {e}"))?;

    let new_access = body
        .get("access_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "token refresh response had no access_token".to_string())?
        .to_string();
    let tokens = doc
        .get_mut("tokens")
        .and_then(|t| t.as_object_mut())
        .ok_or_else(|| "auth.json has no tokens object".to_string())?;
    tokens.insert("access_token".into(), Value::String(new_access.clone()));
    if let Some(id_token) = body.get("id_token").and_then(|t| t.as_str()) {
        tokens.insert("id_token".into(), Value::String(id_token.to_string()));
    }
    if let Some(rt) = body.get("refresh_token").and_then(|t| t.as_str()) {
        tokens.insert("refresh_token".into(), Value::String(rt.to_string()));
    }
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    doc.as_object_mut().map(|d| {
        d.insert("last_refresh".into(), Value::String(now));
    });

    let serialized = serde_json::to_string_pretty(doc)
        .map_err(|e| format!("could not serialize refreshed auth.json: {e}"))?;
    std::fs::write(path, serialized)
        .map_err(|e| format!("could not write refreshed auth.json: {e}"))?;

    let account_id = doc
        .pointer("/tokens/account_id")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(OAuthTokens {
        access_token: new_access,
        account_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an unsigned JWT-shaped string with the given payload.
    fn fake_jwt(payload: &str) -> String {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
        format!("aaa.{b64}.ccc")
    }

    #[test]
    fn jwt_exp_reads_payload() {
        let tok = fake_jwt(r#"{"exp":2000000000,"sub":"x"}"#);
        assert_eq!(jwt_exp(&tok), Some(2_000_000_000));
    }

    #[test]
    fn jwt_exp_rejects_garbage() {
        assert_eq!(jwt_exp("not-a-jwt"), None);
        assert_eq!(jwt_exp("a.!!!.c"), None);
        assert_eq!(jwt_exp(""), None);
    }
}
