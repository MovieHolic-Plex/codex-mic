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

const REALTIME_WS_BASE: &str = "wss://api.openai.com/v1/realtime?model=";
/// The realtime session model — the WebSocket endpoint itself. It hosts the
/// connection and the transcription attachment; for dictation it never speaks.
const REALTIME_MODEL: &str = "gpt-realtime-2.1";

pub fn default_realtime_model() -> &'static str {
    REALTIME_MODEL
}

/// Session models this endpoint accepted, found by trying names and keeping
/// the ones that opened.
///
/// There is no discovery call for these. A bad *transcription* model name gets
/// an error that enumerates the alternatives; a bad session model just closes
/// the socket, and only `gpt-realtime-1.5` enumerated transcribers at all — the
/// newer sessions answer a bad name with an empty result. So this list is
/// necessarily "names someone thought to try", not "names that exist", and
/// `gpt-realtime-2` and `gpt-realtime-2.1` were both missed on the first pass
/// for exactly that reason. Probe before assuming something is unavailable.
///
/// Refused so far: `gpt-realtime-2.0`, `2.2`, `2.5`, `3`, `3.0`, `4`, `v2`,
/// `1.1`-`1.4`, `1.5.1`, `1.6`, `gpt-realtime-2-mini`,
/// `gpt-realtime-mini-2`, `gpt-realtime-mini-2.1`, every `*-preview` and every
/// dated variant tried besides `gpt-realtime-2025-08-28`,
/// `gpt-4o-realtime-preview`, `gpt-4o-mini-realtime-preview`, `gpt-audio`,
/// `gpt-4o-audio-preview`.
///
/// Measured across a grid on one recording, the session model made no
/// difference to the transcript — for a given transcription model every
/// session produced the same text within run-to-run noise. That is the
/// architecture showing through: this model hosts the socket and carries the
/// transcription attachment, and for dictation it is never asked to speak.
/// Pick on cost and connect latency, not on accuracy.
pub const REALTIME_MODELS: &[(&str, &str)] = &[
    ("gpt-realtime-2.1", "최신"),
    ("gpt-realtime-2.1-mini", "최신, 더 가벼움"),
    ("gpt-realtime-2", ""),
    ("gpt-realtime-1.5", "전사 목록을 알려주는 유일한 세션"),
    ("gpt-realtime", ""),
    ("gpt-realtime-mini", "더 가벼움"),
    ("gpt-realtime-2025-08-28", "gpt-realtime 고정 버전"),
];

/// Session model to connect with: env, else settings, else the default.
fn realtime_model() -> String {
    if let Ok(m) = std::env::var("CODEX_MIC_REALTIME_MODEL") {
        if !m.trim().is_empty() {
            return m.trim().to_string();
        }
    }
    let configured = crate::config::get().realtime_model;
    if !configured.trim().is_empty() {
        return configured.trim().to_string();
    }
    REALTIME_MODEL.to_string()
}

fn realtime_ws_url() -> String {
    format!("{REALTIME_WS_BASE}{}", realtime_model())
}
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

/// Transcription model for input audio.
///
/// The endpoint's supported set, as it reported it when asked for a bad name:
/// `whisper-1`, `gpt-realtime-whisper`, `gpt-4o-transcribe`,
/// `gpt-4o-mini-transcribe`, `gpt-4o-mini-transcribe-2025-03-20`,
/// `gpt-4o-mini-transcribe-2025-12-15`. (`gpt-4o-transcribe-diarize` exists but
/// this organization has no access.)
///
/// Chosen from daily use, which outweighs the fixtures below.
///
/// The endpoint's supported set, as it reported it when asked for a bad name:
/// `whisper-1`, `gpt-realtime-whisper`, `gpt-4o-transcribe`,
/// `gpt-4o-mini-transcribe`, `gpt-4o-mini-transcribe-2025-03-20`,
/// `gpt-4o-mini-transcribe-2025-12-15`. (`gpt-4o-transcribe-diarize` exists but
/// this organization has no access.)
///
/// On the two captured recordings available here, `gpt-4o-transcribe` scored
/// better — it read "테스트 해봤냐?" correctly where this model returned
/// "クエスト", and it kept sentence endings intact on a longer clip. Two
/// recordings is a small sample, and it disagreed with how the tool actually
/// behaves across a day of dictation, so the lived result wins. The fixture
/// results are recorded here rather than deleted: if transcripts come back in
/// the wrong script or empty, this is the first thing to suspect, and
/// `CODEX_MIC_TRANSCRIBE_MODEL=gpt-4o-transcribe` switches back without a
/// rebuild.
///
/// This model rejects the `prompt` field (see `supports_prompt`), so the
/// anti-transliteration bias is not sent with it. Pinning `language` in
/// settings is the remaining lever if the script comes out wrong.
const TRANSCRIPTION_MODEL: &str = "gpt-realtime-whisper";

/// Every model this endpoint accepted, newest measurement first, with what it
/// made of the same recording of "이 API endpoint를 production에 deploy하고
/// code review 요청해줘". Drives the settings dropdown, so the list users pick
/// from is the list that was actually tested rather than one from memory.
pub const TRANSCRIPTION_MODELS: &[(&str, &str)] = &[
    ("gpt-realtime-whisper", "권장 — prompt 미지원, 짧은 말에서 언어 오인 가능"),
    ("gpt-4o-transcribe", "고정 녹음 비교에서 최고점, 영어를 한글로 음차"),
    ("gpt-4o-mini-transcribe", "deploy를 '배포'로 번역함"),
    ("gpt-4o-mini-transcribe-2025-12-15", "조사 부정확"),
    ("gpt-4o-mini-transcribe-2025-03-20", "어미 부정확"),
    ("whisper-1", "한국어에서 문장 붕괴"),
];

/// The model used when settings leave the choice blank.
pub fn default_transcription_model() -> &'static str {
    TRANSCRIPTION_MODEL
}

/// The transcription model to ask for: the settings choice, else the built-in
/// default. `CODEX_MIC_TRANSCRIBE_MODEL` overrides both, which is how the
/// candidates get compared on identical audio without a rebuild.
fn transcription_model() -> String {
    if let Ok(m) = std::env::var("CODEX_MIC_TRANSCRIBE_MODEL") {
        if !m.trim().is_empty() {
            return m.trim().to_string();
        }
    }
    let configured = crate::config::get().transcribe_model;
    if !configured.trim().is_empty() {
        return configured.trim().to_string();
    }
    TRANSCRIPTION_MODEL.to_string()
}

/// Whether a model accepts the `prompt` biasing field.
///
/// The whisper-based realtime model rejects it outright — measured against the
/// live endpoint: `The 'prompt' parameter is not supported for this model`.
/// Sending it anyway makes `session.update` fail, and the session then runs
/// with no input transcription configured at all, so the dictation silently
/// produces nothing.
pub fn supports_prompt(model: &str) -> bool {
    model != "gpt-realtime-whisper"
}

/// Log transcript text, not just its length. Opt-in — this is the user's
/// speech. Pair with `CODEX_MIC_DEBUG_DUMP_WAV` to compare what we sent with
/// what came back.
fn debug_transcript() -> bool {
    std::env::var("CODEX_MIC_DEBUG_TRANSCRIPT").is_ok_and(|v| v != "0")
}

/// The hotkey defines the turn; server VAD is off.
///
/// Server VAD exists so a voice *agent* knows when to reply — it cuts a turn
/// after `silence_duration_ms` of quiet. Dictation already knows where the turn
/// ends: the key came up. Letting VAD guess split one utterance into fragments
/// that had to be stitched back together, transcribed worse for having lost
/// their context, and — the fatal part — left no way to know whether more
/// transcription was still coming after the key was released. Measured against
/// the live service with a four-sentence utterance:
///
/// ```text
/// server_vad:   committed 1 ms after key-up, last two sentences still in
///               flight and silently dropped
/// manual turns: committed 1270 ms after key-up, complete
/// ```
///
/// The old code hid this behind a flat 1200 ms sleep after the commit, which
/// was the only reason VAD mode produced whole sentences at all.
///
/// `CODEX_MIC_SERVER_VAD=1` restores the old behavior as an escape hatch.
pub fn manual_turns() -> bool {
    !std::env::var("CODEX_MIC_SERVER_VAD").is_ok_and(|v| v != "0")
}

/// Bias for the transcriber. The `gpt-4o-transcribe` family drifts into
/// transliterating English speech into the account locale's script without it
/// (measured: "더 퀵 브라운 폭스 점프스…"); with it, clean original-script text.
///
/// Only sent to models that accept it (see `supports_prompt`). The default
/// model does not — it was checked on the same English audio and returned
/// Latin script unprompted, so nothing is lost by omitting it there.
const TRANSCRIPTION_PROMPT: &str = "The user dictates text messages in English \
    and Korean for software development work. Transcribe verbatim in the \
    original language and script. Do not transliterate.";

/// The prompt actually sent. `CODEX_MIC_TRANSCRIBE_PROMPT` overrides it, which
/// is how wording is A/B'd against recorded speech instead of guessed at.
fn transcription_prompt() -> String {
    std::env::var("CODEX_MIC_TRANSCRIBE_PROMPT")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| TRANSCRIPTION_PROMPT.to_string())
}

/// What the session model is told when it is asked to write down the audio.
///
/// This is the answer to "why is there a transcription model at all?" — there
/// are two things in the session. `input_audio_transcription` is a separate
/// smaller model whose `prompt` is a vocabulary prime, not an instruction, and
/// no wording of it reliably stopped English technical terms being written in
/// Hangul: measured on real speech, "이 API endpoint를 production에 deploy하고"
/// came back as "이 API 엔드포인트를 프로덕션에 배포하고". Prompts that did fix
/// it started dropping plain-English sentences instead. The session model hears
/// the same audio and actually follows instructions, so it is simply told.
const DICTATION_INSTRUCTIONS: &str = "Write down exactly what the user said, word for word. \
    This is dictation: never answer, never comment, never summarise, never translate. Keep the \
    speaker's own language. The speaker is a software developer who mixes English technical terms \
    into Korean speech — spell those terms in English, never in Hangul. Punctuate naturally, with \
    sentence-ending marks. Output only the transcript.";

fn dictation_instructions() -> String {
    std::env::var("CODEX_MIC_RESPOND_PROMPT")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| DICTATION_INSTRUCTIONS.to_string())
}

/// Take the transcript from the session model instead of the transcription
/// side channel. Off by default, and it should stay off.
///
/// The session model follows instructions, which fixes the transliteration the
/// side channel cannot be talked out of — but it is a *generative* model, and
/// measured over one session it began inventing speech outright. Nine rounds,
/// three recordings cycled, the same mixed-language clip on rounds 1, 4 and 7:
///
/// ```text
/// round 1  "API 엔드포인트를 production에 deploy하고 code review 요청해줘."
/// round 4  "음, 이제 integration test를 돌려보고, CI pipeline 통과하면
///           staging에서 한 번 더 verification 해야 해."
/// round 7  (the same invention again)
/// ```
///
/// Nothing like that was said. Once the conversation carries a few turns the
/// model starts producing plausible developer speech instead of transcribing,
/// because every `response.create` appends to a conversation it reads in full.
/// The transcription side channel is structurally incapable of this: it is an
/// ASR model, it has no conversation, and it cannot decide to say something
/// else. A dictation tool types into whatever window has focus, so "sometimes
/// writes a sentence the user never spoke" is disqualifying — worse than any
/// spelling complaint.
///
/// `CODEX_MIC_MODEL_TRANSCRIBE=1` enables it anyway.
pub fn model_transcription() -> bool {
    std::env::var("CODEX_MIC_MODEL_TRANSCRIBE").is_ok_and(|v| v == "1")
}

/// Ask the session model to transcribe the audio it has already been sent.
pub async fn transcribe_committed_audio(
    session: &RealtimeSession,
    budget: Duration,
) -> Result<String, RpcError> {
    session.respond(&dictation_instructions(), budget).await
}

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

/// ISO-639-1 hint for the transcription model. Only pinned when the user
/// picks a language explicitly — Auto leaves it unset.
fn transcription_language() -> Option<&'static str> {
    // Probe hook: exercise the pinned-language path without writing to the
    // user's real config.
    match std::env::var("CODEX_MIC_TRANSCRIBE_LANG").as_deref() {
        Ok("ko") => return Some("ko"),
        Ok("en") => return Some("en"),
        _ => {}
    }
    match crate::config::get().language {
        crate::config::Language::Auto => None,
        crate::config::Language::Korean => Some("ko"),
        crate::config::Language::English => Some("en"),
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

/// A failed connect, plus whether it failed because the server refused our
/// credentials — the one case worth retrying with fresh ones.
struct ConnectFailure {
    error: RpcError,
    auth_rejected: bool,
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
    /// Open a session, refreshing the credentials once if the server rejects
    /// them.
    ///
    /// A token can be invalidated server-side while its `exp` claim still says
    /// it is good for days, and the refresh path used to trigger on local
    /// expiry alone — so the app stayed hard-stuck until the dead JWT's clock
    /// ran out. One forced refresh turns that into a hiccup.
    pub async fn connect(emitter: Emitter) -> Result<(Self, ConnectInfo), RpcError> {
        match Self::connect_once(emitter.clone(), false).await {
            Err(ConnectFailure { error, auth_rejected: true }) => {
                warn!(%error, "credentials rejected; forcing a token refresh and retrying");
                Self::connect_once(emitter, true)
                    .await
                    .map_err(|f| f.error)
            }
            other => other.map_err(|f| f.error),
        }
    }

    async fn connect_once(
        emitter: Emitter,
        force_refresh: bool,
    ) -> Result<(Self, ConnectInfo), ConnectFailure> {
        // Record the server's first error so the caller can tell "your token is
        // dead" from any other connect failure. On the first attempt it is
        // recorded but not forwarded: reporting a failure we are about to
        // recover from would just flash an alarming message on the pill.
        let seen_error: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
        let emitter = {
            let sink = seen_error.clone();
            let inner = emitter;
            let forward = force_refresh;
            let wrapped: Emitter = Arc::new(move |name: &str, payload: Value| {
                if name == "realtime://error" {
                    if let Some(m) = payload.get("message").and_then(|m| m.as_str()) {
                        let mut g = sink
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if g.is_none() {
                            *g = Some(m.to_string());
                        }
                    }
                    if !forward {
                        return;
                    }
                }
                inner(name, payload);
            });
            wrapped
        };
        let fail = move |error: RpcError| {
            let recorded = seen_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let auth_rejected = !force_refresh
                && recorded.as_deref().is_some_and(crate::auth::is_auth_rejection);
            ConnectFailure { error, auth_rejected }
        };

        let token_result = if force_refresh {
            crate::auth::force_refresh().await
        } else {
            crate::auth::ensure_fresh_token().await
        };
        let tokens = match token_result {
            Ok(t) => t,
            Err(e) => return Err(fail(RpcError::Disconnected(e))),
        };

        let mut request = realtime_ws_url()
            .into_client_request()
            .map_err(|e| fail(RpcError::Spawn(format!("ws request: {e}"))))?;
        let headers = request.headers_mut();
        headers.insert(
            "Authorization",
            format!("Bearer {}", tokens.access_token)
                .parse()
                .map_err(|e| fail(RpcError::Spawn(format!("bad auth header: {e}"))))?,
        );
        headers.insert(
            "ChatGPT-Account-ID",
            tokens
                .account_id
                .parse()
                .map_err(|e| fail(RpcError::Spawn(format!("bad account header: {e}"))))?,
        );
        headers.insert(
            "originator",
            "codex_chatgpt_desktop"
                .parse()
                .map_err(|e| fail(RpcError::Spawn(format!("bad originator header: {e}"))))?,
        );

        let (ws_stream, _resp) =
            match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
                .await
            {
                Err(_) => return Err(fail(RpcError::Timeout(CONNECT_TIMEOUT))),
                Ok(Err(e)) => {
                    return Err(fail(RpcError::Disconnected(format!("ws connect: {e}"))))
                }
                Ok(Ok(v)) => v,
            };

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

        // The session id arrives in session.created. A rejected token shows up
        // right here: the server emits an `error` and closes without ever
        // sending it.
        let call_id = match tokio::time::timeout(CONNECT_TIMEOUT, session_id_rx).await {
            Err(_) => return Err(fail(RpcError::Timeout(CONNECT_TIMEOUT))),
            Ok(Err(_)) => {
                return Err(fail(RpcError::Disconnected(
                    "ws closed before session.created".into(),
                )))
            }
            Ok(Ok(id)) => id,
        };
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
        // Mirror exactly what goes on the wire, tail silence included.
        crate::audio::dump::write(pcm);
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

    /// Run a text-in / text-out turn on the session that is already open.
    ///
    /// The realtime session is a conversational model that we have merely told
    /// to keep quiet, so a second pass over the transcript needs no new
    /// connection, no second credential and no other API — two messages on the
    /// same socket. `instructions` is per-response, so the session's standing
    /// "transcribe only, never reply" orders stay intact for dictation.
    pub async fn refine_text(
        &self,
        text: &str,
        instructions: &str,
        budget: Duration,
    ) -> Result<String, RpcError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(RpcError::Disconnected("session closed".into()));
        }
        // Subscribe before sending, or a fast reply races the subscription.
        let mut rx = self.events.subscribe();
        // The task goes in the message, not only in `instructions`. The
        // session's standing orders are "transcribe only, never reply", and
        // measured against the live service those beat a per-response
        // `instructions` field outright — the model echoed its input back
        // unchanged. Stated as the user's own request, it complies.
        let item = json!({
            "type": "conversation.item.create",
            "item": {
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": format!("{instructions}\n\n---\n{text}"),
                }],
            }
        });
        self.ws_tx
            .send(item.to_string())
            .await
            .map_err(|_| RpcError::Disconnected("ws writer gone".into()))?;
        let create = json!({
            "type": "response.create",
            "response": {
                "instructions": instructions,
                "output_modalities": ["text"],
            }
        });
        self.ws_tx
            .send(create.to_string())
            .await
            .map_err(|_| RpcError::Disconnected("ws writer gone".into()))?;

        self.collect_response(&mut rx, budget).await
    }

    /// Ask the realtime model itself to write down the audio already in the
    /// buffer, bypassing the input-transcription side channel entirely.
    ///
    /// The session model hears the audio directly; `input_audio_transcription`
    /// is a separate, smaller model running alongside it. The difference that
    /// matters here is that the session model follows instructions, while the
    /// transcription model's `prompt` is only a vocabulary prime.
    pub async fn respond(
        &self,
        instructions: &str,
        budget: Duration,
    ) -> Result<String, RpcError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(RpcError::Disconnected("session closed".into()));
        }
        let mut rx = self.events.subscribe();
        let create = json!({
            "type": "response.create",
            "response": {
                "instructions": instructions,
                "output_modalities": ["text"],
            }
        });
        self.ws_tx
            .send(create.to_string())
            .await
            .map_err(|_| RpcError::Disconnected("ws writer gone".into()))?;
        self.collect_response(&mut rx, budget).await
    }

    async fn collect_response(
        &self,
        rx: &mut broadcast::Receiver<Notification>,
        budget: Duration,
    ) -> Result<String, RpcError> {
        let deadline = tokio::time::Instant::now() + budget;
        let mut out = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(RpcError::Timeout(budget));
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Err(_) => return Err(RpcError::Timeout(budget)),
                Ok(Err(_)) => return Err(RpcError::Disconnected("event stream ended".into())),
                Ok(Ok(n)) if n.method == "thread/realtime/response/delta" => {
                    if let Some(d) = n.params.get("delta").and_then(|d| d.as_str()) {
                        out.push_str(d);
                    }
                }
                Ok(Ok(n)) if n.method == "thread/realtime/response/done" => {
                    if out.trim().is_empty() {
                        if let Some(t) = n
                            .params
                            .pointer("/response/output/0/content/0/text")
                            .and_then(|t| t.as_str())
                        {
                            out.push_str(t);
                        }
                    }
                    return Ok(out);
                }
                Ok(Ok(n)) if n.method == "thread/realtime/error" => {
                    let msg = n
                        .params
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("response failed");
                    return Err(RpcError::Disconnected(msg.to_string()));
                }
                Ok(Ok(_)) => {}
            }
        }
    }

    /// Append digital silence to the input buffer.
    ///
    /// Padding an utterance on both sides gives the transcriber room: audio
    /// that starts or stops the instant speech does loses the first and last
    /// syllable to the model's own windowing.
    pub async fn append_silence(&self, silence: Duration) -> Result<(), RpcError> {
        let samples = (SESSION_SAMPLE_RATE as u64 * silence.as_millis() as u64 / 1000) as usize;
        self.append_pcm(&vec![0u8; samples * 2]).await
    }

    /// Close the current turn: pad the end, commit the buffer, and let the
    /// caller wait for the transcript. Without the padding the last syllable of
    /// every dictation is captured and then thrown away.
    pub async fn flush_tail(&self, silence: Duration) -> Result<(), RpcError> {
        self.append_silence(silence).await?;
        let _ = self
            .ws_tx
            .send(json!({"type": "input_audio_buffer.commit"}).to_string())
            .await;
        // No sleep here. Waiting for the transcript is `DictateState::finish`'s
        // job and it does it properly — it stops as soon as the text has
        // actually settled. A flat 1200 ms on top of that was pure latency on
        // every single dictation, paid whether the transcript took 200 ms or
        // was already in hand.
        Ok(())
    }

    pub async fn disconnect(&self) {
        self.alive.store(false, Ordering::SeqCst);
        // No close message exists for this session type (session.close is
        // translation-only and errors out). Abort the io task; the socket
        // drops with it and the server expires the session on its own.
        if let Some(task) = self.reader_task.lock().await.take() {
            task.abort();
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
        // Only surface a *failure* when the session died on its own. A
        // deliberate disconnect() (stale-session replacement, settings change)
        // must not trip the watchdog into aborting the next dictation.
        if alive.swap(false, Ordering::SeqCst) {
            let _ = events.send(Notification {
                method: "thread/realtime/error".to_string(),
                params: json!({ "message": "realtime session closed unexpectedly" }),
            });
        }
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
            let model = transcription_model();
            let mut transcription = json!({ "model": model });
            if supports_prompt(&model) {
                transcription["prompt"] = json!(transcription_prompt());
            }
            if let Some(lang) = transcription_language() {
                transcription["language"] = json!(lang);
            }
            let update = json!({
                "type": "session.update",
                "session": {
                    "type": "realtime",
                    "instructions": instructions(),
                    "output_modalities": ["text"],
                    "audio": {
                        "input": {
                            "format": { "type": "audio/pcm", "rate": SESSION_SAMPLE_RATE },
                            "turn_detection": if manual_turns() {
                                Value::Null
                            } else {
                                json!({
                                    "type": "server_vad",
                                    "threshold": 0.5,
                                    "prefix_padding_ms": 300,
                                    "silence_duration_ms": 500
                                })
                            },
                            "noise_reduction": { "type": "near_field" },
                            "transcription": transcription,
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
            // The text is the user's speech, so it is length-only by default.
            // CODEX_MIC_DEBUG_TRANSCRIPT=1 opts into logging it, which is what
            // makes "the words are wrong" diagnosable at all.
            if debug_transcript() {
                info!(turn = %transcript, "input transcription completed");
            } else {
                info!(len = transcript.chars().count(), "input transcription completed");
            }
            let params = json!({ "text": transcript });
            let _ = events.send(Notification {
                method: "thread/realtime/transcript/done".to_string(),
                params: params.clone(),
            });
            emitter("realtime://transcript-done", params);
        }
        // Assistant text. Not part of dictation — the session is told to stay
        // silent — but it is what a deliberate `response.create` comes back as.
        "response.output_text.delta" | "response.text.delta" => {
            if let Some(delta) = v.get("delta").and_then(|d| d.as_str()) {
                let _ = events.send(Notification {
                    method: "thread/realtime/response/delta".to_string(),
                    params: json!({ "delta": delta }),
                });
            }
        }
        // Only `response.done` terminates. `response.output_text.done` arrives
        // first and carries the same text; mapping both to one notification
        // emitted two "done"s per response, and the stray second one was picked
        // up by the *next* refine call, which then returned the previous
        // answer.
        "response.done" => {
            let _ = events.send(Notification {
                method: "thread/realtime/response/done".to_string(),
                params: v.clone(),
            });
        }
        "input_audio_buffer.speech_started" => {
            // Also a commit signal, not just a UI cue: it means VAD has opened
            // a turn whose transcript has not arrived yet, so a commit waiting
            // on "have all turns settled?" must keep waiting.
            let _ = events.send(Notification {
                method: "thread/realtime/speech/started".to_string(),
                params: v.clone(),
            });
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

    /// Sending `prompt` to a model that rejects it makes `session.update`
    /// fail, and the session then runs with no input transcription at all — the
    /// dictation produces nothing and nothing says why. Measured error:
    /// `The 'prompt' parameter is not supported for this model`.
    #[test]
    fn prompt_is_withheld_from_the_model_that_rejects_it() {
        assert!(!supports_prompt("gpt-realtime-whisper"));
        for m in [
            "gpt-4o-transcribe",
            "gpt-4o-mini-transcribe",
            "gpt-4o-mini-transcribe-2025-12-15",
            "whisper-1",
        ] {
            assert!(supports_prompt(m), "{m} accepts prompt");
        }
    }

    /// Env beats settings beats the built-in default, and blank means "unset"
    /// at every level — an empty settings field must fall through to the
    /// default rather than asking the server for a model named "".
    #[test]
    fn transcription_model_precedence() {
        // Serialised against other env-touching tests by running the whole
        // precedence chain in one test rather than several.
        std::env::remove_var("CODEX_MIC_TRANSCRIBE_MODEL");
        assert_eq!(transcription_model(), TRANSCRIPTION_MODEL);

        std::env::set_var("CODEX_MIC_TRANSCRIBE_MODEL", "   ");
        assert_eq!(transcription_model(), TRANSCRIPTION_MODEL, "blank env is unset");

        std::env::set_var("CODEX_MIC_TRANSCRIBE_MODEL", " whisper-1 ");
        assert_eq!(transcription_model(), "whisper-1", "env wins and is trimmed");

        std::env::remove_var("CODEX_MIC_TRANSCRIBE_MODEL");
        assert_eq!(transcription_model(), TRANSCRIPTION_MODEL);
    }

    /// The dictation transcript must come from the ASR side channel, which
    /// cannot invent speech. The session model can, and did — see
    /// `model_transcription`.
    #[test]
    fn the_generative_model_is_not_the_default_transcriber() {
        assert!(
            !model_transcription(),
            "a generative model must never be the default source of dictated text"
        );
    }

    /// The default must stay inside the set the endpoint actually accepts.
    #[test]
    fn default_transcription_model_is_a_supported_one() {
        const SUPPORTED: &[&str] = &[
            "whisper-1",
            "gpt-realtime-whisper",
            "gpt-4o-transcribe",
            "gpt-4o-mini-transcribe",
            "gpt-4o-mini-transcribe-2025-03-20",
            "gpt-4o-mini-transcribe-2025-12-15",
        ];
        assert!(
            SUPPORTED.contains(&TRANSCRIPTION_MODEL),
            "{TRANSCRIPTION_MODEL} is not in the endpoint's supported set"
        );
    }

    #[tokio::test]
    async fn unrelated_events_are_ignored() {
        for payload in [
            r#"{"type":"input_audio_buffer.speech_stopped"}"#,
            r#"{"type":"rate_limits.updated"}"#,
            r#"{"type":"response.output_text.done","text":"x"}"#,
        ] {
            assert!(map_event(payload).await.is_empty(), "payload: {payload}");
        }
    }

    /// Assistant text is carried now — `refine_text` needs it — but it must
    /// arrive under its own method. The dictation accumulator keys on the
    /// transcript methods, so a model reply can never be typed as if the user
    /// had said it.
    #[tokio::test]
    async fn assistant_text_is_separated_from_the_transcript() {
        let out = map_event(r#"{"type":"response.output_text.delta","delta":"안녕"}"#).await;
        let (method, params) = out.first().expect("a mapped event");
        assert_eq!(method, "thread/realtime/response/delta");
        assert!(
            !crate::dictate::is_user_transcript_delta(method, params),
            "assistant text must not reach the dictation buffer"
        );
        assert!(!crate::dictate::is_user_transcript_done(method));
    }

    /// Exactly one terminator per response. Mapping `response.output_text.done`
    /// here as well produced two, and the stray one was consumed by the *next*
    /// refine call, which returned the previous answer in 4 ms.
    #[tokio::test]
    async fn only_response_done_terminates_a_refine() {
        let out = map_event(r#"{"type":"response.done","response":{}}"#).await;
        assert_eq!(out.first().expect("mapped").0, "thread/realtime/response/done");
        assert!(
            map_event(r#"{"type":"response.output_text.done","text":"x"}"#)
                .await
                .is_empty(),
            "output_text.done must not also terminate"
        );
    }

    /// The commit needs to know a turn is open, not just the pill: without this
    /// notification a dictation ending on a pause commits before the final
    /// turn's transcript arrives.
    #[tokio::test]
    async fn speech_started_reaches_the_commit_path() {
        let out = map_event(r#"{"type":"input_audio_buffer.speech_started"}"#).await;
        let (method, _) = out.first().expect("a mapped event");
        assert_eq!(method, "thread/realtime/speech/started");
    }
}
