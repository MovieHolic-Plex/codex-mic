# Codex Desktop App — Voice/Dictation Reverse-Engineering

> Research notes for interoperability, produced on the author's own account.
> Unofficial; not affiliated with OpenAI. Use at your own risk.

Findings from reversing `OpenAI.Codex_26.721.3996.0` (`ChatGPT.exe`, Chromium 150 /
Electron) to understand how its voice/dictation feature works, and why a standalone
Tauri tool cannot trivially call the same API.

## 1. App architecture

```
ChatGPT.exe (Chromium 150, MSIX package)
├── app/resources/app.asar          ← renderer + main-process JS (extracted to .senpi/app-extracted/)
│   ├── webview/assets/             ← renderer bundles
│   │   ├── global-dictation-orb-*.js      ← dictation UI + recording logic
│   │   ├── realtime-buffered-audio-worklet-*.js  ← AudioWorklet (mic buffering)
│   │   └── app-initial-*.js (14 MB)       ← qf HTTP client, auth, IPC bridge
│   └── .vite/build/main-D9Ntp4GD.js       ← Electron main process (host)
└── process.resourcesPath/native/<devicecheck>   ← native DeviceCheck module (darwin)
```

Three renderer targets: `app://-/index.html` (main), `?initialRoute=/avatar-overlay`
(dictation orb host), and a `chatgpt.com` webview.

## 2. The REAL dictation protocol (NOT thread/realtime)

`codex app-server`'s `thread/realtime/start` is the **voice-conversation** API
(Codex-CLI-metered → "usage limit"). Dictation uses a **separate streaming API**
extracted from `global-dictation-orb`:

```
POST /codex/dictation-stream-connect-info            → { websocketUrl, protocols }
WS connect(websocketUrl, protocols)
→ send { type:"session.start", config:{
       input_audio_format:"pcm16", sample_rate_hz:<ctx.sampleRate>, num_channels:1,
       max_buffer_size_bytes:4194304, max_utterance_duration_ms:30000,
       session_ttl_ms:300000, provider_mode:"streaming_sse",
       transcript_delivery_mode:"final_only",
       vad:{ type:"server_vad", threshold:0.5, prefix_padding_ms:300, silence_duration_ms:500 } } }
→ stream { type:"audio.append", audio:<base64 pcm16> }   (pcm16 from getUserMedia→ScriptProcessor(2048,1,1))
← session.started / session.updated / transcript.done / transcript.failed / session.error
```

Audio capture in the app: `getUserMedia` → `AudioContext` → `ScriptProcessorNode` →
Float32→Int16 (`Gnt`) → base64 (`jh`). Two modes: streaming (connect-info WS) and
batch (`l(audioBlob)` → POST audio → transcript).

## 3. Endpoint routing — the `__codex-api` host proxy

The renderer's `qf.getInstance().post('/codex/dictation-stream-connect-info')` does
**not** hit `chatgpt.com/backend-api/...` directly. A rewrite transforms
`/backend-api/X` → `/__codex-api/X` (slice 12), and the request is proxied to the
**host** (Electron main) via a `fetch-stream` IPC:

```
renderer: dispatchMessage('fetch-stream', { requestId, url, method, headers, body, format })
  → electronBridge.sendMessageFromView(...)
host: intercepts /__codex-api/* , strips to /backend-api/*, attaches auth, forwards to chatgpt.com
  ← fetch-stream-response / fetch-stream-event / fetch-stream-complete / fetch-stream-error
```

Confirmed: dispatching `fetch-stream` with the `__codex-api` URL returns **HTTP 200**
from the host (the host authenticates). A direct `/backend-api/codex/...` call with
the codex CLI bearer returns **404** (wrong audience).

## 4. Auth + attestation (the standalone barrier)

Request headers the app attaches (`Lh()` / `mba()`):
- `Authorization: Bearer <accessToken>` — `accessToken` from `/api/auth/session`
  (chatgpt.com **web** session cookie), **not** the codex CLI OAuth token (auth.json).
  The codex CLI token authenticates `/backend-api/me` (200) but dictation (404).
- `ChatGPT-Account-ID: <account_id>`
- `originator: <Non>` (desktop originator)
- `X-OpenAI-Attach-Auth: 1` — for `__codex-api` requests
- `X-OpenAI-Attach-Integrity-State: 1`
- `X-OpenAI-Attach-Desktop-Surface: 1`
- `x-sentinel-dc: {"token":"..."}` — **DeviceCheck attestation token**

### DeviceCheck (`main-D9Ntp4GD.js`)
- Native module loaded from `process.resourcesPath/native/<devicecheck>` via `Ne({resourcesPath})`.
- `generateToken()` → native `{supported, tokenBase64, latencyMs}`; null if unsupported.
- **darwin-only**: `if (platform !== 'darwin') return error(1)`. On Windows it cannot
  generate a token.
- `attachToken()` throws `'DeviceCheck token generation is unavailable'` when null.
- Opt-in via `attachDeviceCheckToken` flag (default false); flow `d && attachToken(headers)`.
- Registration endpoint `/devicecheck` sets a cookie (`ensureCookie`).

**Implication:** the `x-sentinel-dc` DeviceCheck token is macOS-only. Forcing
`X-OpenAI-Attach-Auth:1` on Windows yields the "DeviceCheck unavailable" error. The
Windows dictation path therefore either omits the token or uses a different surface —
this is the remaining unknown (requires capturing the app's actual Windows request).

## 5. Originator classification (codex source, `login/src/auth/default_client.rs`)
- `is_first_party_originator`: `codex_cli_rs`, `codex-tui`, `codex_vscode`, `Codex *`
- `is_first_party_chat_originator`: `codex_atlas`, `codex_chatgpt_desktop`
- Default originator `codex_cli_rs`; overridable via `CODEX_INTERNAL_ORIGINATOR_OVERRIDE`.
- Override did **not** bypass the Codex-CLI usage gate on `thread/realtime/start`.

## 6. Cookie decryption (blocked)
Codex Desktop's Chromium v150 cookies are v10-prefixed but **do not AES-GCM
authenticate** with the DPAPI-derived master key (all modes fail: GCM tag@end,
tag@start, CBC, CTR). Non-standard key derivation in this build. The session cookie
is also absent (Desktop uses `oai-sc` + CF cookies, not NextAuth `session-token`).

## 7. Capture infrastructure (live, port 9223)
Desktop app relaunched with `--remote-debugging-port=9223`. Persistent hooks installed
on both `app://` pages wrapping `electronBridge.sendMessageFromView` to capture any
outgoing `fetch-stream` to `dictation-stream-connect-info` (URL + headers + body) and
incoming responses. `dictation-poc.cjs` is ready to consume a `websocketUrl`.

## What remains (needs the app's own attested call)
To fully close the loop, capture the app's **real** connect-info request+response by
triggering dictation in the Desktop app (its hotkey or an in-app mic/dictate button).
The hook at `window.__all` / `window.__dicCap` will record the exact URL, the
attestation headers that pass, and the `websocketUrl`. Then:
```
DICTATION_WS_URL=<captured> node .senpi/dictation-poc.cjs        # handshake proof
DICTATION_WS_URL=<captured> node .senpi/dictation-poc.cjs x.wav  # audio + transcript
```

## 8. WORKING VOICE PROTOCOL — codex CLI bearer (cracked via opencodex live.ts)

Reference: `github.com/lidge-jun/opencodex` → `src/server/live.ts` (issue #371),
which relays the Codex App / ChatGPT voice call (GPT-Live / Frameless Bidi).

**The codex CLI bearer (`~/.codex/auth.json` `access_token`) DOES authorize voice**
when posted directly to the backend-api call-create — confirmed live:

```
POST https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas
Headers:
  Authorization: Bearer <codex access_token>
  ChatGPT-Account-ID: <tokens.account_id>
  originator: codex_chatgpt_desktop
  openai-alpha: quicksilver=v2          (Frameless; quicksilver=v1 for legacy)
  Content-Type: application/json
Body (backend JSON shape):
  { "sdp": "<WebRTC SDP offer>",
    "session": {                      // model is REJECTED — server picks it
      "voice": "cove",
      "input_audio_format": "pcm16",
      "output_audio_format": "pcm16",
      "turn_detection": { "type":"server_vad", "threshold":0.5,
                           "prefix_padding_ms":300, "silence_duration_ms":500 }
    } }
→ 200 + SDP answer body + Location: /.../rtc_<callId>   (call_id)
```

Live probe result on this account: protocol ACCEPTED (auth + session validated),
returns `429 usage_limit_reached { plan_type:"go", resets_at:1787397172 }` — a
**plan-quota wall, not a protocol error**. Go-plan voice quota resets ~Aug 19 2026.

### Why my earlier app-server path failed
`thread/realtime/start` via `codex app-server` builds a Codex **coding** thread +
realtime session (with Codex instructions/startup context) → metered as Codex
agent usage → "You've hit your usage limit. Upgrade to Plus." The DIRECT
`/realtime/calls` POST above carries NO Codex coding context → metered as
ChatGPT **voice** quota instead. opencodex bypasses the app-server for exactly
this reason.

### Sideband WebSocket (audio/events transport)
Per opencodex `buildLiveSidebandUpstreamWsUrl`: chatgpt.com/backend-api rejects
WS upgrades pre-101, so the sideband joins on the **public API host** with the
**same bearer unchanged**:
- Frameless: `wss://api.openai.com/v1/live/<callId>`
- v1: `wss://api.openai.com/v1/realtime?intent=quicksilver&call_id=<callId>`

Audio itself flows over the WebRTC peer connection (the SDP negotiated above);
the sideband WS carries realtime events (transcript deltas, function calls,
output audio for non-WebRTC).

### Relayed client-protocol headers (opencodex `LIVE_CLIENT_PROTOCOL_HEADERS`)
`openai-alpha`, `x-session-id`, `session-id`, `thread-id`, `originator`,
`x-oai-attestation`. `authorization` + `chatgpt-account-id` are always
proxy-owned.

## Bottom line for a standalone tool
- **Voice protocol is fully reversed and works with the codex CLI bearer** —
  no DeviceCheck, no Desktop-app session needed. The only wall is the account's
  Go-plan voice quota (resets ~Aug 19; or upgrade).
- This is the path to use for the dictation tool: capture mic (existing
  `audio.rs` cpal), feed the WebRTC peer connection, POST `/realtime/calls` with
  the codex bearer, read transcripts from the sideband WS.
- The earlier dictation-orb path (`/codex/dictation-stream-connect-info`) is a
  SEPARATE feature (DeviceCheck-attested, macOS-only token) and is NOT the way.

## 9. END-TO-END PROOF (2026-07-25, Plus plan) — CRACKED

Account upgraded Go → Plus (JWT `chatgpt_plan_type: plus`); the 429 quota wall
is gone. Full E2E verified live from `.senpi/voice_e2e_proof.mjs`:

1. `CALL_CREATE: 201` — call `rtc_u2_...` created with ONLY the codex CLI bearer
2. `WEBRTC: connected` — DTLS/SRTP peer connection up (audio track + `oai-events` datachannel)
3. `[DC] {"type":"session.started", ...}` — events flow on the datachannel
4. `SIDEBAND_WS: open` — `wss://api.openai.com/v1/live/<callId>` accepts the same bearer

### The exact session shape (ground truth: codex `frameless_session_json`)

`codex-rs/codex-api/src/endpoint/realtime_websocket/methods_frameless_bidi.rs::session_json`:

```json
{ "sdp": "<WebRTC offer>",
  "session": {
    "model": "gpt-live-1-boulder-alpha",
    "instructions": "",
    "audio": { "output": { "voice": "cove" } },
    "delegation": { "type": "client" } } }
```
Headers: `Authorization: Bearer <codex access_token>`, `ChatGPT-Account-ID`,
`originator: codex_chatgpt_desktop`, `openai-alpha: quicksilver=v2`.

### Probe matrix that led here (all against the live endpoint)

| shape | result |
|---|---|
| top-level `voice` | 400 AVAS `unknown_parameter: 'voice'` — voice belongs at `audio.output.voice` |
| no `model` | 400 `Field session.model is not allowed` (misleading union-validator error; model IS required) |
| `model` + no `audio.output.voice` | 403 `Voice session access denied` |
| `type: "quicksilver"` field | do NOT send — only causes validator noise |
| `openai-alpha: quicksilver=v1` | 403 `Voice session access denied` (v1 = attestation-gated) |
| **full frameless shape + v2 alpha** | **201 + SDP answer** |

`version: v2` (websocket realtime) is rejected for AVAS WebRTC client-side in codex
(`validate_avas_webrtc_start`: v1 or v3 only; v3 = FramelessBidi = `quicksilver=v2` header).

### Remaining for the dictation tool
- Encode mic PCM (cpal → 48k mono) to **Opus** and send on the WebRTC audio track
  (SDP negotiates payload 96/opus). Rust: `webrtc` + `opus` crates.
- Transcripts/events arrive on the `oai-events` datachannel AND the sideband WS
  (`wss://api.openai.com/v1/live/<callId>`, same bearer).
- Calls self-expire (`session.started.expires_at`); close the peer connection to hang up.

## 10. PCMU IS A TRAP — use Opus (2026-07-25, implementation evidence)

The SDP answer lists PCMU (payload 0) next to Opus, and a PCMU-only offer IS
accepted (`m=audio ... 0` both ways, ICE connects, events flow). But every
PCMU-streamed utterance came back transcribed as gibberish ("little pontiff",
Korean noise words) while the client chain was verified byte-perfect:

- μ-law encoder: 0/68186 byte mismatches vs an independent implementation
- PCM content: clean speech energy contour
- werift loopback: 10/10 RTP payloads byte-identical

Conclusion: the server-side PCMU path for gpt-live is broken or vestigial.
The desktop app always negotiates Opus. Do not ship PCMU.

WS-only audio is also dead: the frameless sideband rejects `input_audio.append`
(only session.update/context.append/delegation.*/session.close allowed), and
`POST /v1/live` accepts only `application/sdp` bodies. Audio MUST ride the
WebRTC track.

## 11. SHIPPED (2026-07-25) — codex-mic on OAuth + Opus

The Tauri tool now runs this exact path in Rust:
`cpal → 48kHz mono → libopus (audiopus_sys static) → webrtc-rs PCMU-less offer
→ call-create with codex bearer → oai-events datachannel → dictate pipeline`.

Live E2E proof (`cargo test realtime_oauth` with a 48kHz SAPI speech file):

```
[e2e] transcript: "Hello. This is AJ microphone test The quick brown fox jumps over the"
```

(SAPI said "a microphone test"; the rest is verbatim.) Token refresh
(`auth.openai.com/oauth/token`, codex CLIENT_ID) is implemented, so the tool
runs unattended. Build gotchas: rustls needs an explicit ring CryptoProvider
(webrtc+reqwest pull both), cmake 4.x needs `CMAKE_POLICY_VERSION_MINIMUM=3.5`
for the vendored opus sources.

### Realtime latency, measured (same E2E, timestamped collector)

Transcript deltas stream WHILE audio is still being sent — first delta at
1460ms into an 8524ms stream, then a continuous ~200ms cadence:

```
1460ms [DURING-STREAM] " Hello"
2456ms [DURING-STREAM] " 디스"
2909ms [DURING-STREAM] " 이즈"
3138ms [DURING-STREAM] " 에"
```

So yes: streaming transcription, ~1-1.5s behind speech. Note the deltas above
transliterate English into Hangul ("디스 이즈 에") — the model's script bias on
Korean accounts is real and nondeterministic; mitigated by the
script-preservation clause now in the session instructions and the
auto/Korean/English language setting.
