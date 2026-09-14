# 설치

[English](INSTALL.md) | **한국어**

RTRT는 알파 단계입니다. 설치 경로는 **원라이너 스크립트**와 **`cargo`로 소스 빌드** 두 가지입니다. 원라이너는 최신 릴리스 바이너리를 받고 체크섬을 검증합니다. 최신 릴리스를 찾을 수 없거나 릴리스가 없을 때만 `main` 소스 빌드로 폴백합니다. 릴리스를 선택한 뒤에는 asset 또는 체크섬 오류가 있으면 소스 폴백 없이 실패합니다.

## 원라이너 (권장)

```bash
# Linux / macOS / WSL — 최신 릴리스. 최신 릴리스를 찾을 수 없거나 없을 때만 --main 폴백
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
| `--no-setup` | `-NoSetup` | `RTRT_NO_SETUP=1` | Claude 갱신과 Linux OpenCode bootstrap/session migration 모두 끔 |
| `--no-service` | `-NoService` | `RTRT_NO_SERVICE=1` | `rtrt-dashboard` 백그라운드 서비스 자동 시작 안 함 |
| `--uninstall` | `-Uninstall` | — | 데이터를 보존하는 호환성 셰임; 대화형/purge는 플랫폼 uninstaller 사용 |
| `--dry-run` | `-DryRun` | — | 실제 쓰기 없이 동작만 출력 |

플래그가 환경 변수보다 우선. 최신 릴리스를 찾을 수 없거나 릴리스가 없고 플래그도 없으면 안내 후 `--ref main`으로 자동 폴백합니다. 릴리스를 선택한 뒤 asset 또는 체크섬이 없거나 유효하지 않으면 소스 폴백 대신 설치에 실패합니다.

Linux/WSL에서 기존 OpenCode config/managed state 또는 안전한 absolute `command -v opencode` 결과와 operator-installed fixed `bwrap` 증거가 있고 root가 아니면, 새로 설치한 exact `rtrt`로 machine-only 보안 bootstrap을 자동 적용합니다. 디렉터리를 검색하거나 OpenCode를 실행하지 않습니다. 설치 cwd는 승인하지 않고 기존 global session을 lossless migration합니다. 실패해도 binary와 source DB를 보존하고 manual command를 출력합니다. Native Windows/macOS는 strict `bwrap` setup을 건너뜁니다. 이후 `rtrt opencode --`만 명시적으로 실행한 checkout을 승인합니다.

### 백그라운드 대시보드 서비스

기본적으로 사용자별 machine-scope `rtrt-dashboard`를 등록해 로그인 후 실행합니다. repository cwd, project slug, `RTRT_MEMORY_PATH`, token argv를 사용하지 않고 정확히 `~/.rtrt/dashboard/dashboard.env`를 읽으며, `~/.rtrt/projects`의 검증된 store를 selector로 표시합니다. **All projects**는 집계 보기이므로 project-specific write 전에는 구체적 project를 선택해야 합니다. `--no-service`(Windows는 `-NoService`, 또는 `RTRT_NO_SERVICE=1`)는 service/token 생성을 모두 건너뜁니다. `--dry-run` / `-DryRun`은 쓰지 않지만 일반 pipe/noninteractive 설치는 service를 설치합니다.

- **Linux** — systemd **user** 유닛 `~/.config/systemd/user/rtrt-dashboard.service`.
- **macOS** — launchd LaunchAgent `~/Library/LaunchAgents/io.kodenet.rtrt-dashboard.plist`.
- **Windows** — installer-owned `rtrt-dashboard` 로그온 예약 작업 (`rtrt-dashboard.exe --machine --state-dir "%USERPROFILE%\.rtrt\dashboard"`). Private state ACL은 설치 사용자만 허용하고 재설치는 32-byte CSPRNG token을 재사용하며 확인된 owned task만 갱신합니다.

Linux/macOS 직접 관리: `~/.local/bin/rtrt service install|uninstall|status` (기본 dry-run, `--apply`로 실행). Windows task 생성은 installer가 담당합니다. Unix uninstall과 `install.ps1 -Uninstall`은 확인된 owned service/task definition만 제거하고, 명시적 purge 전에는 machine token과 project DB를 보존합니다.

Windows에서는 현재 `rtrt service` 관리/open을 지원하지 않습니다. <http://127.0.0.1:7311/>을 열고 dashboard bootstrap prompt에만 token을 입력하세요. Token을 command, URL, task definition에 넣지 마세요.

예시:

```bash
# 릴리스 고정
curl -fsSL .../install.sh | sh -s -- --version v0.1.2

# 토픽 브랜치 추적
RTRT_REF=feature/cache curl -fsSL .../install.sh | sh

# 로컬 클론에서 빌드 (오프라인)
sh install.sh --source ~/code/rtrt

# 다른 경로 + 툴체인 체크 우회
sh install.sh --dir /opt/rtrt/bin --skip-deps
```

### 원라이너 제거

```bash
# Linux / macOS / WSL — OpenCode/Claude Code 연동 해제 + 대시보드
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

언인스톨러는 binary 삭제 전에 기록된 OpenCode prior shell을 복원하고 managed agent/service surface를 제거합니다. `--purge` / `-Purge`가 없으면 session DB를 포함한 데이터를 보존합니다.

로컬 실행은 대화형 모드를 지원합니다. 호환성 셰임은 데이터를 보존하며 `install.sh --uninstall`은 managed OpenCode shell을 먼저 복원하고 안전하지 않은 binary 삭제를 거부합니다.

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

MCP 서버 / 대시보드 바이너리까지 전역으로 두려면 `crates/rtrt-mcp`, `crates/rtrt-dashboard`도 같은 방식으로 설치하세요. 이 명령들은 로컬 클론에서 빌드하며, 워크스페이스 크레이트를 crates.io에 게시하지 않습니다.

## 사전 빌드 바이너리

v0.1.2 GitHub 릴리스 채널은 다음 아카이브를 게시합니다.

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

`rtrt info`는 버전과 11개 크레이트 워크스페이스 목록을, `rtrt templates`는 빌트인 4종(`dev`, `design`, `plan`, `standardization`)을 출력해야 합니다.

## 제거 (수동)

`cargo install`로 소스에서 설치했다면:

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
