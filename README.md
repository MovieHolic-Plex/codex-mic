# Codex Mic / 코덱스 마이크

**Hold Ctrl+E, speak, release — your words are typed wherever the keyboard focus is.**
**Ctrl+E를 누르고 말한 뒤 손을 떼면, 키보드 포커스가 있는 곳에 그대로 타이핑됩니다.**

A native Windows voice dictation tool (Tauri + Rust). Signs in with your
**Codex CLI login** — no API key needed.
네이티브 윈도우 음성 받아쓰기 도구(Tauri + Rust)입니다. **Codex CLI 로그인만으로**
동작하며 API 키가 필요 없습니다.

<p align="center">
  <img src="docs/screenshot-pill.png" alt="Codex Mic pill" />
  <img src="docs/screenshot-settings.png" alt="Settings panel" width="320" />
</p>

> **Unofficial project / 비공식 프로젝트.** Not affiliated with OpenAI.
> Provided as-is for personal use and study — adjust it to your needs and use
> it at your own risk. OpenAI와 무관한 비공식 프로젝트입니다. 각자 책임 하에
> 자유롭게 고쳐 쓰세요.

---

## Features / 기능

| Feature | Description |
|---------|-------------|
| Push-to-talk | Hold the hotkey, speak, release — done. Toggle mode available too. |
| Types anywhere | Text lands wherever the keyboard focus is — terminals, editors, browsers |
| Live level meter | The pill shows your mic level while recording, so you know it's hearing you |
| Draggable pill | Click-through-free, focus-stealing-free; drag it anywhere, it remembers |
| Settings panel | Ctrl+Shift+E — hotkeys, mic, gain, language, paste mode, auto-stop |
| Quiet-mic boost | Built-in AGC gain for faint laptop/USB microphones |
| Korean + English | Auto-detects language; 한영혼용 works |
| Never auto-Enter | Newlines/tabs are stripped — it never submits your chat by accident |

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

| pill | meaning |
|------|---------|
| `Ctrl+E` (dim) | idle / 대기 |
| `듣는 중…` + red dot + green meter | recording / 녹음 중 |
| `변환 중…` | finishing / 마무리 중 |
| `✓ text…` | typed / 타이핑 완료 |

---

## Settings / 설정

Press **Ctrl+Shift+E**. Saved to `%APPDATA%/codex-mic/config.json`;
hotkey changes apply instantly. **Ctrl+Shift+E**로 엽니다.
`%APPDATA%/codex-mic/config.json`에 저장되며 핫키는 즉시 적용됩니다.

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

Tips:

- **Nothing gets typed?** Check the level meter while speaking. If the bar
  barely moves, raise **Mic gain** — many laptop/USB mics are very quiet by
  default. 아무것도 안 적히면 녹음 중 레벨 미터를 보세요. 바가 안 움직이면
  마이크 게인을 올리세요.
- **Clipboard mode** pastes via Ctrl+V (instant for long text) and restores
  your previous clipboard after ~1s.
- The **주입 테스트** button in settings types sample text into the focused
  app, so you can verify injection works before dictating for real.

---

## Requirements / 요구사항

- Windows 10/11
- [Codex CLI](https://github.com/openai/codex), signed in once with `codex login`
- To build: Rust 1.77+, CMake on PATH

## Build / 빌드

```powershell
cd src-tauri
cargo build --release
# → src-tauri/target/release/codex-mic.exe
```

## Tests / 테스트

```powershell
cd src-tauri
cargo test                                   # unit tests
$env:CODEX_MIC_AUDIO = "1"; cargo test       # + real microphone smoke
```

Debug helpers:

```powershell
cargo run --example devices    # list input devices + the system default
cargo run --example micprobe   # 6s capture on every device, prints RMS/peak
```

---

## How it works / 구조

```
hold Ctrl+E → cpal mic capture (→ 48 kHz mono) → AGC gain
            → Opus encode (static libopus) → WebRTC realtime session
            → live transcript events → sanitize → enigo keystrokes (or Ctrl+V)
```

```
src-tauri/src/
  auth.rs      — Codex CLI login handling, token refresh
  realtime.rs  — realtime session, Opus, transcript events
  audio.rs     — cpal capture, downmix, resample, gain
  dictate.rs   — transcript accumulator, sanitizer, injection
  config.rs    — persistent settings
  commands.rs  — Tauri commands
  lib.rs       — hotkeys, PCM pump, session lifecycle
ui/            — pill + settings panel
examples/      — devices.rs, micprobe.rs (signal debugging)
```

Tweak freely — hotkeys, prompts, gain, language behavior, injection style:
it's all small, single-purpose modules. 자유롭게 고쳐 쓰세요.

## License

MIT
