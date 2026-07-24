# Codex Mic / 코덱스 마이크

**Ctrl+E to dictate text into any window using Codex's realtime API.**
**Ctrl+E를 누르면 말한 글이 커서 위치에 그대로 타이핑되는 받아쓰기 도구.**

A native desktop voice dictation tool built with Tauri + Rust. Uses Codex's experimental `thread/realtime` API with WebRTC transport and ChatGPT OAuth — no API key required.

ChatGPT OAuth로 작동하는 네이티브 데스크톱 받아쓰기 도구. Tauri + Rust로 제작. API key 불필요.

---

## How It Works / 작동 방식

```
Ctrl+E → cpal (native mic capture, 24kHz mono)
       → WebRTC RTCPeerConnection (webview)
       → SDP offer → codex app-server (thread/realtime/start, WebRTC transport)
       ← SDP answer (thread/realtime/sdp)
       ← transcript deltas (role: user) → enigo.text() at cursor position
```

1. App auto-connects to `codex app-server` on launch (ChatGPT OAuth).
   앱 실행 시 codex app-server에 자동 연결 (ChatGPT OAuth).
2. Press **Ctrl+E** anywhere → mic recording starts, pill indicator appears.
   아무 곳에서나 **Ctrl+E** → 마이크 녹음 시작, pill 표시등 표시.
3. Speak — live transcript appears in the pill (Korean/English auto-detect).
   말하면 실시간 전사가 pill에 표시 (한글/영어 자동 감지).
4. Press **Ctrl+E** again → transcribed text is typed at your cursor position.
   다시 **Ctrl+E** → 전사된 텍스트가 커서 위치에 타이핑됨.

---

## Features / 기능

| Feature | Description |
|---------|-------------|
| Native audio | `cpal` captures mic directly in Rust — no browser `getUserMedia` |
| OAuth auth | Uses ChatGPT login via codex app-server — no API key needed |
| 5-state pill UI | `hidden` → `recording` → `processing` → `success`/`error` with auto-hide |
| Click-through | Pill never steals focus from your target window |
| Hallucination filter | Filters empty transcripts and common Whisper hallucinations |
| Korean + English | Auto-detects language; code-mixing (한영혼용) supported |
| Never auto-Enter | Dictated text is typed; you decide when to press Enter |

---

## Requirements / 요구사항

- [Codex CLI](https://github.com/openai/codex) installed and signed in with ChatGPT
- Rust 1.77+ (for building from source)
- Windows 10/11 (macOS/Linux should work but untested)

---

## Build / 빌드

```sh
cd src-tauri
cargo tauri build
# Output: src-tauri/target/release/codex-mic.exe
```

## Run (dev) / 개발 실행

```sh
cd src-tauri
cargo tauri dev
```

---

## Configuration / 설정

Change the hotkey in `src-tauri/src/lib.rs`:

```rust
const HOTKEY: &str = "Ctrl+E";  // e.g. "Alt+Space", "Ctrl+Shift+D"
```

---

## Tests / 테스트

```sh
cd src-tauri
cargo test                          # Unit tests (JSON-RPC framing, audio encoding, hallucination filter)
CODEX_MIC_INTEGRATION=1 cargo test  # Integration test against real codex app-server
```

---

## Architecture / 구조

```
src-tauri/src/
  audio.rs     — Native mic capture via cpal (24kHz mono i16 → base64 PCM)
  codex.rs     — codex app-server session: spawn, initialize, thread, realtime (WebRTC)
  dictate.rs   — User-role transcript accumulator + enigo keyboard injection + hallucination filter
  jsonrpc.rs   — Stdio JSON-RPC 2.0 client (newline-delimited, no "jsonrpc" field)
  commands.rs  — Tauri #[command] interface (realtime_start, buffer, status, etc.)
  lib.rs       — Global shortcut (Ctrl+E), toggle logic, auto-connect, integration tests
ui/            — Static HTML/CSS/JS webview (5-state pill indicator)
```

---

## How the realtime API works / realtime API 작동 방식

This tool uses Codex's experimental `thread/realtime/*` JSON-RPC methods:

| Method | Purpose |
|--------|---------|
| `thread/realtime/start` | Start a WebRTC realtime session with `{transport:{type:"webrtc",sdp}}` |
| `thread/realtime/sdp` | Server returns the WebRTC answer SDP |
| `thread/realtime/transcript/delta` | Live transcript text (filtered for `role:"user"`) |
| `thread/realtime/stop` | End the realtime session |

The app-server is launched with `--enable realtime_conversation` (experimental feature flag).
Realtime calls are routed to `https://api.openai.com/v1` to bypass any custom chat-model proxy,
so your ChatGPT OAuth token is used directly — same as the Codex desktop app.

---

## Acknowledgments / 참고

Benchmarked against these excellent open-source dictation tools:
- [VoiceType-AI](https://github.com/devaxl/VoiceType-AI) — 5-state HUD, safe injection pipeline
- [dictation-tauri](https://github.com/nsimi22/dictation-tauri) — minimal Tauri pill UI
- [Voicetypr](https://github.com/moinulmoin/voicetypr) — cross-platform Tauri dictation
- [whisper.cpp](https://github.com/ggerganov/whisper.cpp) — the engine under most of these

---

## License

MIT
