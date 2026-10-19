# OmO native 통합

[English](OMO_NATIVE.md) | **한국어**

이 통합은 OmO/Senpi native 확장 API를 대상으로 합니다. OpenCode 2.x server
플러그인이나 구버전 oh-my-openagent 연동과는 별도 경로입니다.

## 설치

`rtrt setup --agent omo` 지원은 **v0.2.4**부터 시작됩니다. v0.2.3을 포함한
이전 릴리스에는 이 setup 대상이 없습니다. 바이너리를 어떻게 받았느냐에 따라
설치 경로를 고르세요.

### 릴리스 바이너리 고정 (권장)

v0.2.4 릴리스 타르볼을 고정하고 설치 스크립트의 Claude 갱신과 Linux OpenCode
부트스트랩은 끄고 대시보드 백그라운드 서비스도 건너뜁니다. 절대 `~/.local/bin/`
경로를 쓰면 PATH 앞쪽의 구버전 `rtrt`/`rtrt-mcp`가 호출을 가져가지 못합니다.

```sh
# Linux / macOS / WSL — v0.2.4 고정, 에이전트 setup 끔, 대시보드 서비스 끔
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/v0.2.4/install.sh \
  | sh -s -- --version v0.2.4 --no-setup --no-service

# 먼저 dry-run — 변경 내용을 출력만 하고 쓰지는 않습니다
"$HOME/.local/bin/rtrt" setup --agent omo \
  --binary "$HOME/.local/bin/rtrt-mcp"

# 미리보기가 정상이면 같은 명령에 --apply를 붙여 적용
"$HOME/.local/bin/rtrt" setup --agent omo \
  --binary "$HOME/.local/bin/rtrt-mcp" --apply
```

두 setup 명령은 `--apply` 외에는 완전히 같은 플래그입니다. `--apply`가 없으면
setup은 변경 계획을 출력만 하고 쓰지 않습니다. `--apply`로 적용한 뒤에는 OmO를
reload하거나 재시작해 갱신된 `mcp.json`과 `extensions/` 디렉터리를 다시
읽게 하세요.

### Windows 릴리스 바이너리 고정

```powershell
# PowerShell — v0.2.4 고정, 에이전트 setup 끔, 대시보드 서비스 끔
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/kernalix7/rtrt/v0.2.4/install.ps1))) `
  -Version v0.2.4 -NoSetup -NoService

# 먼저 dry-run
& "$env:LOCALAPPDATA\Programs\rtrt\rtrt.exe" setup --agent omo `
  --binary "$env:LOCALAPPDATA\Programs\rtrt\rtrt-mcp.exe"

# --apply로 적용
& "$env:LOCALAPPDATA\Programs\rtrt\rtrt.exe" setup --agent omo `
  --binary "$env:LOCALAPPDATA\Programs\rtrt\rtrt-mcp.exe" --apply
```

설치 스크립트가 만든 절대 `.exe` 경로를 `%LOCALAPPDATA%\Programs\rtrt\` 아래에서
가져다 쓰세요. `--apply` 뒤에는 OmO를 재시작해 새 MCP 항목과 `extensions/rtrt/`
디렉터리가 반영되게 해야 합니다. `-NoService`는 대시보드 로그온 작업을 등록하지
않도록 합니다. `rtrt-mcp`의 stdio 프로세스는 그 작업과 무관하게 OmO가
필요할 때마다 실행합니다.

### 소스에서 설치 (대안)

워크스페이스를 직접 빌드하는 경우 `cargo install --path`도 작동합니다. PATH 앞쪽의
구버전 바이너리를 피하려면 절대 경로를 그대로 적어 주세요.

```sh
cargo install --path crates/rtrt-cli --force
cargo install --path crates/rtrt-mcp --force
"$HOME/.cargo/bin/rtrt" setup --agent omo --binary "$HOME/.cargo/bin/rtrt-mcp"
"$HOME/.cargo/bin/rtrt" setup --agent omo --binary "$HOME/.cargo/bin/rtrt-mcp" --apply
```

첫 setup 명령은 dry-run입니다. Windows에서는 설치된 `.exe` 파일의 절대 경로를
사용합니다. 적용한 뒤 OmO를 reload하거나 재시작합니다.

에이전트 디렉터리는 비어 있지 않은 `OMO_CODING_AGENT_DIR`,
`SENPI_CODING_AGENT_DIR`, `PI_CODING_AGENT_DIR` 순으로 선택합니다.
설정한 값은 절대 경로여야 하며 기본값은 `~/.omo/agent`입니다.

Setup은 파일 세 개를 관리합니다.

- `<agentDir>/mcp.json`: `mcpServers.rtrt`에 stdio transport, MCP 실행 파일의
  절대 경로와 빈 `args` 배열을 병합합니다. 활성 프로젝트에서 서버를 시작하도록
  `cwd`는 생략하며 다른 서버와 설정은 보존합니다.
- `<agentDir>/extensions/rtrt/index.js`: native 자동 탐색용 진입점입니다.
  출력 필터에는 setup을 실행한 CLI의 `rtrt` 실행 파일을 고정합니다.
- `<agentDir>/extensions/rtrt/runtime.mjs`: 추가 의존성이 없는 native 어댑터입니다.

진입점은 MCP의 기존 `settings.toolPrefix`를 사용하며 기본값은 `mcp`입니다.
Native 도구 검색과 승격 설정은 변경하지 않습니다.
런타임은 factory 실행 시 native `registerMcpServer` API로 같은 서버를 선언해
native·codemode 도구 탐색에 연결합니다. 기존 사용자 MCP 설정의 우선순위는 호스트가
유지합니다. 도구 prefix를 바꿨다면 setup을 다시 실행합니다.

## 소유권과 제거

Setup은 대상 파일이나 상위 디렉터리의 symlink, 외부 진입점 파일, 수정된 런타임,
충돌하거나 수정된 `mcpServers.rtrt` 항목을 거부합니다. 확장 디렉터리에 있는 다른
사용자 파일은 보존합니다. JSONC 주석과 관련 없는 설정을 유지하며 처음 설정을
변경하기 전에 원본을 `mcp.json.bak`에 남깁니다. 반복 설치는 중복을 만들지 않습니다.

`rtrt uninstall --agent omo`도 기본은 dry-run입니다. `--apply`를 지정하면
소유권이 확인된 MCP 항목과 `index.js`, `runtime.mjs`만 제거합니다. 다른 서버,
사용자 파일, 백업과 디렉터리는 남깁니다. 수정된 관리 파일은 삭제하지 않고 거부합니다.
제거 뒤에는 호스트를 reload하거나 재시작합니다.

## Native 동작

Native MCP 도구 `mcp_rtrt_agent_call`, `mcp_rtrt_agent_route`
(설정된 prefix 적용)의 `tool_call`에 호출별 UUID `invocation_id`,
`parent_session_id`, `parent_call_id`, `caller_agent: "omo-native"`,
`parent_cwd`를 붙입니다. 프로젝트 귀속은 MCP 서버가 결정하며 다른 호출은 바꾸지 않습니다.

단순한 foreground bash 명령이 성공하고 결과가 text 블록 하나일 때 그 출력을 stdin으로
`rtrt proxy -- <원래 명령>`에 전달합니다. 원래 명령을 재실행하거나 인자를
바꾸지 않으며 native 권한 판단도 유지합니다. 필터는 native 세션의 cwd에서
실행하고 결과 본문만 교체합니다. 메타데이터와 text annotation은 보존합니다.

복합 명령·파이프·리다이렉션·치환·여러 줄 명령, 오류, 명시적 background 요청,
nontext·다중 블록 결과, 256 KiB를 넘는 입출력은 그대로 유지합니다.
실행 파일 부재, 필터의 비정상 종료, 취소 또는 전체 1.5초 제한도
원본을 유지합니다. 세션 종료 시 처리 중인 필터를 취소하고 호출 상태를 비웁니다.

프롬프트 자동 수집, Claude 권한 broker 흉내, native 메모리·provider 라우팅 교체,
footer·side panel 변경이나 대시보드 자동 시작은 하지 않습니다.
기존의 명시적 service 명령은 계속 사용할 수 있습니다.

## 검증

```sh
node --test plugins/omo/*.test.mjs
cargo test -p rtrt-cli --test omo_setup
```

Node 테스트는 출처, 출력 필터, 프로세스 경계, Unicode, 취소와 크기·시간 제한을
다룹니다. CLI 테스트는 격리된 HOME과 에이전트 디렉터리에서 설치, 반복 적용,
거부와 제거를 검증합니다.
실제 호스트 테스트는 실행 파일 세 개의 절대 경로를 지정했을 때 실행합니다.

```sh
cargo build -p rtrt-cli -p rtrt-mcp
RTRT_OMO_NATIVE_BIN=/absolute/path/to/omo \
RTRT_NATIVE_RTRT_BIN="$PWD/target/debug/rtrt" \
RTRT_NATIVE_MCP_BIN="$PWD/target/debug/rtrt-mcp" \
node --test plugins/omo/*.test.mjs
```

Linux의 OmO native 5.1.35에서 factory 탐색, MCP templates 성공 호출, 검색 승격,
codemode 내부 bash 출력 필터와 호출별 출처를 확인했습니다. 테스트는 격리된
HOME·에이전트·프로젝트 디렉터리와 loopback 모델 fixture를 사용하며 실제 provider
자격 증명을 사용하지 않습니다. 첫 모델 요청에 카탈로그가 준비됐다고 가정하지 않고
native RPC 연결 응답과 도구 활성화를 기다립니다.
환경 변수가 없으면 외부 호스트 테스트만 건너뜁니다.
