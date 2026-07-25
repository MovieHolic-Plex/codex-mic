use crate::config::{self, InjectionMode};
use crate::error::RpcError;
use crate::realtime::{Notification, RealtimeSession};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{info, warn};

type SharedBuf = Arc<Mutex<String>>;

const HALLUCINATION_PATTERNS: &[&str] = &[
    "thank you for watching",
    "thanks for watching",
    "please subscribe",
    "don't forget to like",
    "see you in the next video",
    "[music]",
    "[applause]",
];

/// Below this many characters, a transcript that merely *contains* a known
/// hallucination is discarded; above it, only an exact match is. Counted in
/// characters, not bytes — a byte threshold would disable the check after ~26
/// Hangul syllables.
const SHORT_TRANSCRIPT_CHARS: usize = 80;

/// Outcome of committing a dictation, so the UI can distinguish "you said
/// nothing" from "we threw your words away".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Committed {
    /// Text was sanitized and typed at the cursor.
    Typed(String),
    /// Nothing was transcribed.
    Empty,
    /// A transcript existed but was rejected as a hallucination.
    Filtered(String),
}

pub struct DictateState {
    buffer: SharedBuf,
    listening: Mutex<bool>,
    accumulator: Mutex<Option<JoinHandle<()>>>,
}

impl DictateState {
    pub fn new() -> Self {
        Self {
            buffer: Arc::new(Mutex::new(String::new())),
            listening: Mutex::new(false),
            accumulator: Mutex::new(None),
        }
    }

    pub async fn is_listening(&self) -> bool {
        *self.listening.lock().await
    }

    /// Begin accumulating user-role transcript deltas from `session`.
    ///
    /// Any accumulator left over from a previous run is aborted first: leaking
    /// them makes every delta land in the shared buffer once per past session,
    /// which duplicates the transcript.
    pub async fn start_listening(&self, session: &Arc<RealtimeSession>) -> Result<(), String> {
        self.abort_accumulator().await;
        self.buffer.lock().await.clear();
        *self.listening.lock().await = true;
        let rx = session.subscribe();
        let buf = self.buffer.clone();
        *self.accumulator.lock().await = Some(tokio::spawn(accumulate_loop(rx, buf)));
        Ok(())
    }

    pub async fn stop_listening(&self) -> Result<Committed, String> {
        *self.listening.lock().await = false;
        self.abort_accumulator().await;

        // Single lock acquisition: taking the string and clearing it in two
        // separate locks lets a delta land in between and be silently dropped.
        let text = std::mem::take(&mut *self.buffer.lock().await);

        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(Committed::Empty);
        }
        if config::get().hallucination_filter && is_hallucination(trimmed) {
            info!("filtered hallucinated transcript");
            return Ok(Committed::Filtered(trimmed.to_string()));
        }

        let safe = sanitize_for_injection(trimmed);
        if safe.is_empty() {
            return Ok(Committed::Empty);
        }

        // enigo drives SendInput and arboard talks to the OS clipboard — both
        // block; never run them on an async worker.
        let to_inject = safe.clone();
        tokio::task::spawn_blocking(move || inject_text(&to_inject))
            .await
            .map_err(|e| format!("injection task failed: {e}"))?
            .map_err(|e| e.to_string())?;
        Ok(Committed::Typed(safe))
    }

    /// Tear down without typing anything.
    ///
    /// Used when the realtime session dies mid-recording: there is no usable
    /// transcript, and whatever partial text exists must not be injected.
    pub async fn abort(&self) {
        *self.listening.lock().await = false;
        self.abort_accumulator().await;
        self.buffer.lock().await.clear();
    }

    async fn abort_accumulator(&self) {
        if let Some(handle) = self.accumulator.lock().await.take() {
            handle.abort();
        }
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
                    // Deliberately not logging the delta itself: it is the
                    // user's speech and may contain secrets.
                    buf.lock().await.push_str(delta);
                }
            }
            Ok(_) => {}
            Err(RecvError::Closed) => break,
            Err(RecvError::Lagged(k)) => warn!(skipped = k, "dictate lagged"),
        }
    }
}

pub fn is_hallucination(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_lowercase();
    let short = trimmed.chars().count() < SHORT_TRANSCRIPT_CHARS;
    HALLUCINATION_PATTERNS
        .iter()
        .any(|p| lower == *p || (short && lower.contains(p)))
}

/// Strip anything that would act as a keystroke rather than a character.
///
/// This is what backs the "never auto-Enter" guarantee: a transcript containing
/// a newline would otherwise be replayed by `enigo.text()` as Enter, submitting
/// whatever chat box, terminal, or prompt the cursor happens to be in. Tabs are
/// dropped for the same reason (focus traversal).
pub fn sanitize_for_injection(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = false;
    for ch in text.chars() {
        let mapped = match ch {
            '\n' | '\r' | '\t' | '\u{0b}' | '\u{0c}' | '\u{85}' | '\u{2028}' | '\u{2029}' => ' ',
            c if c.is_control() => continue,
            c => c,
        };
        if mapped == ' ' {
            if last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        out.push(mapped);
    }
    out.trim().to_string()
}

pub fn is_user_transcript_delta(method: &str, params: &Value) -> bool {
    method == "thread/realtime/transcript/delta"
        && params.get("role").and_then(|r| r.as_str()) == Some("user")
}

pub fn type_text_safe(text: &str) -> Result<(), RpcError> {
    if text.is_empty() {
        return Ok(());
    }
    debug_assert!(
        !text.contains('\n') && !text.contains('\r'),
        "sanitize_for_injection must run before typing"
    );
    let mut enigo = Enigo::new(&Settings::default())
        .map_err(|e| RpcError::Spawn(format!("enigo init: {e}")))?;
    enigo
        .text(text)
        .map_err(|e| RpcError::Spawn(format!("enigo text: {e}")))?;
    info!(len = text.len(), "typed transcript");
    Ok(())
}

/// Inject the transcript using the configured mode.
///
/// `Type` replays keystrokes — universal but slow for long text and dependent
/// on the active keyboard layout. `Clipboard` stages the text and sends one
/// Ctrl+V — instant, and it can optionally restore the previous clipboard so
/// the user's own clipboard is never clobbered (the VoiceInk behavior).
fn inject_text(text: &str) -> Result<(), RpcError> {
    let cfg = config::get();
    let text = if cfg.append_trailing_space {
        format!("{text} ")
    } else {
        text.to_string()
    };
    match cfg.injection_mode {
        InjectionMode::Type => type_text_safe(&text),
        InjectionMode::Clipboard => paste_via_clipboard(&text, cfg.restore_clipboard),
    }
}

/// Settings-panel "injection test": sanitize + inject through the exact
/// production path, into whatever window currently has focus.
pub async fn test_inject(text: &str) -> Result<String, String> {
    let safe = sanitize_for_injection(text);
    if safe.is_empty() {
        return Err("injection text is empty after sanitize".to_string());
    }
    let t = safe.clone();
    tokio::task::spawn_blocking(move || inject_text(&t))
        .await
        .map_err(|e| format!("injection task failed: {e}"))?
        .map_err(|e| e.to_string())?;
    Ok(safe)
}

fn paste_via_clipboard(text: &str, restore_clipboard: bool) -> Result<(), RpcError> {
    let mut board = arboard::Clipboard::new()
        .map_err(|e| RpcError::Spawn(format!("clipboard init: {e}")))?;
    let previous = if restore_clipboard {
        board.get_text().ok()
    } else {
        None
    };
    board
        .set_text(text)
        .map_err(|e| RpcError::Spawn(format!("clipboard set: {e}")))?;

    let mut enigo = Enigo::new(&Settings::default())
        .map_err(|e| RpcError::Spawn(format!("enigo init: {e}")))?;
    enigo
        .key(Key::Control, Direction::Press)
        .and_then(|_| enigo.key(Key::Unicode('v'), Direction::Click))
        .and_then(|_| enigo.key(Key::Control, Direction::Release))
        .map_err(|e| RpcError::Spawn(format!("enigo paste: {e}")))?;
    info!(len = text.len(), "pasted transcript via clipboard");

    // Give the target app a beat to read the clipboard before restoring it —
    // VoiceInk uses ~2s; 1s has been enough in practice and stalls the caller
    // less. Restoration itself is best-effort.
    if let Some(prev) = previous {
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let _ = board.set_text(prev);
    }
    Ok(())
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
            "thread/realtime/started",
            &json!({ "role": "user" })
        ));
    }

    #[tokio::test]
    async fn empty_and_whitespace_transcripts_commit_as_empty() {
        for input in ["", "   ", "\n\t "] {
            let state = DictateState::new();
            *state.buffer.lock().await = input.into();
            assert_eq!(
                state.stop_listening().await.unwrap(),
                Committed::Empty,
                "input {input:?}"
            );
        }
    }

    #[test]
    fn hallucination_filtered() {
        assert!(is_hallucination("thank you for watching"));
        assert!(is_hallucination("[music]"));
        assert!(is_hallucination("Please subscribe"));
    }

    #[test]
    fn normal_korean_not_filtered() {
        assert!(!is_hallucination("안녕하세요"));
        assert!(!is_hallucination("이거 API endpoint를 deploy해줘"));
    }

    #[test]
    fn normal_english_not_filtered() {
        assert!(!is_hallucination("hello world"));
        assert!(!is_hallucination("add a function that calculates factorial"));
    }

    /// The short-transcript threshold is measured in characters; a byte-based
    /// one silently disabled the check for Korean after ~26 syllables.
    #[test]
    fn long_korean_transcript_still_checked_by_length() {
        // 30 syllables is only 30 chars but 90 bytes — under the old byte-based
        // check this transcript skipped the "contains" branch entirely.
        let short_korean_long_in_bytes = format!("{} please subscribe", "가".repeat(25));
        assert!(short_korean_long_in_bytes.chars().count() < SHORT_TRANSCRIPT_CHARS);
        assert!(short_korean_long_in_bytes.len() > SHORT_TRANSCRIPT_CHARS);
        assert!(
            is_hallucination(&short_korean_long_in_bytes),
            "char-count threshold must still catch this"
        );

        // A genuinely long transcript is only rejected on an exact match.
        let long = format!("{} please subscribe", "word ".repeat(30));
        assert!(long.chars().count() > SHORT_TRANSCRIPT_CHARS);
        assert!(!is_hallucination(&long));
    }

    #[test]
    fn newline_never_survives_injection() {
        assert_eq!(sanitize_for_injection("send it\nnow"), "send it now");
        assert_eq!(sanitize_for_injection("a\r\nb"), "a b");
        assert_eq!(sanitize_for_injection("line1\n\n\nline2"), "line1 line2");
        assert_eq!(sanitize_for_injection("\ndeploy\n"), "deploy");
    }

    #[test]
    fn tabs_and_control_chars_stripped() {
        assert_eq!(sanitize_for_injection("a\tb"), "a b");
        assert_eq!(sanitize_for_injection("a\u{0007}b"), "ab");
        assert_eq!(sanitize_for_injection("a\u{2028}b"), "a b");
    }

    #[test]
    fn sanitize_preserves_normal_text() {
        assert_eq!(sanitize_for_injection("안녕하세요"), "안녕하세요");
        assert_eq!(
            sanitize_for_injection("deploy the API endpoint 해줘"),
            "deploy the API endpoint 해줘"
        );
        assert_eq!(sanitize_for_injection("emoji 🎤 ok"), "emoji 🎤 ok");
    }

    #[test]
    fn sanitize_of_only_control_chars_is_empty() {
        assert_eq!(sanitize_for_injection("\n\r\t"), "");
    }

    #[tokio::test]
    async fn stop_without_start_is_empty_not_error() {
        let state = DictateState::new();
        assert_eq!(state.stop_listening().await.unwrap(), Committed::Empty);
    }

    #[tokio::test]
    async fn hallucinated_buffer_reports_filtered_not_silently_dropped() {
        let state = DictateState::new();
        *state.buffer.lock().await = "thank you for watching".into();
        assert_eq!(
            state.stop_listening().await.unwrap(),
            Committed::Filtered("thank you for watching".into())
        );
    }
}
