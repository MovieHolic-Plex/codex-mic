# Codex Mic

**Windows voice dictation that runs on your Codex CLI login. No API key, no
dictation subscription, no new account.**

If `codex login` already works on this machine, there is nothing else to set up.
Codex Mic reads the OAuth tokens the CLI keeps in `~/.codex/auth.json`, refreshes
them itself when they expire, and streams your microphone to the realtime
endpoint the Codex desktop app uses. It stores no credential of its own.

Hold `Ctrl+E`, speak, release. The transcript is typed wherever the keyboard
focus is.

**[한국어 README](README.ko.md)**

<p align="center">
  <img src="docs/screenshot-pill.png" alt="pill" />
  <img src="docs/screenshot-settings.png" alt="settings panel" width="300" />
</p>

> Unofficial project, not affiliated with OpenAI. The realtime protocol was
> reverse-engineered. Use at your own risk.

## The login

`codex login` writes an OAuth access token, a refresh token and an account id to
`~/.codex/auth.json` (or `$CODEX_HOME/auth.json`). Codex Mic reads that file and
sends the access token as a bearer token. That is the whole authentication story.

The access token is a JWT that expires in days. Two minutes before it does, the
app exchanges the refresh token at `auth.openai.com/oauth/token` — the same
request the CLI makes — and writes the result back to `auth.json`. You do not
re-login just to dictate.

The pill tells you at startup if no login is present, without a network probe.

## Install

Download the exe from [Releases](../../releases/latest) and run it. Single
self-contained executable, no installer.

```powershell
codex login    # once, before the first launch
```

It is not code-signed, so SmartScreen warns on first launch: `More info` →
`Run anyway`. Building from source is one command if you would rather not.

## Usage

1. Click where you want the text. Terminal, editor, chat, anything.
2. Hold `Ctrl+E` and speak. The pill reacts the moment the key goes down.
3. Release. The transcript is typed at the cursor.

| pill | state |
|------|-------|
| `Ctrl+E` (dim) | idle. Dims out of the way, brightens on hover |
| `듣는 중…` + red dot + level meter | recording |
| `변환 중…` | draining audio, waiting for the final transcript |
| `✓ text…` | typed |
| `⚠ …` | error, with the reason |

The pill never takes keyboard focus, so clicking or dragging it does not pull
focus from the window you are dictating into. Its position is remembered.

Newlines and tabs are stripped before injection, so a transcript can never
submit the chat box or prompt you are typing into.

## Hotkey

A global hotkey is registered system-wide, so **no other app receives it while
Codex Mic runs.** `Ctrl+E` is the default because it never fires by accident, but
it does take over your shell's move-to-end-of-line and VS Code's quick-open.
Change it in settings if that bothers you.

Single keys that register (verified): `F9` `F13` `CapsLock` `` ` `` `Space`
`ScrollLock` `Pause` `NumLock` `Insert` `Home`.

A bare modifier cannot be registered, so "hold right Alt to talk" is not
possible. If you bind `CapsLock`, the lock state is put back the way it was. Text
is never injected while a modifier is still held, so chord hotkeys never fire
shortcuts in the target app.

```powershell
cd src-tauri; cargo run --example hotkeys    # check any other key string
```

## Settings

`Ctrl+Shift+E`. Saved to `%APPDATA%\codex-mic\config.json`; hotkey changes apply
without a restart. The panel is in Korean.

| Setting | Options | Default |
|---------|---------|---------|
| Dictation hotkey | any key or chord | `Ctrl+E` |
| Settings hotkey | any chord | `Ctrl+Shift+E` |
| Activation | push-to-talk, toggle | push-to-talk |
| Silence auto-stop | off, 1.5s, 2.5s, 4s | off |
| Injection | keystrokes, clipboard paste | keystrokes |
| Restore clipboard | on/off (clipboard mode) | on |
| Trailing space | on/off | off |
| Language | auto, Korean, English | auto |
| Mic gain | 0, +10, +20, +30 dB | +20 dB |
| Microphone | system default or any input | default |
| Hallucination filter | on/off | on |

## When it does not work

**Nothing gets typed.** Watch the level meter while speaking. If the bar barely
moves, raise the mic gain; laptop and USB microphones often deliver speech far
below the level the server's voice detection triggers on. If it does not move at
all, the wrong input device is selected.

**The hotkey does nothing.** Another app registered it first, and the pill shows
a registration error at startup. Pick a different key.

**Text lands in the wrong window.** It goes wherever the keyboard focus was.
Click into the target first. The `주입 테스트` button in settings types sample
text so you can check before dictating for real.

**Long text types slowly.** Switch injection to clipboard paste: one `Ctrl+V`
instead of replaying every keystroke, with your previous clipboard restored a
second later.

## Build and test

Windows 10/11, Rust 1.77+, CMake on `PATH` (libopus is built from source).
Windows 11 ships WebView2; on Windows 10 you may need the
[runtime](https://developer.microsoft.com/microsoft-edge/webview2/).

```powershell
cd src-tauri
cargo build --release    # → target/release/codex-mic.exe
cargo test               # 66 tests, no hardware or network needed
```

Tests that touch real hardware or the live service are opt-in:

```powershell
$env:CODEX_MIC_AUDIO = "1"; cargo test         # real microphone
$env:CODEX_MIC_INTEGRATION = "1"               # live OAuth WebRTC round trip
$env:CODEX_MIC_TEST_PCM = "speech-48k.pcm"     #   (needs a speech sample)
```

Signal debugging:

```powershell
cargo run --example devices    # input devices and the system default
cargo run --example micprobe   # 6s capture per device, prints RMS/peak
```

Point `CODEX_MIC_DEBUG_PCM_FILE` at a 48 kHz mono PCM16LE file and the pump
loops that instead of the microphone. Separates capture bugs from session bugs.

## How it works

```
press   → cpal capture (48 kHz mono) → gain → Opus → WebRTC session
release → send the leftover PCM → flush the partial frame → silence tail
        → wait for the final transcript → type (or Ctrl+V)
```

Transcription lags speech. Committing the moment the key comes up truncates the
end of every sentence and returns nothing at all for short ones. So the release
path hands over the audio the pump had not read yet, appends a beat of silence so
the server can close the turn, and waits for the authoritative transcript before
typing.

Everything that can start or stop a recording — key down and up, silence
auto-stop, session failure, "you let go while we were still connecting" — goes
through one state machine, so two paths can never both start or both stop the
same recording.

```
src-tauri/src/
  lib.rs       hotkey handling, PCM pump, recording lifecycle
  keystate.rs  the Idle/Starting/Recording/Stopping state machine
  winkeys.rs   Win32 keyboard state: caps toggle, modifier-held checks
  realtime.rs  WebRTC realtime session, Opus encode, transcript events
  audio.rs     cpal capture, downmix, resample, gain
  dictate.rs   transcript accumulation, commit timing, sanitize, injection
  auth.rs      Codex CLI OAuth tokens and refresh
  config.rs    persistent settings
  commands.rs  Tauri commands and shared state
ui/            pill and settings panel, plain HTML/CSS/JS
examples/      devices.rs, micprobe.rs, hotkeys.rs
```

Small single-purpose modules with tests next to them. Change the hotkeys, the
prompt, the gain, the injection style — it is all yours.

## License

MIT
