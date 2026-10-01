# rtrt-agent

`rtrt-agent`는 RTRT의 OpenCode 통합입니다. 지원되는 hook 사이에서 상위 session,
agent, worktree, invocation 및 permission broker 문맥을 보존하고 로컬 대시보드를
백그라운드에서 사용할 수 있게 유지합니다.

## 설치 (v1 setup-managed wiring, v0.2.0)

이 소스는 v0.2.0을 대상으로 합니다. 설치하려면 버전이 일치하는 게시된 npm release가 필요하며, 소스의 버전 표시는 게시 증명이 아닙니다. 이전 `rtrt-agent@0.1.7` release는 그대로이며 아래 native v2 진입점을 포함하지 않습니다.

버전이 일치하는 npm release의 설치 형태는 다음과 같습니다.

```sh
npm install rtrt-agent@0.2.0
```

`opencode.json`의 단수형 `plugin` 배열에 패키지를 등록합니다.

```json
{
  "plugin": ["rtrt-agent@0.2.0"]
}
```

v1 패키지 root는 이름 있는 export `RtrtProvenance` 하나를 그대로 유지하며 v0.2.0에서도 이름은 바뀌지 않습니다. 선택 사항인 v1 TUI statusline은 별도 setup-managed 통합으로 남고 v1 패키지는 이를 export하거나 pack하지 않습니다. 새로 staged 된 native v2 소스는 아래에서 설명하며 `rtrt-agent@0.1.7`에는 게시되지 않습니다.

## 대시보드

플러그인을 로드하면 `rtrt-agent`는 `127.0.0.1:7311`에서 실행되는 사용자별 detached
`rtrt-dashboard` process를 예약합니다. 시작 과정은 대시보드 준비를 기다리지 않으며
브라우저를 자동으로 열지 않습니다. 정확히 같은 버전의 플랫폼별 optional npm 패키지가
실행 파일을 제공하므로 미리 설치된 `rtrt`, `PATH` 검색, install script 또는 project
build를 사용하지 않습니다.

필요할 때만 브라우저를 엽니다.

```sh
rtrt-dashboard-open
```

에이전트는 인자가 없는 `rtrt_dashboard_open` 도구로 같은 명시적 동작을 수행할 수
있습니다. 두 경로 모두 60초 fragment bootstrap을 사용하고 고정 상태만 반환하므로
credential이 prompt나 command template에 들어가지 않습니다. 실행 파일 또는 private
state가 안전하지 않거나 없으면 시작만 fail-soft로 중단되고 OpenCode는 정상적으로
계속됩니다. 기존 `~/.rtrt` 데이터는 보존합니다.

`rtrt setup --agent opencode --apply`는 전체 RTRT 통합을 관리하지만 npm 설치는 수행하지
않으며, 설정된 패키지는 OpenCode가 시작할 때 설치합니다. Setup은 먼저 정확한
`rtrt-agent@0.2.0` 등록과 모든 대체 관리 asset을 기록합니다. 이 기록이 성공한 뒤에만
인식된 legacy RTRT plugin을 제거하며, 이전 단계가 실패하면 legacy runtime을 보존합니다.
Setup은 `rtrt-agent`의 정확히 일치하는 bare, pinned, ranged, tuple, object 형식만 소유하고
정확한 `"rtrt-agent@0.2.0"` string 하나로 정규화합니다. 이전의 미게시 `rtrt`와 초안
`rtrt-opencode` 패키지 spec은 외부 값이며 순서를 유지합니다. 위 직접 등록과
`opencode plugin rtrt-agent@0.2.0 --global`은 계속 유효합니다.

관리 agent 소유권 상태는 다음 순서의 첫 비어 있지 않은 root에서 읽습니다.
`$OPENCODE_CONFIG_DIR`, `$XDG_CONFIG_HOME/opencode`, `~/.config/opencode`. 공존은 OMO
4.19.4를 대상으로 CI에서 검사하고 OpenCode 1.18.29에서 직접 검증했으며, 어느 쪽도
미래 버전 지원을 보장하지 않습니다. 통합 RTRT 릴리스 워크플로는 정확한 버전의
대시보드 플랫폼 패키지 5개를 `rtrt-agent@0.2.0`보다 먼저 게시한 뒤 일치하는 Rust
릴리스 artifact를 게시합니다.

## Native OpenCode 2.0.20 진입점

v0.2.0 source manifest의 pack 목록에는 native v2 server와 TUI 파일이 추가되지만, npm registry의 `rtrt-agent@0.1.7`에는 **포함되지 않습니다**. 게시된 `rtrt-agent@0.1.7`은 native v2 진입점을 설치·등록하지 않으며, 과거 `0.1.7` 패키지 계약은 게시된 그대로 유지됩니다. v0.2.0 native 소스의 검증 대상은 정확히 `@opencode/cli@2.0.20`과 `@opencode/plugin@2.0.20`이며, 1.x SDK의 v2 preview가 아닙니다. 패키지 root는 v1 이름 있는 export `RtrtProvenance`를 유지합니다 (이름만, v0.2.0에서도 변경 없음).

v0.2.0 source pack에 staged 된 항목:

- `./server`의 native `Plugin.Definition` default export. 이름 있는 `createNativeServer` 팩토리가 `{ id: "rtrt-agent", setup }` 형태로 생성합니다. v1 `RtrtProvenance` 경로는 그대로 별도입니다.
- `package.json`의 `exports`에서 `./server`와 `./tui`에 사용하는 single-element 배열. OpenCode 1.18.33 legacy resolver는 배열을 무시하고 `main`인 `index.js`로 fallback 하며, native 2.0.20은 정상적으로 해석합니다.
- v1 TUI statusline은 여전히 별도의 `app_bottom` 등록입니다. v2 native `rtrt-statusline.tsx`는 `tui/v2/` 아래에 있고 2.x에서만 동작하며 v1 연결은 그대로입니다.
- Native `prompt.footer.status` 등록과 server·TUI hook의 cleanup.

Tool-arg provenance, 대시보드 자동 시작, 명시적 `rtrt_dashboard_open`은 모두 native server 뒤에 있습니다. Permission 정책은 fail-closed이며 명시적 `deny`는 v1 broker와 v2 evaluator 양쪽에서 그대로 보존됩니다. 외부 Claude CLI permission broker 지원과 per-call shell identity / provider-limit recovery는 native v2 build에서 **지원되지 않습니다**. 이는 2.0.20 public API에 해당 표면이 없기 때문이며 어떤 workaround로도 자동 승인되지 않습니다. 기본 deny 정책은 v1 broker에서와 동일합니다.

### 로컬 소스 checkout (npm 설치 없음)

로컬 checkout으로 개발할 때는 OpenCode의 native `plugins` config가 디렉터리 참조를 받습니다. 정확한 경로는 checkout에 따라 다르므로 형식만 보여 줍니다.

```json
{
  "plugins": [
    "/absolute/path/to/rtrt/plugins/opencode"
  ]
}
```

이 디렉터리 형식은 격리된 HOME/XDG 환경에서 실제 `@opencode/cli@2.0.20` host로 로컬 검증했습니다. 그 정확한 버전에서 `server.js`가 active이고 native TUI 탐색 결과는 `features.tui: true`이며 RTRT stdio MCP가 연결됩니다. TUI strict typecheck, 번들 빌드, 120열·40열 headless footer 렌더링도 통과합니다. 검증은 전체 interactive OpenCode TUI를 다루지 않으며 다른 OpenCode 버전을 보장하지 않습니다. 검증 대상은 정확히 테스트한 `@opencode/cli@2.0.20` host에 한정됩니다.

### Native MCP 형식 (정보 제공, OpenCode 2 공식 schema)

Native local MCP 형식은 `mcp.servers.rtrt.type: "local"`과 `["/absolute/path/to/rtrt-mcp", "--transport", "stdio"]` 같은 `command` 배열을 사용합니다. 옵션 이름은 `disabled`와 `codemode`이며 sandbox source config에서는 둘 다 `false`입니다. Release에 고정된 v1 등록은 `rtrt-agent@0.2.0`입니다. `rtrt setup --agent opencode --apply`는 여전히 v1 설정을 기록하며 native v2 설치가 아닙니다. Setup 자체는 npm 패키지를 설치하지 않고, OpenCode가 시작할 때 설정된 패키지를 해석합니다.

게시된 `rtrt-agent@0.1.7`은 npm registry에서 그대로입니다. Sandbox의 기본 v2 설정은 MCP-only이며 staged native plugin·footer·forwarding adapter는 그 경로에서 활성화되지 않습니다. 이 소스 작업은 Docker 배포, `rtrt collector` 서비스, 운영자 측 `forward` 배선 또는 다른 운영 서비스를 promote, 활성화, 연결하지 않습니다 — 이 표면은 분리되어 있고 v0.2.0 소스 변경으로 켜지지 않습니다.

## 라이선스

MIT
