<div align="center">

# 🎙️ Codex Mic

### 말하면, 커서에 꽂힌다

`Ctrl+E`를 누른 채 말하고 손을 떼면 끝.<br>
**API 키도, 구독도, 새 계정도 필요 없다.**

[![Windows](https://img.shields.io/badge/Windows-10%20%7C%2011-0078D4?style=for-the-badge&logo=windows&logoColor=white)](#설치)
[![Rust](https://img.shields.io/badge/Rust-Tauri%202-CE422B?style=for-the-badge&logo=rust&logoColor=white)](#동작-방식)
[![API Key](https://img.shields.io/badge/API%20키-불필요-2ea44f?style=for-the-badge)](#왜-api-키가-필요-없나)

[![release](https://img.shields.io/github/v/release/MovieHolic-Plex/codex-mic?color=7ef0c0&label=%EC%B5%9C%EC%8B%A0%20%EB%B2%84%EC%A0%84)](https://github.com/MovieHolic-Plex/codex-mic/releases/latest)
[![downloads](https://img.shields.io/github/downloads/MovieHolic-Plex/codex-mic/total?color=7ef0c0&label=%EB%8B%A4%EC%9A%B4%EB%A1%9C%EB%93%9C)](https://github.com/MovieHolic-Plex/codex-mic/releases)
[![license](https://img.shields.io/github/license/MovieHolic-Plex/codex-mic?color=lightgrey)](LICENSE)

**[⬇️ 다운로드](https://github.com/MovieHolic-Plex/codex-mic/releases/latest)** · [English](README.en.md)

<br>

<img src="docs/screenshot-pill.png" alt="알약 오버레이" /><br><br>
<img src="docs/screenshot-settings.png" alt="설정 패널" width="320" />

</div>

<br>

> [!CAUTION]
> **이거 쓰다 좆되면 님 책임임.**
>
> OpenAI와 무관한 비공식 프로젝트입니다. realtime 프로토콜은 리버스 엔지니어링한 것이라 예고 없이 죽거나 동작이 바뀔 수 있습니다. 계정에 무슨 일이 생겨도 보증 못 합니다. 알아서 쓰세요.

---

## 왜 API 키가 필요 없나

API 키로 붙는 공개 realtime 엔드포인트가 아니라, **Codex 데스크톱 앱이 쓰는 ChatGPT 백엔드**에 `codex login`이 남겨둔 OAuth 토큰으로 붙습니다.

```bash
codex login    # 이미 했다면 건너뛰기
```

`~/.codex/auth.json`의 토큰을 읽어 씁니다. 만료되면 알아서 갱신하고, 앱이 따로 저장하는 자격 증명은 없습니다.

## 설치

[**릴리즈**](https://github.com/MovieHolic-Plex/codex-mic/releases/latest)에서 `codex-mic.exe`를 받아 실행하면 끝입니다. 설치 과정 없는 단독 실행 파일입니다.

> [!TIP]
> 서명하지 않은 바이너리라 SmartScreen이 막습니다 → `추가 정보` → `실행`

<details>
<summary><b>직접 빌드</b></summary>

```bash
git clone https://github.com/MovieHolic-Plex/codex-mic
cd codex-mic/src-tauri
cargo build --release        # → target/release/codex-mic.exe
cargo test                   # 하드웨어도 네트워크도 필요 없음
```

Rust 툴체인과 WebView2만 있으면 됩니다. 프론트엔드는 정적 파일이라 별도 빌드 단계가 없습니다.
</details>

## 쓰는 법

<div align="center">

| | |
|:--|:--|
| 🎤 `Ctrl+E` **누른 채 말하기** | 떼면 커서 위치에 받아쓰기 |
| ⚙️ `Ctrl+Shift+E` | 설정 패널 |

</div>

알약은 **키보드 포커스를 뺏지 않습니다.** 드래그해서 옮겨도 받아쓰던 창이 활성 상태 그대로 유지되고, 옮긴 위치는 저장됩니다.

> [!IMPORTANT]
> **엔터는 절대 입력되지 않습니다.** 전사에 줄바꿈이 있어도 공백으로 바뀝니다 — 채팅창이나 터미널에서 의도치 않게 전송되는 사고를 막습니다.

## 설정

<div align="center">

| 항목 | 기본값 |
|:---|:---|
| **작동 방식** | 누르는 동안 녹음 |
| **텍스트 입력 방식** | 키 입력 시뮬레이션 |
| **언어** | 자동 감지 |
| **전사 모델** | 실측 코멘트가 붙은 목록에서 선택 |
| **세션 모델** | realtime 세션 종류 |
| **마이크 게인** | +20 dB |
| **환각 전사 필터** | 켬 |

</div>

`%APPDATA%\codex-mic\config.json`에 저장됩니다. 핫키와 모델은 **재시작 없이** 바로 적용됩니다.

## 안 될 때

<details>
<summary><b>받아쓰기가 비어 있음</b></summary>

말하면서 레벨 미터를 보세요. 거의 안 움직이면 마이크 게인을 올립니다. 노트북·USB 마이크는 출력이 아주 작은 경우가 흔합니다. 전혀 안 움직이면 마이크 장치가 잘못 잡힌 겁니다.
</details>

<details>
<summary><b>핫키가 반응 없음</b></summary>

다른 앱이 먼저 등록한 경우입니다. 시작할 때 알약에 등록 실패가 표시됩니다. 설정에서 다른 키로 바꾸세요.

단독으로 쓸 수 있는 키(확인함): `F9` `F13` `CapsLock` `` ` `` `Space` `ScrollLock` `Pause` `NumLock` `Insert` `Home`
</details>

<details>
<summary><b>「codex login 필요」</b></summary>

터미널에서 `codex login`을 다시 실행하세요. 액세스 토큰뿐 아니라 리프레시 토큰까지 서버에서 폐기된 경우입니다.
</details>

<details>
<summary><b>영어 단어가 한글로 나옴</b></summary>

`endpoint` → `엔드포인트`처럼 나오는 건 전사 모델 공통 성질입니다. 설정에서 다른 전사 모델을 시도해 보세요 — 각 항목에 실측 코멘트가 붙어 있습니다.
</details>

<details>
<summary><b>로그와 음성 덤프</b></summary>

```powershell
$env:CODEX_MIC_DEBUG_TRANSCRIPT=1                    # 전사 텍스트 기록
$env:CODEX_MIC_DEBUG_DUMP_WAV="$env:TEMP\sent.wav"   # 보낸 음성을 WAV로
.\codex-mic.exe
```

`sent.wav`에는 서버로 나간 오디오가 그대로 담깁니다. **들어보면 문제가 마이크 쪽인지 모델 쪽인지 즉시 갈립니다.**
</details>

## 동작 방식

```
Ctrl+E 누름 ──▶ cpal 캡처 48kHz ──▶ 24kHz 모노 PCM16 ──▶ WebSocket 스트리밍
                                                              │
Ctrl+E 뗌   ──▶ 무음 2초 패딩 ──▶ commit ────────────▶ realtime 세션
                                                              │
                                                    전사 모델 (ASR)
                                                              ▼
                                              정제 ──▶ 커서에 주입
```

설계 결정 몇 가지 — **전부 실측으로 정한 것입니다:**

- **오디오는 webview를 거치지 않습니다.** cpal이 잡아 Rust에서 바로 나갑니다.
- **서버 VAD를 끕니다.** 턴 경계는 키를 뗀 시점입니다. VAD에 맡기면 문장 사이 쉼마다 턴이 갈려 전사가 조각납니다.
- **앞뒤 2초 무음 패딩.** 말이 시작되는 순간 녹음이 시작되면 모델 윈도잉이 첫 음절을 먹습니다.
- **전사는 ASR 부가 채널이 담당합니다.** 세션 모델은 생성형이라 문맥이 쌓이면 **하지 않은 말을 지어냅니다** — 실측으로 확인해 기본에서 제외했습니다.

<details>
<summary><b>환경변수</b> — 재빌드 없이 실험</summary>

| 변수 | 용도 |
|:---|:---|
| `CODEX_MIC_TRANSCRIBE_MODEL` | 전사 모델 (설정보다 우선) |
| `CODEX_MIC_REALTIME_MODEL` | 세션 모델 |
| `CODEX_MIC_SERVER_VAD=1` | 서버 VAD 복원 |
| `CODEX_MIC_MODEL_TRANSCRIBE=1` | 세션 모델로 전사 (권장하지 않음) |
| `CODEX_MIC_DEBUG_TRANSCRIPT=1` | 전사 텍스트 로깅 |
| `CODEX_MIC_DEBUG_DUMP_WAV` | 보낸 음성 WAV 덤프 |
| `CODEX_MIC_DEBUG_PCM_FILE` | 마이크 대신 PCM 파일 재생 |

</details>

<details>
<summary><b>구조</b></summary>

```
src-tauri/src/
  lib.rs       핫키 처리, PCM 펌프, 녹음 수명주기
  keystate.rs  Idle/Starting/Recording/Stopping 상태 머신
  winkeys.rs   Win32 키보드 상태 — 캡스 토글, 물리 키 확인
  realtime.rs  realtime WebSocket 세션, 전사 이벤트
  audio.rs     cpal 캡처, 다운믹스, 리샘플, 게인
  dictate.rs   전사 누적, 커밋 타이밍, 새니타이즈, 주입
  auth.rs      Codex CLI OAuth 토큰과 갱신
  config.rs    설정 저장
  commands.rs  Tauri 커맨드와 공유 상태
ui/            알약과 설정 패널 — 프레임워크 없는 HTML/CSS/JS
```

작은 단일 목적 모듈이고 테스트가 바로 옆에 붙어 있습니다.
</details>

---

<div align="center">

**[MIT](LICENSE)**

</div>
