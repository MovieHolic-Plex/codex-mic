use crate::config::{self, InjectionMode};
use crate::error::RpcError;
use crate::realtime::Notification;
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// How long the target app is given to release its modifiers before we type.
const MODIFIER_GRACE: Duration = Duration::from_millis(600);

/// Everything the accumulator has heard about the current dictation.
#[derive(Debug, Default)]
struct Incoming {
    /// Concatenated `transcript/delta` fragments.
    text: String,
    /// The authoritative full transcript from `turn.done`, when it arrives.
    done: Option<String>,
    /// When the last transcript event landed — drives the quiet-window commit.
    last: Option<Instant>,
    /// Whether any transcript event arrived at all this dictation.
    seen: bool,
}

impl Incoming {
    fn mark(&mut self) {
        self.seen = true;
        self.last = Some(Instant::now());
    }

    /// The best transcript available: `turn.done` if the server sent one,
    /// otherwise the accumulated deltas.
    fn resolve(self) -> String {
        self.done.unwrap_or(self.text)
    }
}

type Shared = Arc<Mutex<Incoming>>;

/// How long a commit waits for the transcript after the audio stops.
///
/// Transcription lags speech: the server only emits the tail of an utterance
/// (and `turn.done`) after the audio has drained through its VAD. Committing
/// the instant the key comes up — what this used to do — truncated the last
/// words of every dictation and returned nothing at all for short ones.
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// Hard ceiling on the wait.
    pub deadline: Duration,
    /// Commit once the transcript has been quiet this long.
    pub quiet: Duration,
    /// Give up early if not one transcript event ever arrived.
    pub no_event: Duration,
    /// Poll granularity.
    pub poll: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            deadline: Duration::from_millis(3500),
            quiet: Duration::from_millis(900),
            no_event: Duration::from_millis(1500),
            poll: Duration::from_millis(25),
        }
    }
}

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
    /// Text was sanitized and (by `stop_listening`) typed at the cursor.
    Typed(String),
    /// Nothing was transcribed.
    Empty,
    /// A transcript existed but was rejected as a hallucination.
    Filtered(String),
}

pub struct DictateState {
    incoming: Shared,
    listening: Mutex<bool>,
    accumulator: Mutex<Option<JoinHandle<()>>>,
}

impl DictateState {
    pub fn new() -> Self {
        Self {
            incoming: Arc::new(Mutex::new(Incoming::default())),
            listening: Mutex::new(false),
            accumulator: Mutex::new(None),
        }
    }

    pub async fn is_listening(&self) -> bool {
        *self.listening.lock().await
    }

    /// Begin accumulating user-role transcript events from `rx`.
    ///
    /// Any accumulator left over from a previous run is aborted first: leaking
    /// them makes every delta land in the shared buffer once per past session,
    /// which duplicates the transcript.
    pub async fn start_listening(&self, rx: broadcast::Receiver<Notification>) -> Result<(), String> {
        self.abort_accumulator().await;
        *self.incoming.lock().await = Incoming::default();
        *self.listening.lock().await = true;
        let incoming = self.incoming.clone();
        *self.accumulator.lock().await = Some(tokio::spawn(accumulate_loop(rx, incoming)));
        Ok(())
    }

    /// Settle the transcript and inject it at the cursor.
    pub async fn stop_listening(&self) -> Result<Committed, String> {
        let outcome = self.finish(Timings::default()).await?;
        if let Committed::Typed(text) = &outcome {
            // enigo drives SendInput and arboard talks to the OS clipboard —
            // both block; never run them on an async worker.
            let to_inject = text.clone();
            tokio::task::spawn_blocking(move || inject_text(&to_inject))
                .await
                .map_err(|e| format!("injection task failed: {e}"))?
                .map_err(|e| e.to_string())?;
        }
        Ok(outcome)
    }

    /// Wait for the transcript to settle, then filter and sanitize it.
    ///
    /// Resolution only — injection is [`Self::stop_listening`]'s job, so tests
    /// can exercise the whole timing path without typing into the developer's
    /// screen.
    ///
    /// The wait ends at the first of: `turn.done` arriving, the transcript
    /// going quiet for `quiet` with text in hand, `no_event` passing with
    /// nothing heard at all, or the hard `deadline`. When no accumulator is
    /// running there is nothing to wait for and the commit is immediate.
    pub async fn finish(&self, t: Timings) -> Result<Committed, String> {
        *self.listening.lock().await = false;
        if self.accumulator.lock().await.is_some() {
            self.await_transcript(t).await;
        }
        self.abort_accumulator().await;

        // Single lock acquisition: taking the transcript and clearing it in two
        // separate locks lets a delta land in between and be silently dropped.
        let text = std::mem::take(&mut *self.incoming.lock().await).resolve();

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
        Ok(Committed::Typed(safe))
    }

    /// Block until the transcript has settled — see [`Timings`].
    async fn await_transcript(&self, t: Timings) {
        let start = Instant::now();
        loop {
            {
                let inc = self.incoming.lock().await;
                if inc.done.is_some() {
                    info!("commit: turn.done received");
                    return;
                }
                let elapsed = start.elapsed();
                if elapsed >= t.deadline {
                    warn!(?elapsed, "commit: transcript deadline hit");
                    return;
                }
                if !inc.seen && elapsed >= t.no_event {
                    info!("commit: no transcript events at all");
                    return;
                }
                if let Some(last) = inc.last {
                    if !inc.text.trim().is_empty() && last.elapsed() >= t.quiet {
                        info!("commit: transcript went quiet");
                        return;
                    }
                }
            }
            tokio::time::sleep(t.poll).await;
        }
    }

    /// Tear down without typing anything.
    ///
    /// Used when the realtime session dies mid-recording: there is no usable
    /// transcript, and whatever partial text exists must not be injected.
    pub async fn abort(&self) {
        *self.listening.lock().await = false;
        self.abort_accumulator().await;
        *self.incoming.lock().await = Incoming::default();
    }

    async fn abort_accumulator(&self) {
        if let Some(handle) = self.accumulator.lock().await.take() {
            handle.abort();
        }
    }

    /// Live preview text for the pill.
    pub async fn current_buffer(&self) -> String {
        let inc = self.incoming.lock().await;
        inc.done.clone().unwrap_or_else(|| inc.text.clone())
    }

    #[cfg(test)]
    async fn set_text(&self, text: &str) {
        self.incoming.lock().await.text = text.to_string();
    }
}

impl Default for DictateState {
    fn default() -> Self {
        Self::new()
    }
}

async fn accumulate_loop(mut rx: broadcast::Receiver<Notification>, incoming: Shared) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(n) if is_user_transcript_delta(&n.method, &n.params) => {
                if let Some(delta) = n.params.get("delta").and_then(|d| d.as_str()) {
                    // Deliberately not logging the delta itself: it is the
                    // user's speech and may contain secrets.
                    let mut inc = incoming.lock().await;
                    inc.text.push_str(delta);
                    inc.mark();
                }
            }
            // The authoritative end-of-turn transcript. Used in preference to
            // the accumulated deltas, which can arrive partial or repeated.
            Ok(n) if is_user_transcript_done(&n.method) => {
                if let Some(text) = n.params.get("text").and_then(|t| t.as_str()) {
                    let mut inc = incoming.lock().await;
                    inc.done = Some(text.to_string());
                    inc.mark();
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

/// `turn.done` is only mapped for the user role upstream, so the method alone
/// is enough — assistant turns never reach this notification.
pub fn is_user_transcript_done(method: &str) -> bool {
    method == "thread/realtime/transcript/done"
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
    // `global-hotkey` reports "released" as soon as the hotkey's *main* key
    // comes up, so with a chord like Ctrl+E the commit starts while Ctrl is
    // still held. Typing then lands as Ctrl+letter shortcuts and a paste
    // becomes Ctrl+Shift+V. Wait the modifiers out; if the user really is
    // leaning on one, inject anyway rather than lose the transcript.
    if !crate::winkeys::wait_for_modifiers_release(MODIFIER_GRACE) {
        warn!("modifiers still held after grace period; injecting anyway");
    }
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

    /// Short timings so the wait-for-transcript tests stay fast.
    fn fast() -> Timings {
        Timings {
            deadline: Duration::from_millis(600),
            quiet: Duration::from_millis(120),
            no_event: Duration::from_millis(200),
            poll: Duration::from_millis(10),
        }
    }

    fn delta(text: &str) -> Notification {
        Notification {
            method: "thread/realtime/transcript/delta".into(),
            params: json!({ "role": "user", "delta": text }),
        }
    }

    fn done(text: &str) -> Notification {
        Notification {
            method: "thread/realtime/transcript/done".into(),
            params: json!({ "text": text }),
        }
    }

    #[tokio::test]
    async fn empty_and_whitespace_transcripts_commit_as_empty() {
        for input in ["", "   ", "\n\t "] {
            let state = DictateState::new();
            state.set_text(input).await;
            assert_eq!(
                state.stop_listening().await.unwrap(),
                Committed::Empty,
                "input {input:?}"
            );
        }
    }

    /// The core timing fix: the transcript keeps arriving after the microphone
    /// stops. Committing on key-up alone dropped the tail of every utterance.
    #[tokio::test]
    async fn commit_waits_for_deltas_that_arrive_after_the_key_is_released() {
        let (tx, _) = broadcast::channel(16);
        let state = DictateState::new();
        state.start_listening(tx.subscribe()).await.unwrap();
        tx.send(delta("배포")).unwrap();

        // The rest of the sentence lands after the user has already let go —
        // within the quiet window, so the commit has to still be waiting.
        let late = tx.clone();
        let lag = Duration::from_millis(60);
        tokio::spawn(async move {
            tokio::time::sleep(lag).await;
            let _ = late.send(delta(" 해줘"));
        });

        let start = Instant::now();
        let out = state.finish(fast()).await.unwrap();
        assert_eq!(out, Committed::Typed("배포 해줘".into()));
        assert!(
            start.elapsed() >= lag,
            "committed before the tail could arrive"
        );
    }

    /// `turn.done` carries the authoritative transcript and wins over whatever
    /// the deltas happened to accumulate.
    #[tokio::test]
    async fn turn_done_overrides_accumulated_deltas_and_ends_the_wait() {
        let (tx, _) = broadcast::channel(16);
        let state = DictateState::new();
        state.start_listening(tx.subscribe()).await.unwrap();
        tx.send(delta("안녕")).unwrap();
        tx.send(done("안녕하세요")).unwrap();

        let start = Instant::now();
        let out = state.finish(fast()).await.unwrap();
        assert_eq!(out, Committed::Typed("안녕하세요".into()));
        assert!(
            start.elapsed() < Duration::from_millis(120),
            "turn.done must end the wait immediately, took {:?}",
            start.elapsed()
        );
    }

    /// A dead session must not stall the commit for the full deadline.
    #[tokio::test]
    async fn silent_session_gives_up_at_the_no_event_grace() {
        let (tx, _) = broadcast::channel(16);
        let state = DictateState::new();
        state.start_listening(tx.subscribe()).await.unwrap();

        let t = fast();
        let start = Instant::now();
        assert_eq!(state.finish(t).await.unwrap(), Committed::Empty);
        let elapsed = start.elapsed();
        assert!(elapsed >= t.no_event, "returned too early: {elapsed:?}");
        assert!(elapsed < t.deadline, "waited the full deadline: {elapsed:?}");
    }

    /// A server that streams forever must not hold the commit open forever.
    #[tokio::test]
    async fn endless_deltas_are_cut_off_at_the_deadline() {
        let (tx, _) = broadcast::channel(64);
        let state = DictateState::new();
        state.start_listening(tx.subscribe()).await.unwrap();
        let chatty = tx.clone();
        tokio::spawn(async move {
            loop {
                if chatty.send(delta("x")).is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });

        let t = fast();
        let start = Instant::now();
        let out = state.finish(t).await.unwrap();
        assert!(matches!(out, Committed::Typed(_)), "got {out:?}");
        assert!(
            start.elapsed() < t.deadline + Duration::from_millis(300),
            "overran the deadline: {:?}",
            start.elapsed()
        );
    }

    /// Once the transcript stops moving there is no reason to sit out the rest
    /// of the deadline.
    #[tokio::test]
    async fn quiet_transcript_commits_without_waiting_for_the_deadline() {
        let (tx, _) = broadcast::channel(16);
        let state = DictateState::new();
        state.start_listening(tx.subscribe()).await.unwrap();
        tx.send(delta("hello")).unwrap();

        let t = fast();
        let start = Instant::now();
        assert_eq!(state.finish(t).await.unwrap(), Committed::Typed("hello".into()));
        let elapsed = start.elapsed();
        assert!(elapsed >= t.quiet, "committed before the quiet window: {elapsed:?}");
        assert!(elapsed < t.deadline, "waited for the deadline: {elapsed:?}");
    }

    /// Assistant chatter and non-transcript traffic must never extend the wait
    /// or reach the buffer.
    #[tokio::test]
    async fn unrelated_events_do_not_feed_the_transcript() {
        let (tx, _) = broadcast::channel(16);
        let state = DictateState::new();
        state.start_listening(tx.subscribe()).await.unwrap();
        tx.send(Notification {
            method: "thread/realtime/transcript/delta".into(),
            params: json!({ "role": "assistant", "delta": "나는 어시스턴트" }),
        })
        .unwrap();
        tx.send(Notification {
            method: "thread/realtime/started".into(),
            params: json!({}),
        })
        .unwrap();

        assert_eq!(state.finish(fast()).await.unwrap(), Committed::Empty);
    }

    /// A recording that is aborted mid-flight must leave nothing behind for the
    /// next one to inject.
    #[tokio::test]
    async fn abort_discards_the_transcript() {
        let (tx, _) = broadcast::channel(16);
        let state = DictateState::new();
        state.start_listening(tx.subscribe()).await.unwrap();
        tx.send(delta("secret")).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(state.current_buffer().await, "secret");

        state.abort().await;
        assert!(!state.is_listening().await);
        assert_eq!(state.current_buffer().await, "");
        assert_eq!(state.finish(fast()).await.unwrap(), Committed::Empty);
    }

    /// Restarting must not resurrect the previous dictation's text, and the old
    /// accumulator must be gone — two live accumulators duplicated every delta.
    #[tokio::test]
    async fn restart_clears_previous_transcript_and_retires_the_accumulator() {
        let (first, _) = broadcast::channel(16);
        let (second, _) = broadcast::channel(16);
        let state = DictateState::new();
        state.start_listening(first.subscribe()).await.unwrap();
        first.send(delta("old")).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        state.start_listening(second.subscribe()).await.unwrap();
        assert_eq!(state.current_buffer().await, "");
        // Traffic on the retired channel must not reach the new dictation.
        first.send(delta("stale")).unwrap();
        second.send(delta("new")).unwrap();
        assert_eq!(state.finish(fast()).await.unwrap(), Committed::Typed("new".into()));
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
        state.set_text("thank you for watching").await;
        assert_eq!(
            state.stop_listening().await.unwrap(),
            Committed::Filtered("thank you for watching".into())
        );
    }
}
