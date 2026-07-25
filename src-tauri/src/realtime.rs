//! Direct OAuth WebRTC realtime session — no codex app-server, no API key.
//!
//! Protocol (reverse-engineered 2026-07-25, see REVERSE-ENGINEERING.md §9):
//!
//! ```text
//! POST https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas
//!   Authorization: Bearer <codex CLI OAuth access_token>
//!   ChatGPT-Account-ID: <account_id>
//!   originator: codex_chatgpt_desktop
//!   openai-alpha: quicksilver=v2
//!   { "sdp": "<offer>", "session": {
//!       "model": "gpt-live-1-boulder-alpha",
//!       "instructions": "...",
//!       "audio": { "output": { "voice": "cove" } },
//!       "delegation": { "type": "client" } } }
//! → 201 + SDP answer + Location: /v1/realtime/calls/rtc_<callId>
//! ```
//!
//! Audio flows over the WebRTC media track (Opus 48 kHz). Events — including
//! the user's own transcript (`input_transcript.added`, `turn.done`) — flow
//! over the `oai-events` data channel. The sideband WebSocket is a mirror of
//! the same events and is not needed.

use crate::error::RpcError;
use bytes::Bytes;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, Mutex};
use tracing::{info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

pub type Emitter = Arc<dyn Fn(&str, Value) + Send + Sync>;

const CALLS_URL: &str =
    "https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas";
const MODEL: &str = "gpt-live-1-boulder-alpha";

/// The session doubles as a voice agent; for dictation we want a stenographer,
/// not a conversation partner. Anything it still says is discarded client-side
/// (output events are never forwarded). The script clause is load-bearing:
/// without it the model sometimes transliterates English speech into Hangul
/// on Korean accounts.
fn instructions() -> String {
    let base = "You are a speech-to-text transcription engine. \
        Transcribe the user's speech exactly as heard. \
        Always keep the original language and script — English speech in Latin \
        script, Korean speech in Hangul; never transliterate. \
        Do not respond, do not speak, do not comment, do not translate.";
    match crate::config::get().language {
        crate::config::Language::Auto => base.to_string(),
        crate::config::Language::Korean => {
            format!("{base} The user usually speaks Korean.")
        }
        crate::config::Language::English => {
            format!("{base} The user usually speaks English.")
        }
    }
}

/// Opus frames are 20 ms of 48 kHz mono audio.
pub const OPUS_FRAME_SAMPLES: usize = 960;
const OPUS_SAMPLE_RATE: u32 = 48_000;

const ICE_GATHER_TIMEOUT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Recreate the call this long before the server-side expiry.
const EXPIRY_MARGIN_SECS: u64 = 60;

/// Sessions go stale server-side after a few minutes without audio: events
/// just stop arriving (verified live — 6+ minute idle sessions produce zero
/// transcription despite healthy audio). The call is therefore only reused
/// while young; older sessions are recreated on the next dictation.
fn stale_after() -> Duration {
    std::env::var("CODEX_MIC_SESSION_STALE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(90))
}

/// Mirror of the old app-server notification shape so the dictate pipeline
/// stays transport-agnostic.
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

/// Safe-ish wrapper around the libopus encoder handle. The handle is only
/// touched under a Mutex, so the raw pointer never crosses threads unsafely.
struct OpusEncoder(*mut audiopus_sys::OpusEncoder);
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    fn new() -> Result<Self, RpcError> {
        let mut error: i32 = 0;
        let handle = unsafe {
            audiopus_sys::opus_encoder_create(
                OPUS_SAMPLE_RATE as i32,
                1,
                audiopus_sys::OPUS_APPLICATION_VOIP,
                &mut error,
            )
        };
        if error != audiopus_sys::OPUS_OK || handle.is_null() {
            return Err(RpcError::Spawn(format!("opus_encoder_create failed: {error}")));
        }
        Ok(Self(handle))
    }

    fn encode(&mut self, frame: &[i16]) -> Result<Vec<u8>, RpcError> {
        let mut out = vec![0u8; 1500];
        let written = unsafe {
            audiopus_sys::opus_encode(
                self.0,
                frame.as_ptr(),
                frame.len() as i32,
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        if written < 0 {
            return Err(RpcError::Spawn(format!("opus_encode failed: {written}")));
        }
        out.truncate(written as usize);
        Ok(out)
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { audiopus_sys::opus_encoder_destroy(self.0) }
    }
}

pub struct RealtimeSession {
    pc: webrtc::peer_connection::RTCPeerConnection,
    track: Arc<TrackLocalStaticSample>,
    events: broadcast::Sender<Notification>,
    encoder: Mutex<OpusEncoder>,
    /// Leftover samples smaller than one Opus frame, carried to the next pump.
    pending: Mutex<Vec<i16>>,
    expires_at: Arc<Mutex<Option<u64>>>,
    created_at: std::time::Instant,
}

impl RealtimeSession {
    pub async fn connect(emitter: Emitter) -> Result<(Self, ConnectInfo), RpcError> {
        // webrtc's DTLS and reqwest's rustls pull in different crypto
        // providers (ring vs aws-lc-rs); rustls refuses to guess which one to
        // use process-wide, so pick ring explicitly. Idempotent — installing
        // twice just returns an error we ignore.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let tokens = crate::auth::ensure_fresh_token()
            .await
            .map_err(RpcError::Disconnected)?;

        // PCMU was offered too during probing; its server-side path returned
        // unintelligible audio, so Opus is the only codec we advertise.
        let mut media_engine = MediaEngine::default();
        media_engine.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: OPUS_SAMPLE_RATE,
                    channels: 2, // SDP convention: opus is always signaled stereo
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )
        .map_err(|e| RpcError::Spawn(format!("register opus codec: {e}")))?;

        // mDNS candidates (hostnames ending in .local) are useless to a remote
        // peer; expose real host IPs like the werift probe that connected.
        let mut setting_engine = SettingEngine::default();
        setting_engine.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);

        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .map_err(|e| RpcError::Spawn(format!("interceptors: {e}")))?;
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .build();
        let pc = api
            .new_peer_connection(RTCConfiguration::default())
            .await
            .map_err(|e| RpcError::Spawn(format!("peer connection: {e}")))?;

        let (events, _) = broadcast::channel::<Notification>(256);
        let expires_at: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

        let dc = pc
            .create_data_channel("oai-events", None)
            .await
            .map_err(|e| RpcError::Spawn(format!("data channel: {e}")))?;
        {
            let events = events.clone();
            let emitter = emitter.clone();
            let expires_at = expires_at.clone();
            dc.on_message(Box::new(move |msg: DataChannelMessage| {
                let events = events.clone();
                let emitter = emitter.clone();
                let expires_at = expires_at.clone();
                Box::pin(async move {
                    handle_event(&msg, &events, &emitter, expires_at).await;
                })
            }));
        }

        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: OPUS_SAMPLE_RATE,
                channels: 2,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            "audio".to_owned(),
            "codex-mic".to_owned(),
        ));
        pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .map_err(|e| RpcError::Spawn(format!("add audio track: {e}")))?;

        // Signal connection completion before the answer arrives, or the state
        // change can fire before we start waiting for it.
        let (state_tx, state_rx) = tokio::sync::oneshot::channel::<RTCPeerConnectionState>();
        let state_tx = Arc::new(std::sync::Mutex::new(Some(state_tx)));
        pc.on_peer_connection_state_change(Box::new(move |state| {
            let state_tx = state_tx.clone();
            Box::pin(async move {
                if matches!(
                    state,
                    RTCPeerConnectionState::Connected
                        | RTCPeerConnectionState::Failed
                        | RTCPeerConnectionState::Disconnected
                        | RTCPeerConnectionState::Closed
                ) {
                    if let Some(tx) = state_tx.lock().unwrap().take() {
                        let _ = tx.send(state);
                    }
                }
            })
        }));

        let offer = pc
            .create_offer(None)
            .await
            .map_err(|e| RpcError::Spawn(format!("create offer: {e}")))?;
        let mut gather = pc.gathering_complete_promise().await;
        pc.set_local_description(offer)
            .await
            .map_err(|e| RpcError::Spawn(format!("set local description: {e}")))?;
        // The promise channel closes when gathering completes.
        let _ = tokio::time::timeout(ICE_GATHER_TIMEOUT, gather.recv()).await;
        let local = pc
            .local_description()
            .await
            .ok_or_else(|| RpcError::Spawn("no local description after gather".into()))?;

        let (answer_sdp, call_id) =
            call_create(&local.sdp, &tokens.access_token, &tokens.account_id).await?;

        pc.set_remote_description(RTCSessionDescription::answer(answer_sdp).map_err(
            |e| RpcError::Spawn(format!("parse answer sdp: {e}")),
        )?)
        .await
        .map_err(|e| RpcError::Spawn(format!("set remote description: {e}")))?;

        match tokio::time::timeout(CONNECT_TIMEOUT, state_rx).await {
            Ok(Ok(RTCPeerConnectionState::Connected)) => {}
            Ok(Ok(other)) => {
                return Err(RpcError::Disconnected(format!(
                    "peer connection state {other:?} before connect"
                )))
            }
            Ok(Err(_)) => {
                return Err(RpcError::Disconnected(
                    "connection state channel closed early".into(),
                ))
            }
            Err(_) => return Err(RpcError::Timeout(CONNECT_TIMEOUT)),
        }
        info!(call_id = %call_id, "realtime session connected");

        let info = ConnectInfo {
            call_id: call_id.clone(),
            account_id: tokens.account_id,
            auth_mode: "chatgpt-oauth".to_string(),
        };
        let session = Self {
            pc,
            track,
            events,
            encoder: Mutex::new(OpusEncoder::new()?),
            pending: Mutex::new(Vec::new()),
            expires_at,
            created_at: std::time::Instant::now(),
        };
        Ok((session, info))
    }

    /// False when the peer connection died, the call is about to expire, or
    /// the session is old enough that the server may have stopped listening —
    /// the next hotkey press should reconnect instead of streaming into a void.
    pub async fn is_usable(&self) -> bool {
        if self.pc.connection_state() != RTCPeerConnectionState::Connected {
            return false;
        }
        if self.created_at.elapsed() >= stale_after() {
            return false;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        match *self.expires_at.lock().await {
            Some(exp) => exp > now + EXPIRY_MARGIN_SECS,
            None => true,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.events.subscribe()
    }

    /// Push captured 48 kHz mono PCM16 (little-endian bytes) into the call.
    /// Samples accumulate until a full 20 ms Opus frame is available.
    pub async fn append_pcm(&self, pcm: &[u8]) -> Result<(), RpcError> {
        let mut pending = self.pending.lock().await;
        pending.reserve(pcm.len() / 2);
        for chunk in pcm.chunks_exact(2) {
            pending.push(i16::from_le_bytes([chunk[0], chunk[1]]));
        }
        while pending.len() >= OPUS_FRAME_SAMPLES {
            let frame: Vec<i16> = pending.drain(..OPUS_FRAME_SAMPLES).collect();
            let encoded = self.encoder.lock().await.encode(&frame)?;
            self.track
                .write_sample(&Sample {
                    data: Bytes::from(encoded),
                    timestamp: SystemTime::now(),
                    duration: Duration::from_millis(20),
                    packet_timestamp: 0,
                    prev_dropped_packets: 0,
                    prev_padding_packets: 0,
                })
                .await
                .map_err(|e| RpcError::Disconnected(format!("write sample: {e}")))?;
        }
        Ok(())
    }

    pub async fn disconnect(&self) {
        if let Err(e) = self.pc.close().await {
            warn!(error = %e, "peer connection close failed");
        }
    }
}

/// The call-create HTTP exchange. Returns `(answer_sdp, call_id)`.
async fn call_create(
    offer_sdp: &str,
    access_token: &str,
    account_id: &str,
) -> Result<(String, String), RpcError> {
    let client = reqwest::Client::new();
    let res = client
        .post(CALLS_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("ChatGPT-Account-ID", account_id)
        .header("originator", "codex_chatgpt_desktop")
        .header("openai-alpha", "quicksilver=v2")
        .json(&json!({
            "sdp": offer_sdp,
            "session": {
                "model": MODEL,
                "instructions": instructions(),
                "audio": { "output": { "voice": "cove" } },
                "delegation": { "type": "client" },
            },
        }))
        .send()
        .await
        .map_err(|e| RpcError::Disconnected(format!("call-create request: {e}")))?;

    let status = res.status();
    if !status.is_success() {
        let body = res.text().await.unwrap_or_default();
        return Err(RpcError::Disconnected(format!(
            "call-create failed ({status}): {}",
            body.chars().take(300).collect::<String>()
        )));
    }
    let call_id = res
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .and_then(|loc| loc.rsplit('/').next().map(str::to_string))
        .unwrap_or_default();
    let body = res
        .text()
        .await
        .map_err(|e| RpcError::Disconnected(format!("read answer: {e}")))?;
    if !body.starts_with("v=0") {
        return Err(RpcError::Disconnected(format!(
            "call-create returned no SDP: {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    Ok((body, call_id))
}

/// Map a frameless-bidi event from the data channel onto the internal
/// notification contract the dictate pipeline understands.
async fn handle_event(
    msg: &DataChannelMessage,
    events: &broadcast::Sender<Notification>,
    emitter: &Emitter,
    expires_at: Arc<Mutex<Option<u64>>>,
) {
    let Ok(text) = String::from_utf8(msg.data.to_vec()) else { return };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return };
    let Some(kind) = v.get("type").and_then(|t| t.as_str()) else { return };

    let mapped: Option<(String, Value, Option<&'static str>)> = match kind {
        // User speech transcript fragment — the dictation payload.
        "input_transcript.added" => v
            .pointer("/item/text")
            .and_then(|t| t.as_str())
            .map(|delta| {
                (
                    "thread/realtime/transcript/delta".to_string(),
                    json!({ "delta": delta, "role": "user" }),
                    Some("realtime://transcript-delta"),
                )
            }),
        // End of a user turn with the authoritative full transcript.
        "turn.done" => match v.pointer("/turn/role").and_then(|r| r.as_str()) {
            Some("user") => v
                .pointer("/turn/transcript")
                .and_then(|t| t.as_str())
                .map(|text| {
                    (
                        "thread/realtime/transcript/done".to_string(),
                        json!({ "text": text }),
                        Some("realtime://transcript-done"),
                    )
                }),
            _ => None,
        },
        "session.started" | "session.updated" => Some((
            "thread/realtime/started".to_string(),
            v.clone(),
            Some("realtime://started"),
        )),
        "error" => Some((
            "thread/realtime/error".to_string(),
            json!({ "message": v.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("unknown realtime error") }),
            Some("realtime://error"),
        )),
        _ => None,
    };

    if kind == "session.started" {
        if let Some(exp) = v.pointer("/session/expires_at").and_then(|e| e.as_u64()) {
            *expires_at.lock().await = Some(exp);
        }
        info!("realtime session.started");
    }
    // Diagnostics: without these, a silent session (no deltas, no errors) is
    // indistinguishable from a working one — which is exactly the bug class
    // that took a live QA session to find.
    match kind {
        "turn.done" => {
            let role = v.pointer("/turn/role").and_then(|r| r.as_str()).unwrap_or("?");
            let len = v
                .pointer("/turn/transcript")
                .and_then(|t| t.as_str())
                .map(|t| t.chars().count())
                .unwrap_or(0);
            info!(role, len, "turn.done");
        }
        "input_transcript.added" => {
            let len = v
                .pointer("/item/text")
                .and_then(|t| t.as_str())
                .map(|t| t.chars().count())
                .unwrap_or(0);
            info!(len, "input_transcript delta");
        }
        "error" => {
            warn!(payload = %text, "realtime error event");
        }
        _ => {}
    }

    if let Some((method, params, frontend)) = mapped {
        let _ = events.send(Notification {
            method: method.to_string(),
            params: params.clone(),
        });
        if let Some(name) = frontend {
            emitter(name, params);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Feed a frameless event through the mapper and collect what comes out.
    async fn map_event(
        payload: &str,
    ) -> Vec<(String, Value)> {
        let (tx, mut rx) = broadcast::channel(8);
        let seen: Arc<StdMutex<Vec<(String, Value)>>> = Arc::new(StdMutex::new(vec![]));
        let sink = seen.clone();
        let emitter: Emitter = Arc::new(move |name, params| {
            sink.lock().unwrap().push((name.to_string(), params));
        });
        let expires = Arc::new(Mutex::new(None));
        let msg = DataChannelMessage {
            data: Bytes::from(payload.to_string()),
            is_string: true,
        };
        handle_event(&msg, &tx, &emitter, expires).await;
        let mut out = vec![];
        while let Ok(n) = rx.try_recv() {
            out.push((n.method, n.params));
        }
        out.extend(seen.lock().unwrap().iter().map(|(n, p)| (n.clone(), p.clone())));
        out
    }

    #[tokio::test]
    async fn user_transcript_maps_to_delta() {
        let out = map_event(r#"{"type":"input_transcript.added","item":{"text":" hello"}}"#).await;
        let (method, params) = out.first().expect("a mapped event");
        assert_eq!(method, "thread/realtime/transcript/delta");
        assert_eq!(params["delta"], " hello");
        assert_eq!(params["role"], "user");
        assert!(out.iter().any(|(n, _)| n == "realtime://transcript-delta"));
    }

    #[tokio::test]
    async fn user_turn_done_maps_to_transcript_done() {
        let out = map_event(
            r#"{"type":"turn.done","turn":{"role":"user","transcript":"hello world"}}"#,
        )
        .await;
        let (method, params) = out.first().expect("a mapped event");
        assert_eq!(method, "thread/realtime/transcript/done");
        assert_eq!(params["text"], "hello world");
    }

    #[tokio::test]
    async fn assistant_turn_done_is_ignored() {
        let out = map_event(
            r#"{"type":"turn.done","turn":{"role":"assistant","transcript":"hi there"}}"#,
        )
        .await;
        assert!(out.is_empty(), "assistant output must never reach dictation: {out:?}");
    }

    #[tokio::test]
    async fn output_events_are_ignored() {
        for payload in [
            r#"{"type":"output_transcript.added","item":{"text":"assistant says"}}"#,
            r#"{"type":"output_audio.delta","audio":"AAAA"}"#,
            r#"{"type":"turn.created","turn":{"role":"assistant","transcript":"x"}}"#,
        ] {
            assert!(map_event(payload).await.is_empty(), "payload: {payload}");
        }
    }

    #[tokio::test]
    async fn error_maps_to_realtime_error() {
        let out = map_event(
            r#"{"type":"error","error":{"message":"session expired"}}"#,
        )
        .await;
        let (method, params) = out.first().expect("a mapped event");
        assert_eq!(method, "thread/realtime/error");
        assert_eq!(params["message"], "session expired");
    }

    #[tokio::test]
    async fn opus_encoder_produces_frames() {
        let mut enc = OpusEncoder::new().expect("encoder");
        let frame = vec![0i16; OPUS_FRAME_SAMPLES];
        let out = enc.encode(&frame).expect("encode");
        assert!(!out.is_empty(), "a 20ms silence frame must still encode");
        // DTX may shrink silence frames, but they stay well under the cap.
        assert!(out.len() < 1500);
    }
}
