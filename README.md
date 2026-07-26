# Codex Mic

**OpenAI 실시간 음성 API로 받아쓰는 윈도우 받아쓰기 도구. 인증은 Codex CLI
로그인을 그대로 쓴다. API 키도, 구독도, 새로 만들 계정도 없다.**

API 키로 붙는 공개 realtime 엔드포인트가 아니라, Codex 데스크톱 앱이 쓰는 ChatGPT
백엔드 쪽에 `codex login`이 남겨둔 OAuth 토큰으로 붙는다. 키가 필요 없는 이유가
이거다.

마이크는 cpal로 잡아 Rust에서 Opus로 인코딩한 뒤 WebRTC 트랙으로 바로 나간다.
오디오는 webview를 거치지 않고, 전사는 데이터 채널로 스트리밍되어 돌아온다.

`Ctrl+E`를 누른 채로 말하고 손을 떼면 커서가 있는 곳에 받아쓴다.

[English](README.en.md)

<p align="center">
  <img src="docs/screenshot-pill.png" alt="pill" />
  <img src="docs/screenshot-settings.png" alt="설정 패널" width="300" />
</p>

> OpenAI와 무관한 비공식 프로젝트다. 실시간 프로토콜은 리버스 엔지니어링한 것이다.
> 각자 책임 하에 쓰고 고쳐라.

## 로그인

`codex login`이 `~/.codex/auth.json`(또는 `$CODEX_HOME/auth.json`)에 넣어둔 액세스
토큰, 리프레시 토큰, 계정 ID를 읽어 쓴다. 인증은 이게 전부다.

액세스 토큰은 며칠이면 만료되는 JWT다. 만료 2분 전에 앱이 직접
`auth.openai.com/oauth/token`으로 리프레시 토큰을 교환하고 결과를 `auth.json`에
다시 쓴다. 받아쓰기 하려고 다시 로그인할 일은 없고, 앱이 따로 저장하는 자격 증명도
없다.

## 설치

[Releases](../../releases/latest)에서 exe를 받아 실행한다. 설치 과정 없는 단독 실행
파일이다. 처음 한 번은 `codex login`이 되어 있어야 한다.

서명하지 않은 바이너리라 SmartScreen이 막는다. `추가 정보` → `실행`.

## 쓰는 법

1. 받아쓸 곳을 클릭한다. 터미널, 에디터, 채팅창 어디든 상관없다.
2. `Ctrl+E`를 누른 채로 말한다. 키를 누르는 순간 pill이 반응한다.
3. 손을 뗀다. 커서 위치에 타이핑된다.

| pill | 상태 |
|------|------|
| `Ctrl+E` (흐림) | 대기. 마우스를 올리면 진해진다 |
| `듣는 중…` + 빨간 점 + 레벨 미터 | 녹음 중 |
| `변환 중…` | 남은 오디오를 보내고 최종 전사를 기다리는 중 |
| `✓ 텍스트…` | 타이핑 완료 |
| `⚠ …` | 오류. 메시지에 이유가 적힌다 |

pill은 키보드 포커스를 가져가지 않는다. 드래그해서 옮겨도 받아쓰던 창의 포커스가
풀리지 않고, 옮긴 위치는 저장된다. 줄바꿈과 탭은 제거하고 넣으므로 받아쓴 내용
때문에 채팅이나 프롬프트가 실수로 전송되는 일은 없다.

## 핫키

전역 등록이라 **앱이 떠 있는 동안 그 키는 다른 앱에 가지 않는다.** `Ctrl+E`가
기본인 건 실수로 눌릴 일이 없어서인데, 대신 셸의 줄 끝 이동과 VS Code 빠른 열기를
가져간다. 거슬리면 설정에서 바꿔라.

단독으로 쓸 수 있는 키(확인함): `F9` `F13` `CapsLock` `` ` `` `Space` `ScrollLock`
`Pause` `NumLock` `Insert` `Home`. 수정자 키 단독은 등록되지 않아서 오른쪽 Alt를
눌러 말하기 같은 건 안 된다.

`CapsLock`을 쓰면 캡스 상태는 앱이 원래대로 돌려놓는다. 조합 키를 써도 수정자가
완전히 떨어진 뒤에 입력하므로 `Ctrl+문자` 단축키가 잘못 발동하지 않는다.

## 설정

`Ctrl+Shift+E`로 연다. `%APPDATA%\codex-mic\config.json`에 저장되고 핫키는 재시작
없이 바로 적용된다.

| 설정 | 값 | 기본 |
|------|-----|------|
| 받아쓰기 핫키 | 아무 키나 조합 | `Ctrl+E` |
| 설정 핫키 | 아무 조합 | `Ctrl+Shift+E` |
| 작동 방식 | 누르는 동안만, 토글 | 누르는 동안만 |
| 무음 자동 종료 | 끔, 1.5초, 2.5초, 4초 | 끔 |
| 텍스트 입력 방식 | 키보드 타이핑, 클립보드 붙여넣기 | 타이핑 |
| 클립보드 복원 | 켬/끔 (클립보드 모드) | 켬 |
| 뒤에 공백 추가 | 켬/끔 | 끔 |
| 언어 | 자동, 한국어, English | 자동 |
| 마이크 게인 | 0, +10, +20, +30 dB | +20 dB |
| 마이크 | 시스템 기본 또는 지정 | 기본 |
| 환각 필터 | 켬/끔 | 켬 |

## 안 될 때

| 증상 | 해결 |
|------|------|
| 아무것도 안 적힌다 | 말하면서 레벨 미터를 봐라. 거의 안 움직이면 마이크 게인을 올린다. 노트북·USB 마이크는 출력이 아주 작아서 서버 음성 감지에 안 걸리는 경우가 흔하다. 전혀 안 움직이면 마이크 장치가 잘못 잡힌 거다 |
| 핫키를 눌러도 반응이 없다 | 다른 앱이 먼저 등록했다. 시작할 때 pill에 등록 실패가 뜬다. 다른 키로 바꿔라 |
| 엉뚱한 창에 적힌다 | 포커스가 있던 곳으로 간다. 받아쓸 창을 먼저 클릭해라. 설정의 `주입 테스트`로 미리 확인할 수 있다 |
| 긴 문장이 느리게 찍힌다 | 텍스트 입력 방식을 클립보드로 바꿔라. `Ctrl+V` 한 번으로 끝나고 1초 뒤에 원래 클립보드를 되돌려준다 |

<details>
<summary><b>빌드 / 테스트</b></summary>

Windows 10/11, Rust 1.77+, PATH에 CMake가 필요하다(libopus를 소스에서 빌드한다).
Windows 11은 WebView2가 기본 탑재고, Windows 10이면
[런타임](https://developer.microsoft.com/microsoft-edge/webview2/)이 필요할 수 있다.

```powershell
cd src-tauri
cargo build --release    # → target/release/codex-mic.exe
cargo test               # 66개. 하드웨어도 네트워크도 필요 없다
```

실제 장비와 서비스를 쓰는 테스트는 켜야 돈다.

```powershell
$env:CODEX_MIC_AUDIO = "1"; cargo test         # 실제 마이크
$env:CODEX_MIC_INTEGRATION = "1"               # 실제 OAuth WebRTC 왕복
$env:CODEX_MIC_TEST_PCM = "speech-48k.pcm"     #   (음성 샘플 필요)
```

신호 디버깅:

```powershell
cargo run --example devices    # 입력 장치 목록과 시스템 기본값
cargo run --example micprobe   # 장치마다 6초 캡처해서 RMS/피크 출력
cargo run --example hotkeys    # 키 문자열이 등록 가능한지 확인
```

`CODEX_MIC_DEBUG_PCM_FILE`에 48kHz mono PCM16LE 파일을 지정하면 마이크 대신 그
파일을 반복 재생해서 흘려보낸다. 캡처 문제인지 세션 문제인지 가를 때 쓴다.

</details>

## 동작

```
누름 → cpal 캡처(48kHz mono) → 게인 → Opus 인코딩 → WebRTC 미디어 트랙
                                                   ↓
                                      chatgpt.com/backend-api/codex/realtime
                                      모델 gpt-live-1-boulder-alpha
                                                   ↓
뗌   → 남은 PCM 전송 → 부분 프레임 flush → 무음 꼬리 → 최종 전사 대기 → 타이핑
```

세션은 `oai-events` 데이터 채널로 이벤트를 돌려준다. 그중 사용자 발화 전사만
골라 쓰고 모델이 말하려는 출력은 버린다. 받아쓰기지 대화가 아니니까.

전사는 음성보다 늦게 도착한다. 키를 뗀 순간 커밋하면 문장 끝이 잘리고 짧은 말은
통째로 비어버린다. 그래서 손을 뗀 뒤에 펌프가 못 읽은 오디오를 마저 보내고, 서버가
턴을 닫을 수 있도록 무음을 조금 붙인 다음, 최종 전사가 올 때까지 기다렸다가
타이핑한다.

녹음을 시작하거나 멈출 수 있는 모든 경로(키 누름과 뗌, 무음 자동 종료, 세션 오류,
"연결 중에 손을 뗌")는 상태 머신 하나를 통과한다. 두 경로가 같은 녹음을 동시에
시작하거나 동시에 멈추는 일이 생기지 않는다.

```
src-tauri/src/
  lib.rs       핫키 처리, PCM 펌프, 녹음 수명주기
  keystate.rs  Idle/Starting/Recording/Stopping 상태 머신
  winkeys.rs   Win32 키보드 상태: 캡스 토글, 수정자 눌림 확인
  realtime.rs  WebRTC 실시간 세션, Opus 인코딩, 전사 이벤트
  audio.rs     cpal 캡처, 다운믹스, 리샘플, 게인
  dictate.rs   전사 누적, 커밋 타이밍, 새니타이즈, 입력
  auth.rs      Codex CLI OAuth 토큰, 갱신
  config.rs    설정 저장
  commands.rs  Tauri 커맨드와 공유 상태
ui/            pill과 설정 패널. 프레임워크 없는 HTML/CSS/JS
examples/      devices.rs, micprobe.rs, hotkeys.rs
```

작은 단일 목적 모듈들이고 테스트가 바로 옆에 붙어 있다. 핫키든 프롬프트든 게인이든
입력 방식이든 마음대로 고쳐 써라.

## 라이선스

MIT
