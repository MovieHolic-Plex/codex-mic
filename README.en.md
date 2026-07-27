<div align="center">

# 🎙️ Codex Mic

### Speak, and it lands at your cursor

Hold `Ctrl+E`, talk, let go. That's it.<br>
**No API key, no subscription, no new account.**

[![Windows](https://img.shields.io/badge/Windows-10%20%7C%2011-0078D4?style=for-the-badge&logo=windows&logoColor=white)](#install)
[![Rust](https://img.shields.io/badge/Rust-Tauri%202-CE422B?style=for-the-badge&logo=rust&logoColor=white)](#how-it-works)
[![API Key](https://img.shields.io/badge/API%20key-not%20needed-2ea44f?style=for-the-badge)](#why-no-api-key)

[![release](https://img.shields.io/github/v/release/MovieHolic-Plex/codex-mic?color=7ef0c0&label=latest)](https://github.com/MovieHolic-Plex/codex-mic/releases/latest)
[![downloads](https://img.shields.io/github/downloads/MovieHolic-Plex/codex-mic/total?color=7ef0c0)](https://github.com/MovieHolic-Plex/codex-mic/releases)
[![license](https://img.shields.io/github/license/MovieHolic-Plex/codex-mic?color=lightgrey)](LICENSE)

**[⬇️ Download](https://github.com/MovieHolic-Plex/codex-mic/releases/latest)** · [한국어](README.md)

<br>

<img src="docs/screenshot-pill.png" alt="pill overlay" /><br><br>
<img src="docs/screenshot-settings.png" alt="settings panel" width="320" />

</div>

<br>

> [!CAUTION]
> **Use this and get screwed, that's on you.**
>
> An unofficial project, unaffiliated with OpenAI. The realtime protocol was reverse engineered; it can break or change without warning. No guarantees about anything that happens to your account. You're on your own.

---

## Why no API key

This is not the public realtime endpoint you reach with an API key. It connects to the **ChatGPT backend the Codex desktop app uses**, with the OAuth token `codex login` left behind.

```bash
codex login    # skip if you already have
```

It reads the tokens from `~/.codex/auth.json`, refreshes them when they expire, and stores no credentials of its own.

## Install

Grab `codex-mic.exe` from [**Releases**](https://github.com/MovieHolic-Plex/codex-mic/releases/latest) and run it. Single file, no installer.

> [!TIP]
> The binary is unsigned, so SmartScreen will block it → `More info` → `Run anyway`

<details>
<summary><b>Build it yourself</b></summary>

```bash
git clone https://github.com/MovieHolic-Plex/codex-mic
cd codex-mic/src-tauri
cargo build --release        # → target/release/codex-mic.exe
cargo test                   # no hardware, no network
```

You need the Rust toolchain and WebView2. The frontend is static files, so there is no separate build step.
</details>

## Using it

<div align="center">

| | |
|:--|:--|
| 🎤 **Hold** `Ctrl+E` **and speak** | Release, and it types at the cursor |
| ⚙️ `Ctrl+Shift+E` | Settings panel |

</div>

The pill **never takes keyboard focus.** Drag it anywhere and the window you were dictating into stays active; the position is remembered.

> [!IMPORTANT]
> **Enter is never typed.** Newlines in a transcript become spaces — no accidental sends in a chat box, terminal, or prompt.

## Settings

<div align="center">

| Setting | Default |
|:---|:---|
| **Activation** | Hold to record |
| **Injection** | Simulated keystrokes |
| **Language** | Auto-detect |
| **Transcription model** | Pick from a measured list |
| **Session model** | Realtime session flavour |
| **Mic gain** | +20 dB |
| **Hallucination filter** | On |

</div>

Stored in `%APPDATA%\codex-mic\config.json`. Hotkeys and models apply **without a restart.**

## Troubleshooting

<details>
<summary><b>Empty transcripts</b></summary>

Watch the level meter while you speak. Barely moving means the gain is too low — laptop and USB microphones are often very quiet. Not moving at all means the wrong capture device is selected.
</details>

<details>
<summary><b>The hotkey does nothing</b></summary>

Another app registered it first; the pill reports the failure at startup. Pick a different key in settings.

Keys confirmed to work on their own: `F9` `F13` `CapsLock` `` ` `` `Space` `ScrollLock` `Pause` `NumLock` `Insert` `Home`
</details>

<details>
<summary><b>"codex login required"</b></summary>

Run `codex login` again. This appears when the refresh token, not just the access token, has been revoked server-side.
</details>

<details>
<summary><b>English words come back in Hangul</b></summary>

`endpoint` → `엔드포인트` is a property shared by every transcription model here. Try another one in settings — each carries a note from an actual measurement.
</details>

<details>
<summary><b>Logs and audio dump</b></summary>

```powershell
$env:CODEX_MIC_DEBUG_TRANSCRIPT=1                    # log transcript text
$env:CODEX_MIC_DEBUG_DUMP_WAV="$env:TEMP\sent.wav"   # dump the audio sent
.\codex-mic.exe
```

`sent.wav` holds exactly what went over the wire. **Listening to it settles instantly whether the problem is the microphone or the model.**
</details>

## How it works

```
Ctrl+E down ──▶ cpal capture 48kHz ──▶ 24kHz mono PCM16 ──▶ WebSocket stream
                                                                 │
Ctrl+E up   ──▶ 2s silence padding ──▶ commit ───────────▶ realtime session
                                                                 │
                                                        transcription (ASR)
                                                                 ▼
                                                   sanitize ──▶ typed at cursor
```

A few design decisions, **all settled by measurement:**

- **Audio never touches the webview.** cpal captures it and Rust sends it straight out.
- **Server VAD is off.** The turn ends when the key comes up. Left to VAD, every pause between sentences splits the turn and the transcript arrives in fragments.
- **Two seconds of silence on each side.** A recording that starts the instant speech does loses its first syllable to the model's own windowing.
- **The ASR side channel does the transcribing.** The session model is generative and, once the conversation carries a few turns, **invents speech that was never spoken** — measured, and disabled by default because of it.

<details>
<summary><b>Environment variables</b> — experiment without rebuilding</summary>

| Variable | Purpose |
|:---|:---|
| `CODEX_MIC_TRANSCRIBE_MODEL` | Transcription model (overrides settings) |
| `CODEX_MIC_REALTIME_MODEL` | Session model |
| `CODEX_MIC_SERVER_VAD=1` | Restore server VAD |
| `CODEX_MIC_MODEL_TRANSCRIBE=1` | Transcribe with the session model (not recommended) |
| `CODEX_MIC_DEBUG_TRANSCRIPT=1` | Log transcript text |
| `CODEX_MIC_DEBUG_DUMP_WAV` | Dump sent audio to WAV |
| `CODEX_MIC_DEBUG_PCM_FILE` | Feed a PCM file instead of the microphone |

</details>

<details>
<summary><b>Layout</b></summary>

```
src-tauri/src/
  lib.rs       hotkey handling, PCM pump, recording lifecycle
  keystate.rs  Idle/Starting/Recording/Stopping state machine
  winkeys.rs   Win32 keyboard state — caps toggle, physical key checks
  realtime.rs  realtime WebSocket session, transcript events
  audio.rs     cpal capture, downmix, resample, gain
  dictate.rs   transcript accumulation, commit timing, sanitize, injection
  auth.rs      Codex CLI OAuth tokens and refresh
  config.rs    settings persistence
  commands.rs  Tauri commands and shared state
ui/            pill and settings panel — framework-free HTML/CSS/JS
```

Small single-purpose modules with their tests next to them.
</details>

---

<div align="center">

**[MIT](LICENSE)**

</div>
