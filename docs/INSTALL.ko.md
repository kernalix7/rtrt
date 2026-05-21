# 설치

[English](INSTALL.md) | **한국어**

RTRT는 알파 단계입니다. 설치 경로는 **원라이너 스크립트** (`install.sh` / `install.ps1` — 최신 릴리스가 없으면 `--main`으로 소스 빌드 폴백)와 **`cargo`로 소스 빌드** 두 가지입니다.

## 원라이너 (권장)

```bash
# Linux / macOS / WSL — 최신 릴리스. 릴리스가 없으면 --main으로 소스 빌드
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh

# main 브랜치에서 직접 빌드 (사전 빌드 바이너리 출시 전)
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh -s -- --main
```

```powershell
# Windows PowerShell
irm https://raw.githubusercontent.com/kernalix7/rtrt/main/install.ps1 | iex
```

설치 스크립트는 OS + arch를 감지하고 최신 GitHub Release에서 맞는 타르볼/zip을 받아 SHA256을 검증한 뒤 `rtrt` / `rtrt-mcp` / `rtrt-dashboard`를 `~/.local/bin/` (Linux/macOS) 또는 `%LOCALAPPDATA%\Programs\rtrt\` (Windows)에 배치합니다.

### 원라이너 제거

```bash
# Linux / macOS / WSL — 바이너리만 (~/.rtrt 상태 유지)
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh | bash -s -- --confirm

# 완전 제거 — 바이너리 + ~/.rtrt + fastembed 모델 캐시
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.sh | bash -s -- --purge
```

```powershell
# Windows PowerShell
irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1 | iex -Args '-Confirm'
irm https://raw.githubusercontent.com/kernalix7/rtrt/main/uninstall.ps1 | iex -Args '-Purge'
```

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

원라이너 설치 경로는 위쪽 [원라이너 제거](#원라이너-제거) 절을 사용하세요. 독립 스크립트(`uninstall.sh` / `uninstall.ps1`)로 살아 있으며 `--confirm` (바이너리만) 또는 `--purge` (바이너리 + `~/.rtrt` + fastembed 캐시)를 받습니다.

수동 상태 정리:

```bash
rm -rf ~/.rtrt/                # 메모리 저장소, 프롬프트 레지스트리, 커스텀 템플릿
rm -rf ~/.cache/fastembed/      # ONNX 모델 캐시 (embeddings 피처 사용 시에만 생성)
```

저장소 클론도 필요 없다면 함께 삭제하세요.
