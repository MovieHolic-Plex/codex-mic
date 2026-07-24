# Codex Mic

**Ctrl+E를 누르면 말한 글이 그대로 타이핑되는 받아쓰기(dictation) 도구.** Orca의 voice처럼, 키보드 대신 음성으로 아무 창에나 입력. ChatGPT OAuth 그대로 사용 — API key 필요 없음.

## 작동 방식

1. 앱이 백그라운드에서 codex app-server에 자동 연결 (ChatGPT OAuth).
2. 아무 곳에서나 **Ctrl+E** 누르면 → 마이크 녹음 시작, 작은 pill 표시등이 화면 위에 나타남 ("듣는 중…").
3. 말하면 실시간 전사가 pill에 표시됨 (한글/영어 자동 감지).
4. 다시 **Ctrl+E** 누르면 → 녹음 종료, 전사된 텍스트가 커서 위치에 그대로 타이핑됨.

codex의 실험용 `thread/realtime` API를 WebRTC 텍스트 모드로 구동합니다:

```
Ctrl+E → getUserMedia(mic) → RTCPeerConnection → SDP offer
       → thread/realtime/start {transport:{type:"webrtc",sdp}, outputModality:"text", clientManagedHandoffs:true}
       ← thread/realtime/transcript/delta (role:"user")  ← 실시간 전사 누적
Ctrl+E → enigo.text(누적된 텍스트)  ← 커서 위치에 타이핑
```

## 요구사항

- Codex CLI 설치 + ChatGPT 로그인 (`codex` on PATH, 또는 `C:\Users\<you>\.codex\packages\standalone\current\bin\codex.exe`에 자동 감지).
- Rust 1.77+.

## 실행

```sh
cd src-tauri
cargo tauri dev      # 개발
cargo tauri build    # 릴리스 exe → target/release/codex-mic.exe
```

빌드 후 `codex-mic.exe`를 실행하면 백그라운드에서 돌며 Ctrl+E로 받아쓰기.

## 테스트

```sh
cd src-tauri
cargo test                          # 단위 테스트 (JSON-RPC 프레이밍 + 전사 role 필터)
CODEX_MIC_INTEGRATION=1 cargo test  # 실제 codex 바이너리 통합 테스트 포함
```

## 구조

```
src-tauri/src/
  jsonrpc.rs   — stdio JSON-RPC 클라이언트 + 프레이밍 (단위 테스트)
  codex.rs     — 세션: app-server 스폰 → initialize → thread → realtime_start/stop
  dictate.rs   — user-role 전사 누적 + enigo 키보드 주입 (단위 테스트)
  commands.rs  — Tauri #[command] (realtime_start/stop, dictate_start/stop, buffer, status)
  lib.rs       — Ctrl+E 글로벌 단축키 + 자동 연결 + 통합 테스트
ui/            — 정적 웹뷰: 작은 pill 하나 (상태 표시등 + 실시간 전사 미리보기)
```

## 설정

단축키는 `src/lib.rs`의 `HOTKEY` 상수로 변경 가능 (`"Ctrl+E"`, `"Alt+Space"`, 등 — Tauri shortcut 문법).
