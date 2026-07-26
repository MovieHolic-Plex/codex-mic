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
///
/// One hold is not one turn. Server VAD closes a turn after
/// `silence_duration_ms` of quiet (500 ms), so an ordinary sentence pause
/// splits a single dictation into several turns, each with its own delta
/// stream and its own `transcript/done`. Treating a `done` as *the* transcript
/// — which this used to do — kept only the last segment and threw the rest of
/// the sentence away.
#[derive(Debug, Default)]
struct Incoming {
    /// Transcripts of the turns the server has already finished, in order.
    committed: String,
    /// Deltas for the turn still in flight. Superseded by that turn's `done`.
    pending: String,
    /// How many turns have completed.
    turns: usize,
    /// VAD has opened a turn that has not produced its transcript yet. Between
    /// speech ending and the transcript arriving there are no deltas at all, so
    /// without this the commit cannot tell "nothing more is coming" from
    /// "transcription is still running".
    speech_open: bool,
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

    /// Close the current turn with its authoritative transcript.
    ///
    /// The `done` text is the trustworthy version of the deltas we accumulated
    /// for *this* turn, so it replaces `pending` — and appends to, rather than
    /// replaces, the turns that came before it.
    fn finish_turn(&mut self, text: &str) {
        self.pending.clear();
        self.speech_open = false;
        self.turns += 1;
        append_segment(&mut self.committed, text);
        self.mark();
    }

    /// Everything heard so far: the finished turns plus the turn in flight.
    fn resolve(self) -> String {
        let mut out = self.committed;
        append_segment(&mut out, &self.pending);
        out
    }
}

/// Join transcript segments with a single space. Segments are whole utterances
/// split apart by a VAD pause, so they need a separator, but the server already
/// pads its own with spaces often enough that blind concatenation double-spaces.
fn append_segment(out: &mut String, segment: &str) {
    let segment = segment.trim();
    if segment.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(segment);
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
    /// Commit once a turn's deltas have stalled this long without its `done`.
    pub quiet: Duration,
    /// How long a turn VAD has opened is given to produce its transcript.
    ///
    /// Longer than `quiet` because this window covers the whole tail of the
    /// pipeline: VAD still needs its `silence_duration_ms` to close the turn,
    /// and only then does transcription run. Measured from the last transcript
    /// event, ~1s is typical, so a 900 ms window drops the final sentence of
    /// every dictation that ends on a pause.
    pub open_turn: Duration,
    /// Commit this long after a turn completes with nothing following it.
    ///
    /// A completed turn is not the end of the dictation — VAD splits one hold
    /// into several — so the wait cannot stop the moment a `done` lands. It
    /// waits out this window instead, long enough for the next turn's first
    /// delta to appear if there is one, short enough not to be felt.
    pub settle: Duration,
    /// Give up early if not one transcript event ever arrived.
    pub no_event: Duration,
    /// Poll granularity.
    pub poll: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            deadline: Duration::from_millis(10_000),
            quiet: Duration::from_millis(900),
            // Measured key-up-to-transcript on live audio is 1.7-1.9 s, so the
            // old 1800 ms sat exactly on the boundary. This window costs
            // nothing when things work — the transcript arriving exits through
            // `settle` — and only bounds the case where it never comes.
            open_turn: Duration::from_millis(5_000),
            settle: Duration::from_millis(300),
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
        self.stop_listening_with(None).await
    }

    /// As [`Self::stop_listening`], but preferring a transcript produced
    /// elsewhere — the session model's own reading of the audio.
    ///
    /// The side channel is still drained either way: it settles the accumulator
    /// and stands in whenever the model returns nothing.
    pub async fn stop_listening_with(
        &self,
        preferred: Option<String>,
    ) -> Result<Committed, String> {
        let fallback = self.finish(Timings::default()).await?;
        let outcome = match preferred {
            Some(text) if !text.trim().is_empty() => classify(&text),
            _ => fallback,
        };
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
    /// The wait ends at the first of: every turn so far having completed and
    /// `settle` passing with no new turn starting, the transcript going quiet
    /// for `quiet` with text in hand, `no_event` passing with nothing heard at
    /// all, or the hard `deadline`. When no accumulator is running there is
    /// nothing to wait for and the commit is immediate.
    pub async fn finish(&self, t: Timings) -> Result<Committed, String> {
        *self.listening.lock().await = false;
        if self.accumulator.lock().await.is_some() {
            self.await_transcript(t).await;
        }
        self.abort_accumulator().await;

        // Single lock acquisition: taking the transcript and clearing it in two
        // separate locks lets a delta land in between and be silently dropped.
        let text = std::mem::take(&mut *self.incoming.lock().await).resolve();

        Ok(classify(&text))
    }

    /// Block until the transcript has settled — see [`Timings`].
    async fn await_transcript(&self, t: Timings) {
        let start = Instant::now();
        loop {
            {
                let inc = self.incoming.lock().await;
                let elapsed = start.elapsed();
                if elapsed >= t.deadline {
                    warn!(?elapsed, "commit: transcript deadline hit");
                    return;
                }
                // Time since the last transcript event, or since the wait began
                // if none has arrived. A committed turn typically has no events
                // yet, so anchoring only on `last` would leave it unmeasured.
                let idle = inc.last.map(|l| l.elapsed()).unwrap_or(elapsed);
                if inc.speech_open {
                    // Audio has been accepted for transcription and its text is
                    // owed. Words are at stake, so this waits the longest — and
                    // it must outrank the no-event grace, which otherwise gives
                    // up on a turn we know is coming. That ordering bug
                    // committed every real dictation empty at 1.5 s while the
                    // transcript, measured at 1.7-1.9 s, was still in flight.
                    if idle >= t.open_turn {
                        warn!(?idle, "commit: a committed turn never transcribed");
                        return;
                    }
                } else if !inc.seen && elapsed >= t.no_event {
                    info!("commit: no transcript events at all");
                    return;
                } else if !inc.pending.trim().is_empty() {
                    // Deltas arrived but stalled without a `done`.
                    if idle >= t.quiet {
                        info!("commit: transcript went quiet mid-turn");
                        return;
                    }
                } else if inc.turns > 0 && idle >= t.settle {
                    // Every turn closed and none opened since: the dictation is
                    // over. Waiting longer is pure latency.
                    info!(turns = inc.turns, "commit: all turns settled");
                    return;
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

    /// Declare that audio has been committed and its transcript is owed.
    ///
    /// With server VAD off there are no `speech_started` events, so nothing
    /// else tells the commit that a turn is in flight — it would see an idle
    /// session and give up at the no-event grace while the transcript was still
    /// being produced.
    pub async fn expect_turn(&self) {
        self.incoming.lock().await.speech_open = true;
    }

    /// Live preview text for the pill.
    pub async fn current_buffer(&self) -> String {
        let inc = self.incoming.lock().await;
        let mut out = inc.committed.clone();
        append_segment(&mut out, &inc.pending);
        out
    }

    #[cfg(test)]
    async fn set_text(&self, text: &str) {
        self.incoming.lock().await.pending = text.to_string();
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
                    inc.pending.push_str(delta);
                    inc.mark();
                }
            }
            // The authoritative transcript for the turn that just closed. It
            // supersedes that turn's deltas (which can arrive partial or
            // repeated) and is appended to the turns before it — one hold can
            // contain several.
            Ok(n) if is_user_transcript_done(&n.method) => {
                if let Some(text) = n.params.get("text").and_then(|t| t.as_str()) {
                    incoming.lock().await.finish_turn(text);
                }
            }
            // VAD opened a turn. Deliberately does not `mark()`: this is not a
            // transcript event, and letting it feed the quiet/no-event timers
            // would keep a silent session waiting for the full deadline.
            Ok(n) if is_speech_started(&n.method) => {
                incoming.lock().await.speech_open = true;
            }
            Ok(_) => {}
            Err(RecvError::Closed) => break,
            Err(RecvError::Lagged(k)) => warn!(skipped = k, "dictate lagged"),
        }
    }
}

/// Judge raw transcript text: hallucination filter, then sanitize.
///
/// Shared so that a transcript from the session model and one from the
/// input-transcription side channel are held to identical standards — the
/// never-auto-Enter guarantee in particular must not depend on which produced
/// it.
pub fn classify(text: &str) -> Committed {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Committed::Empty;
    }
    if config::get().hallucination_filter && is_hallucination(trimmed) {
        info!("filtered hallucinated transcript");
        return Committed::Filtered(trimmed.to_string());
    }
    let safe = sanitize_for_injection(trimmed);
    if safe.is_empty() {
        return Committed::Empty;
    }
    Committed::Typed(safe)
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

/// Server VAD detected the start of an utterance.
pub fn is_speech_started(method: &str) -> bool {
    method == "thread/realtime/speech/started"
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
            deadline: Duration::from_millis(900),
            quiet: Duration::from_millis(120),
            open_turn: Duration::from_millis(300),
            settle: Duration::from_millis(50),
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

    fn speech_started() -> Notification {
        Notification {
            method: "thread/realtime/speech/started".into(),
            params: json!({}),
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

    /// `turn.done` carries the authoritative transcript for its turn and wins
    /// over whatever that turn's deltas happened to accumulate.
    #[tokio::test]
    async fn turn_done_overrides_that_turns_deltas() {
        let (tx, _) = broadcast::channel(16);
        let state = DictateState::new();
        state.start_listening(tx.subscribe()).await.unwrap();
        tx.send(delta("안녕")).unwrap();
        tx.send(done("안녕하세요")).unwrap();

        let t = fast();
        let start = Instant::now();
        let out = state.finish(t).await.unwrap();
        assert_eq!(out, Committed::Typed("안녕하세요".into()));
        assert!(
            start.elapsed() < t.quiet,
            "a settled turn must not wait out the full quiet window, took {:?}",
            start.elapsed()
        );
    }

    /// The bug that made real dictation unusable: server VAD closes a turn
    /// after 500 ms of silence, so an ordinary sentence pause splits one hold
    /// into several turns. Each `done` used to overwrite the last, so only the
    /// final fragment survived and everything said before the pause was thrown
    /// away.
    #[tokio::test]
    async fn every_turn_of_one_hold_is_kept() {
        let (tx, _) = broadcast::channel(16);
        let state = DictateState::new();
        state.start_listening(tx.subscribe()).await.unwrap();

        tx.send(delta("안녕")).unwrap();
        tx.send(done("안녕하세요.")).unwrap();
        tx.send(delta("지금 마이크")).unwrap();
        tx.send(done("지금 마이크 테스트를 하고 있습니다.")).unwrap();
        tx.send(delta("오후에는 코드 리뷰를 해야 합니다.")).unwrap();

        assert_eq!(
            state.finish(fast()).await.unwrap(),
            Committed::Typed(
                "안녕하세요. 지금 마이크 테스트를 하고 있습니다. 오후에는 코드 리뷰를 해야 합니다."
                    .into()
            )
        );
    }

    /// A turn whose audio VAD has already accepted must not be abandoned just
    /// because its transcript is slow. Between `speech_started` and the
    /// transcript there are no deltas at all, so a purely timer-based wait
    /// committed early and dropped the last thing the user said.
    #[tokio::test]
    async fn an_open_turn_is_waited_for_even_with_no_deltas_yet() {
        let (tx, _) = broadcast::channel(16);
        let state = DictateState::new();
        state.start_listening(tx.subscribe()).await.unwrap();
        tx.send(done("첫 문장입니다.")).unwrap();
        tx.send(speech_started()).unwrap();

        // Its transcript lands long after both the settle and the stalled-delta
        // windows would have fired — only `open_turn` keeps the commit waiting.
        let t = fast();
        let lag = (t.quiet + t.open_turn) / 2;
        assert!(lag > t.quiet && lag < t.open_turn, "lag must isolate open_turn");
        let late = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(lag).await;
            let _ = late.send(done("이어서 말합니다."));
        });

        let start = Instant::now();
        let out = state.finish(t).await.unwrap();
        assert_eq!(out, Committed::Typed("첫 문장입니다. 이어서 말합니다.".into()));
        assert!(start.elapsed() >= lag, "committed before the open turn landed");
    }

    /// A committed turn has no transcript events yet by definition, so the
    /// no-event grace must not apply to it. When it did, every real dictation
    /// committed empty 1.5 s after key-up while the transcript — measured at
    /// 1.7-1.9 s — was still in flight:
    ///
    /// ```text
    /// audio capture stopped
    /// commit: no transcript events at all      (1.5s later)
    /// dictation committed: EMPTY  peak_rms=4851
    /// ```
    #[tokio::test]
    async fn a_committed_turn_outranks_the_no_event_grace() {
        let (tx, _) = broadcast::channel(16);
        let state = DictateState::new();
        state.start_listening(tx.subscribe()).await.unwrap();
        // Exactly what `stop_dictation` does: commit the audio, then wait.
        state.expect_turn().await;

        let t = fast();
        let lag = (t.no_event + t.open_turn) / 2;
        assert!(lag > t.no_event && lag < t.open_turn, "lag must isolate the bug");
        let late = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(lag).await;
            let _ = late.send(done("늦게 도착한 전사"));
        });

        assert_eq!(
            state.finish(t).await.unwrap(),
            Committed::Typed("늦게 도착한 전사".into()),
            "gave up on a turn we had committed audio for"
        );
    }

    /// …but a committed turn whose transcript never comes still has to end.
    #[tokio::test]
    async fn a_committed_turn_that_never_transcribes_is_bounded() {
        let state = DictateState::new();
        let (tx, _) = broadcast::channel(16);
        state.start_listening(tx.subscribe()).await.unwrap();
        state.expect_turn().await;

        let t = fast();
        let start = Instant::now();
        assert_eq!(state.finish(t).await.unwrap(), Committed::Empty);
        let elapsed = start.elapsed();
        assert!(elapsed >= t.open_turn, "gave up too early: {elapsed:?}");
        assert!(elapsed < t.deadline, "waited the full deadline: {elapsed:?}");
    }

    /// The production windows must be ordered, or one silently shadows another
    /// — which is how `open_turn` ended up unreachable behind `no_event`.
    #[test]
    fn default_timings_are_consistently_ordered() {
        let t = Timings::default();
        assert!(t.settle < t.quiet, "settle must be the quickest exit");
        assert!(
            t.open_turn > t.no_event,
            "a committed turn must outlast the no-event grace"
        );
        assert!(
            t.deadline > t.open_turn,
            "the deadline must not pre-empt the open-turn window"
        );
    }

    /// An open turn still cannot hold the commit open forever — a VAD hit whose
    /// transcript never arrives is bounded by `open_turn`, not the deadline.
    #[tokio::test]
    async fn an_open_turn_that_never_transcribes_still_commits() {
        let (tx, _) = broadcast::channel(16);
        let state = DictateState::new();
        state.start_listening(tx.subscribe()).await.unwrap();
        tx.send(done("말한 것")).unwrap();
        tx.send(speech_started()).unwrap();

        let t = fast();
        let start = Instant::now();
        assert_eq!(state.finish(t).await.unwrap(), Committed::Typed("말한 것".into()));
        let elapsed = start.elapsed();
        assert!(elapsed >= t.open_turn, "gave up too early: {elapsed:?}");
        assert!(elapsed < t.deadline, "waited the full deadline: {elapsed:?}");
    }

    /// Segments are separate utterances and must not be glued together, however
    /// the server pads them.
    #[test]
    fn segments_join_with_exactly_one_space() {
        let mut out = String::new();
        append_segment(&mut out, "안녕하세요.");
        append_segment(&mut out, "  반갑습니다.  ");
        append_segment(&mut out, "   ");
        append_segment(&mut out, "");
        append_segment(&mut out, "끝.");
        assert_eq!(out, "안녕하세요. 반갑습니다. 끝.");

        let mut first = String::new();
        append_segment(&mut first, "  leading  ");
        assert_eq!(first, "leading", "no space before the first segment");
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
