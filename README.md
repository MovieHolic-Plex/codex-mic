# Codex Mic / 코덱스 마이크

> **Research project / 연구용 프로젝트.** This is an unofficial, reverse-engineered
> experiment in how ChatGPT's realtime voice protocol works. It is not affiliated
> with OpenAI. Using undocumented endpoints with your account can get you
> rate-limited or suspended under OpenAI's ToS — **use at your own risk**, read
> the code, and adjust it to your needs.
>
> 이 프로젝트는 ChatGPT realtime 음성 프로토콜을 리버스엔지니어링한 **비공식 연구용
> 실험**입니다. OpenAI와 무관하며, 비공식 엔드포인트 사용은 계정 제한/정지 사유가
> 될 수 있습니다. **각자 책임 하에** 코드를 읽고 입맛에 맞게 고쳐 쓰세요.

**Hold Ctrl+E, speak, release — your words are typed wherever the keyboard focus is.**
**Ctrl+E를 누르고 말한 뒤 손을 떼면, 키보드 포커스가 있는 곳에 그대로 타이핑됩니다.**

A native Windows voice dictation tool (Tauri + Rust) that streams your microphone
to ChatGPT's realtime voice model over WebRTC — authenticated with nothing but
your **Codex CLI login** (`codex login`). No API key, no attestation, no app-server.

마이크를 ChatGPT realtime 음성 모델로 WebRTC 스트리밍하는 네이티브 윈도우
받아쓰기 도구(Tauri + Rust). **Codex CLI 로그인(OAuth) 하나만으로** 인증됩니다.
API 키도, attestation도, app-server도 필요 없습니다.

---

## Why this is interesting / 흥미로운 지점

The full realtime voice path was reverse-engineered and verified live
(see [REVERSE-ENGINEERING.md](REVERSE-ENGINEERING.md) for the complete trail):

- The frameless ("quicksilver") call-create protocol and the exact session JSON,
  recovered from the Codex desktop app and the `codex-rs` source
- ChatGPT-OAuth bearer works directly — the desktop app's DeviceCheck/attestation
  is **not** required on this path
- The server-side **PCMU path is broken** (accepts the codec, returns gibberish)
  and WS-only audio is refused — Opus over the WebRTC track is the only way
- Sessions silently go stale after a few idle minutes; quiet laptop/USB mics
  need ~+20 dB of gain before server VAD ever fires

전체 음성 경로를 리버스엔지니어링하고 라이브로 검증했습니다. 프로토콜 상세,
PCMU 함정, 세션 스테일, 마이크 게인 이슈까지 전 과정이
[REVERSE-ENGINEERING.md](REVERSE-ENGINEERING.md)에 기록되어 있습니다.

---

## Usage / 사용법

```
1. Click where you want text (terminal, editor, chat — anywhere)
2. Hold Ctrl+E and speak
3. Release — the transcript is typed at the cursor

1. 타이핑할 곳 클릭 (터미널, 에디터 어디든)
2. Ctrl+E를 누른 채로 말하기
3. 손 떼기 → 커서 위치에 타이핑
```

The pill shows a live level meter while recording (green bar = the mic hears
you). Drag the pill anywhere; it remembers its position.

녹음 중 pill에 레벨 미터가 표시됩니다(초록 바가 움직이면 마이크가 듣는 중).
pill은 드래그로 옮길 수 있고 위치를 기억합니다.

| pill | meaning |
|------|---------|
| `Ctrl+E` (dim) | idle / 대기 |
| `듣는 중…` + red dot + green meter | recording / 녹음 중 |
| `변환 중…` | finishing / 마무리 중 |
| `✓ text…` | typed / 타이핑 완료 |

---

## Settings / 설정

Press **Ctrl+Shift+E** to open the settings panel. Persisted to
`%APPDATA%/codex-mic/config.json`; hotkey changes apply without restart.
**Ctrl+Shift+E**로 설정을 엽니다. `%APPDATA%/codex-mic/config.json`에 저장되며
핫키 변경은 재시작 없이 적용됩니다.

| Setting | Options | Default |
|---------|---------|---------|
| Dictation hotkey / 받아쓰기 핫키 | any shortcut | `Ctrl+E` |
| Settings hotkey / 설정 핫키 | any shortcut | `Ctrl+Shift+E` |
| Activation / 작동 방식 | push-to-talk, toggle | push-to-talk |
| Silence auto-stop / 무음 자동 종료 | off, 1.5s, 2.5s, 4s | off |
| Injection / 텍스트 입력 방식 | keystrokes, clipboard paste | keystrokes |
| Restore clipboard / 클립보드 복원 | on/off (clipboard mode) | on |
| Trailing space / 뒤에 공백 | on/off | off |
| Language / 언어 | auto, 한국어, English | auto |
| Mic gain / 마이크 게인 | 0, +10, +20, +30 dB | +20 dB |
| Microphone / 마이크 | system default or any input | default |
| Hallucination filter / 환각 필터 | on/off | on |

Notes:

- **Mic gain matters.** Many laptop/USB mics deliver speech at ~-40 dBFS,
  which server VAD never fires on. If nothing gets transcribed, raise the gain;
  if audio distorts, lower it (clipping auto-reduces it too).
  조용한 마이크는 게인이 생명입니다. 전사가 안 되면 게인을 올리세요.
- **Clipboard mode** stages the transcript and sends one Ctrl+V (instant for
  long text), then restores your previous clipboard after ~1s.
- **Toggle mode** exists if you prefer press-once-start / press-once-stop.

---

## Requirements / 요구사항

- Windows 10/11
- [Codex CLI](https://github.com/openai/codex), logged in once with `codex login`
  (codex-mic reads `~/.codex/auth.json` and refreshes it automatically)
- A ChatGPT plan with voice access (verified on Plus)
- To build: Rust 1.77+, CMake on PATH (static libopus build)

---

## Build / 빌드

```powershell
cd src-tauri
cargo build --release
# → src-tauri/target/release/codex-mic.exe
```

CMake 4.x note: the vendored opus sources predate its policy floor; this repo
pins `CMAKE_POLICY_VERSION_MINIMUM=3.5` in `src-tauri/.cargo/config.toml`, so no
manual environment setup is needed.

## Tests / 테스트

```powershell
cd src-tauri
cargo test                                   # unit tests (39)
$env:CODEX_MIC_AUDIO = "1"; cargo test       # + real microphone smoke

# Live end-to-end: real call, real transcription of a speech file
$env:CODEX_MIC_INTEGRATION = "1"
$env:CODEX_MIC_TEST_PCM = "C:\path\to\speech48k.pcm"   # 48kHz mono PCM16LE
cargo test realtime_oauth -- --nocapture
```

Debug utilities (actual signal measurement, device enumeration):

```powershell
cargo run --example devices    # list input devices + the system default
cargo run --example micprobe   # 6s capture on every device, prints RMS/peak
```

---

## How it works / 작동 방식

```
hold Ctrl+E → cpal mic capture (device rate → 48 kHz mono)
            → AGC gain (+20 dB default, clip-guard)
            → libopus (static) 20 ms frames
            → WebRTC audio track
                call created via:
                POST https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas
                Authorization: Bearer <codex OAuth access_token>
                ChatGPT-Account-ID: <account_id>
                originator: codex_chatgpt_desktop
                openai-alpha: quicksilver=v2
                { sdp, session: { model: "gpt-live-1-boulder-alpha",
                    instructions, audio: { output: { voice: "cove" } },
                    delegation: { type: "client" } } }
            ← oai-events data channel: input_transcript.added / turn.done / error
            → sanitize (never auto-Enter) → enigo keystrokes (or clipboard paste)
```

Sessions idle for ~90 s are recreated on the next dictation (the server stops
sending events on stale sessions), and the mic buffers during the reconnect so
your first words survive.

```
src-tauri/src/
  auth.rs      — codex auth.json loader, JWT expiry, OAuth refresh
  realtime.rs  — webrtc-rs peer, call-create, Opus wrapper, frameless event map
  audio.rs     — cpal capture, downmix, resample, AGC gain
  dictate.rs   — transcript accumulator, sanitizer, hallucination filter, injection
  config.rs    — persistent settings
  g711.rs      — μ-law (kept from the PCMU probe; not on the active path)
  commands.rs  — Tauri commands
  lib.rs       — hotkeys (PTT/toggle), PCM pump, session lifecycle
ui/            — static pill + settings panel (no audio code)
examples/      — devices.rs, micprobe.rs (signal debugging)
```

---

## Adjust it to your needs / 입맛에 맞게 고치기

This is a reference implementation, not a product. Things people typically
change first — 이 코드는 제품이 아니라 레퍼런스입니다. 보통 이런 걸 먼저 바꿉니다:

- **Instructions** (transcription style, language bias): `realtime.rs::instructions()`
- **Voice / model**: `realtime.rs` `MODEL`, session `audio.output.voice`
- **Session reuse window**: `CODEX_MIC_SESSION_STALE_SECS` (default 90)
- **Gain**: settings panel or `mic_gain_db` in config
- **Korean↔English behavior**: `language` setting, or edit the prompt directly
- Want a different transport or your own orchestration? The protocol is fully
  documented in [REVERSE-ENGINEERING.md](REVERSE-ENGINEERING.md) — take the
  tables, skip the code.

## Not affiliated / 면책

Unofficial research code. "OpenAI", "ChatGPT", and "Codex" are trademarks of
OpenAI; this project is not endorsed by or affiliated with OpenAI. Protocol
details were learned from the Apache-2.0-licensed
[openai/codex](https://github.com/openai/codex) source and observation of
first-party client behavior on the author's own account, for interoperability
research.

## Acknowledgments / 참고

- [opencodex](https://github.com/lidge-jun/opencodex) — first public GPT-Live relay notes
- [openai/codex](https://github.com/openai/codex) — codex-rs source (protocol ground truth)
- [VoiceInk](https://tryvoiceink.com/), [Wispr Flow](https://wisprflow.ai/), [Superwhisper](https://superwhisper.com/) — settings/UX patterns
- [webrtc.rs](https://github.com/webrtc-rs/webrtc), [audiopus_sys](https://github.com/lakelezz/audiopus_sys) — Rust WebRTC/Opus stack

## License

MIT
