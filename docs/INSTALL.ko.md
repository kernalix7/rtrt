# 설치

[English](INSTALL.md) | **한국어**

RTRT는 알파 단계입니다. 지금 권장되는 설치 경로는 **`cargo`로 소스 빌드**입니다. 사전 빌드 바이너리, crates.io 게시, 원라이너 스크립트는 예정 항목입니다.

## 소스 빌드 (현재 권장)

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

## 원라이너 (예정)

```bash
# macOS / Linux / WSL
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh
```

```powershell
# Windows
irm https://raw.githubusercontent.com/kernalix7/rtrt/main/install.ps1 | iex
```

설치 스크립트는 최신 GitHub 릴리스에서 호스트에 맞는 사전 빌드 바이너리를 받아 사용자 경로(`~/.local/bin/` 또는 `%LOCALAPPDATA%\Programs\rtrt\`)에 배치할 예정입니다. 아직 연결되지 않았으며 [#1](https://github.com/kernalix7/rtrt/issues)에서 추적합니다.

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

## 제거

cargo로 설치한 바이너리 제거:

```bash
cargo uninstall rtrt-cli rtrt-mcp rtrt-dashboard
```

로컬 상태(메모리 저장소, 커스텀 템플릿)도 같이 지우려면:

```bash
rm -rf ~/.rtrt/
```

저장소 클론도 필요 없다면 함께 삭제하세요.
