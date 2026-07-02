# 설치

[English](INSTALL.md) | **한국어**

RTRT는 알파 단계입니다. 설치 경로는 **원라이너 스크립트** (`install.sh` / `install.ps1` — 최신 릴리스가 없으면 `--main`으로 소스 빌드 폴백)와 **`cargo`로 소스 빌드** 두 가지입니다.

## 원라이너 (권장)

```bash
# Linux / macOS / WSL — 최신 릴리스. 릴리스 없으면 자동으로 --main 폴백
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh
```

```powershell
# Windows PowerShell
irm https://raw.githubusercontent.com/kernalix7/rtrt/main/install.ps1 | iex
```

설치 스크립트는 OS + arch를 감지하고 최신 GitHub Release에서 맞는 타르볼/zip을 받아 SHA256을 검증한 뒤 `rtrt` / `rtrt-mcp` / `rtrt-dashboard`를 `~/.local/bin/` (Linux/macOS) 또는 `%LOCALAPPDATA%\Programs\rtrt\` (Windows)에 배치합니다.

### 플래그 + 환경 변수

| 플래그 | PowerShell | 환경 변수 | 동작 |
|--------|-----------|-----------|------|
| `--version vX.Y.Z` | `-Version` | — | 특정 릴리스 타르볼 고정 |
| `--main` (`--ref main` 별칭) | `-Main` | `RTRT_REF=main` | git main HEAD 빌드 |
| `--ref TAG` | `-Ref` | `RTRT_REF` | 임의 태그 / 브랜치 / 커밋 빌드 |
| `--source PATH` | `-Source` | `RTRT_SOURCE` | 로컬 복사본 빌드 (오프라인) |
| `--dir PATH` | `-InstallDir` | — | 설치 경로 변경 |
| `--skip-deps` | `-SkipDeps` | `RTRT_SKIP_DEPS=1` | cargo / git 툴체인 체크 우회 |
| `--no-setup` | — | `RTRT_NO_SETUP=1` | Claude Code MCP 설정 + 훅 자동 갱신 안 함 |
| `--no-service` | `-NoService` | `RTRT_NO_SERVICE=1` | `rtrt-dashboard` 백그라운드 서비스 자동 시작 안 함 |
| `--uninstall` | `-Uninstall` | — | 호환성 셰임 — `uninstall.sh` / `uninstall.ps1` 권장 |
| `--dry-run` | `-DryRun` | — | 실제 쓰기 없이 동작만 출력 |

플래그가 환경 변수보다 우선. 릴리스 없고 플래그도 없으면 안내 후 `--ref main`으로 자동 폴백.

### 백그라운드 대시보드 서비스

기본적으로 설치 시 `rtrt-dashboard`를 백그라운드 서비스로 띄워, 직접 실행 안 해도 <http://127.0.0.1:7311> 웹 UI가 항상 떠 있음 — 크래시 시 재시작, 로그인 시 자동 기동. `--no-service`(Windows는 `-NoService`, 또는 `RTRT_NO_SERVICE=1`)로 끔.

- **Linux** — systemd **user** 유닛 `~/.config/systemd/user/rtrt-dashboard.service`.
- **macOS** — launchd LaunchAgent `~/Library/LaunchAgents/io.kodenet.rtrt-dashboard.plist`.
- **Windows** — `rtrt-dashboard` 로그온 예약 작업.

직접 관리: `rtrt service install|uninstall|status` (Linux/macOS; 기본 dry-run, `--apply`로 실행). 언인스톨러는 바이너리 삭제 전에 서비스를 먼저 제거.

예시:

```bash
# 릴리스 고정
curl -fsSL .../install.sh | sh -s -- --version v0.2.0

# 토픽 브랜치 추적
RTRT_REF=feature/cache curl -fsSL .../install.sh | sh

# 로컬 클론에서 빌드 (오프라인)
sh install.sh --source ~/code/rtrt

# 다른 경로 + 툴체인 체크 우회
sh install.sh --dir /opt/rtrt/bin --skip-deps
```

### 원라이너 제거

```bash
# Linux / macOS / WSL — Claude Code 연동(MCP + 훅 + 상태줄) 해제 + 대시보드
# 서비스 + 바이너리 제거 (~/.rtrt 상태 유지)
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh | bash -s -- --confirm

# 완전 제거 — 위 항목 + ~/.rtrt + fastembed 모델 캐시
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh | bash -s -- --purge
```

```powershell
# Windows PowerShell — `irm | iex`는 파라미터를 전달하지 못하므로 스크립트블록으로 감쌉니다
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1))) -Confirm
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1))) -Purge
```

언인스톨러는 `install.sh` / `rtrt setup`이 만든 모든 것 — Claude Code MCP 등록, 훅, 상태줄, 스킬(내부적으로 `rtrt uninstall --agent claude --plugin --apply`), 대시보드 백그라운드 서비스, 바이너리 3종 — 을 제거하되, `--purge` / `-Purge`를 넘기지 않는 한 데이터(`~/.rtrt` 메모리 저장소, 프롬프트 레지스트리)는 보존합니다.

로컬에서 `--confirm` / `-Confirm` 없이 실행하면 단계마다 확인을 묻는 대화형 모드로 동작합니다. 기존 `install.sh --uninstall` / `install.ps1 -Uninstall`은 상태를 건드리지 않는 호환성 셰임으로 남아 있습니다.

## 소스 빌드

필요한 도구:

- 러스트 stable 1.85+ (edition 2024). 없으면 `rustup install stable`.
- `rusqlite` 번들 SQLite 빌드용 C 툴체인 (`gcc` 또는 `clang`).

```bash
git clone https://github.com/kernalix7/rtrt
cd rtrt
cargo build --release --workspace
```

빌드 산출물은 `target/release/`에 세 개 바이너리로 떨어집니다.

- `rtrt` — 최상위 CLI (`crates/rtrt-cli`)
- `rtrt-mcp` — MCP 서버 (`crates/rtrt-mcp`)
- `rtrt-dashboard` — 웹 대시보드 (`crates/rtrt-dashboard`)

CLI를 `PATH`에 설치하려면:

```bash
cargo install --path crates/rtrt-cli
```

MCP 서버 / 대시보드 바이너리까지 전역으로 두려면 `crates/rtrt-mcp`, `crates/rtrt-dashboard`도 같은 방식으로 설치하세요.

## crates.io (예정)

```bash
cargo install rtrt-cli         # rtrt 바이너리
cargo install rtrt-mcp         # MCP 서버
cargo install rtrt-dashboard   # 웹 대시보드
```

아직 게시 전입니다.

## 사전 빌드 바이너리 (예정)

GitHub 릴리스에 다음 아카이브를 게시할 예정입니다.

- `rtrt-<version>-x86_64-unknown-linux-gnu.tar.gz`
- `rtrt-<version>-aarch64-unknown-linux-gnu.tar.gz`
- `rtrt-<version>-x86_64-apple-darwin.tar.gz`
- `rtrt-<version>-aarch64-apple-darwin.tar.gz`
- `rtrt-<version>-x86_64-pc-windows-msvc.zip`

각 아카이브에는 `rtrt`, `rtrt-mcp`, `rtrt-dashboard`가 모두 포함됩니다.

## 설치 확인

```bash
rtrt --version
rtrt info
rtrt templates
```

`rtrt info`는 버전과 크레이트 목록을, `rtrt templates`는 빌트인 6종을 출력해야 합니다.

## 제거 (수동)

`cargo install` 경로로 설치했다면:

```bash
cargo uninstall rtrt-cli rtrt-mcp rtrt-dashboard
```

원라이너 설치 경로는 위쪽 [원라이너 제거](#원라이너-제거) 절을 사용하세요. 독립 스크립트(`uninstall.sh` / `uninstall.ps1`)로 살아 있으며 `--confirm` (Claude Code 연동 + 서비스 + 바이너리, 데이터 유지) 또는 `--purge` (위 항목 + `~/.rtrt` + fastembed 캐시)를 받습니다.

수동 상태 정리:

```bash
rm -rf ~/.rtrt/                # 메모리 저장소, 프롬프트 레지스트리, 커스텀 템플릿
rm -rf ~/.cache/fastembed/      # ONNX 모델 캐시 (embeddings 피처 사용 시에만 생성)
```

저장소 클론도 필요 없다면 함께 삭제하세요.
