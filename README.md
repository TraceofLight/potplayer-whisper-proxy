# whisper-proxy-rs

PotPlayer의 실시간 자막 변환(Whisper) 기능을 사내 GPU 서버로 우회시키는 도구.
저사양 PC에서도 large-v3 한국어 STT 사용 가능.

```
[PotPlayer] ──spawn──▶ [main64.exe (이 wrapper)] ──HTTP──▶ [Whisper API 서버]
```

## 산출물

| 바이너리 | 크기 | 역할 |
|---------|------|------|
| `main64.exe` | ~1.5 MB | whisper.cpp `main(64).exe` 드롭인 대체 |
| `whisper-proxy-installer.exe` | ~1.6 MB | PotPlayer 자동 탐색 + 설치/롤백 (위 바이너리 임베드) |

미리 빌드된 바이너리는 [Releases](../../releases) 페이지에서 다운로드.

## 빌드 (소스에서)

```powershell
cargo build --release -p whisper-proxy            # main64.exe 먼저
cargo build --release -p whisper-proxy-installer  # installer (proxy 임베드)
```

산출물은 `target/release/`에 떨어집니다.

## 배포 / 사용

### 사용자 PC에 설치 (관리자 PowerShell)

```powershell
.\whisper-proxy-installer.exe install --url http://<your-whisper-server>:<port>/v1
```

### 옵션

```powershell
# 다른 엔진만 (기본은 모든 엔진)
.\whisper-proxy-installer.exe install --engine Vulkan --url http://...

# 모델 변경 (default: Systran/faster-whisper-large-v3)
.\whisper-proxy-installer.exe install --model Systran/faster-whisper-large-v3 --url ...

# OpenAI 클라우드 (유료)
.\whisper-proxy-installer.exe install `
    --url https://api.openai.com/v1 `
    --api-key sk-XXX `
    --model whisper-1

# PotPlayer가 비표준 위치
.\whisper-proxy-installer.exe install --potplayer-dir "D:\Tools\PotPlayer" --url ...

# 상태 확인 / 롤백
.\whisper-proxy-installer.exe status
.\whisper-proxy-installer.exe uninstall
```

### PotPlayer 사용

1. PotPlayer 실행 → 영상 재생
2. **우클릭 → 자막 → 실시간 자막 변환** (또는 `Alt+W`)
3. 엔진 **Vulkan** 또는 **CPU** 선택
4. 한국어 자막 자동 표시

## 테스트 환경

이 빌드가 정상 동작 확인된 환경:

| 항목 | 값 |
|------|------|
| OS | Windows 10 / Windows 11 |
| PotPlayer | **v260401** (32-bit, `Program Files (x86)\DAUM\PotPlayer`) |
| Whisper 엔진 | **CPU**, **Vulkan** 양쪽 검증 |
| Whisper API 서버 | OpenAI 호환 (faster-whisper-server, speaches 등) |
| 모델 | `Systran/faster-whisper-large-v3` (모델 이름은 서버에 로드된 것과 무관 — 단일 모델 서버는 보통 무시) |

**본인 PotPlayer 버전 확인:**
- PotPlayer 우상단 메뉴 `≡` → **PotPlayer 정보**
- 또는 PowerShell: `(Get-ItemProperty 'HKCU:\SOFTWARE\DAUM\PotPlayer').ProgramVersion`

> **주의**: PotPlayer ↔ whisper.cpp wrapper 사이의 IPC 프로토콜은 비공개라 PotPlayer 업데이트 시 변경될 수 있습니다. 다른 버전에서 동작하지 않으면 issue로 보고 부탁드립니다.

## PotPlayer 자동 탐색 우선순위

1. `HKLM\Software\(WOW6432Node\)?Microsoft\Windows\CurrentVersion\App Paths\PotPlayer.exe` 의 `Path`
2. `PotPlayerMini.exe`, `PotPlayerMini64.exe` 도 동일
3. `C:\Program Files (x86)\DAUM\PotPlayer`
4. `C:\Program Files\DAUM\PotPlayer`

탐색 실패 시 `--potplayer-dir <경로>` 명시.

## 설정 우선순위 (wrapper 측)

1. 환경변수: `WHISPER_PROXY_URL`, `WHISPER_PROXY_KEY`, `WHISPER_PROXY_MODEL`, `WHISPER_PROXY_TIMEOUT`
2. `Module\Whisper\<엔진>\whisper-proxy.ini`
3. 내장 기본값 (`http://localhost:8000/v1`, large-v3, 120초)

설치 시 ini가 자동 생성되며, 이미 존재하면 보존됩니다.

## 디버그 모드

`Module\Whisper\<엔진>\whisper-proxy.debug` 빈 파일을 만들면 모든 호출이
`%TEMP%\whisper-proxy.log` 와 `%TEMP%\whisper-proxy-ipc.log` 에 기록됨.

## 폴더 구조

```
potplayer-whisper-proxy/
├── Cargo.toml                  # 워크스페이스
├── README.md                   # 이 문서
├── proxy/                      # main64.exe 본체
│   ├── Cargo.toml
│   └── src/{main.rs, ipc.rs}
├── installer/                  # whisper-proxy-installer.exe
│   ├── Cargo.toml
│   ├── whisper-proxy.ini.template
│   └── src/main.rs
└── scripts/                    # GPU 서버 셋업 가이드
    └── README.md
```

## 한계

- API 서버 다운 시 자동 fallback 없음
- 첫 호출은 모델 콜드 로드로 ~5초
- 한국어 자동 감지 (수동 언어 지정은 PotPlayer UI)

## 트러블슈팅

| 증상 | 원인 / 조치 |
|------|------------|
| 자막이 안 뜸 | `whisper-proxy-installer.exe status`로 INSTALLED 확인 → ini의 url 확인 |
| 영상 재생이 끊김 | ini의 `timeout = 1800` 이상으로 |
| `connection refused` | API 서버 다운 또는 사내망 접근 불가 |
| seek 후 자막 안 따라옴 | 최신 빌드인지 확인 (seek epoch 추적은 v1.0+ 적용) |
