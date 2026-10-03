# rtrt-agent

`rtrt-agent`는 RTRT의 OpenCode 통합입니다. 지원되는 hook 사이에서 상위 session,
agent, worktree, invocation 및 permission broker 문맥을 보존하고 로컬 대시보드를
백그라운드에서 사용할 수 있게 유지합니다.

## 설치 (v1 setup-managed wiring, v0.2.1)

이 소스는 `v0.2.1`을 대상으로 합니다. 설치에는 일치하는 게시된 npm release가
필요하며, 소스 트리 단독으로는 게시를 증명하지 않습니다. 현재 install command는
다음과 같습니다.

```sh
npm install rtrt-agent@0.2.1
```

`opencode.json`에 등록합니다.

```json
{
  "plugin": ["rtrt-agent@0.2.1"]
}
```

소스 트리는 v1 계약 위에 staged native v2 진입점을 포함하며, `v0.2.1` npm release가
그 소스를 게시합니다. 이미 게시된 `v0.2.0` npm release는 immutable이며 변경되지
않습니다. 이전 `rtrt-agent@0.1.7` release는 npm에서 그대로이며 아래 native v2
진입점을 포함하지 않습니다.

v1 패키지 root는 이름 있는 export `RtrtProvenance` 하나를 그대로 유지하며 v0.2.1에서도
이름은 바뀌지 않습니다. 선택 사항인 v1 TUI statusline은 별도 setup-managed 통합으로
남고 v1 패키지는 이를 export하거나 pack하지 않습니다. 아래에 설명하는 native v2
진입점은 소스 팩에 들어 있으며 v0.2.1 npm release manifest에 추가되어 있습니다. 이
진입점의 host-side 활성화는 [Native OpenCode 2.0.20 진입점](#native-opencode-2020-진입점)에
설명한 경계에 따라 달라집니다.

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
있습니다. 두 경로 모두 보호된 대시보드 state 안의 같은 디렉터리 임시 파일을
전용(exclusive) 모드로 열고 owner 전용 permission 또는 ACL을 적용한 다음 HTML을
작성한 뒤 rename으로 마무리합니다. 일반 opener argv는 로컬 absolute 파일 경로
하나만 담고, URL·fragment·`bootstrap=` 인자는 거부합니다. 60초 single-use HMAC
credential, nonce replay, Origin, bearer exchange, UI fragment clear 동작은 그대로입니다.

`rtrt service open --print-bootstrap`은 짧은 경고 URL만 출력하고 브라우저를 실행하지
않습니다. `--print-bootstrap` 없이 Linux/macOS의 `rtrt service open`은 같은 owner
전용 `bootstrap.html`과 absolute 로컬 파일 경로 argv를 사용합니다. Windows에서는
`rtrt service open`이 거부되므로 <http://127.0.0.1:7311/>을 열고 대시보드 bootstrap
prompt에만 token을 입력하세요.

실행 파일 또는 private state가 안전하지 않거나 없으면 시작만 fail-soft로 중단되고
OpenCode는 정상적으로 계속됩니다. 기존 `~/.rtrt` 데이터는 보존합니다.

`rtrt setup --agent opencode --apply`는 전체 RTRT 통합을 관리하지만 npm 설치는 수행하지
않으며, 설정된 패키지는 OpenCode가 시작할 때 설치합니다. Setup은 먼저 정확한
`rtrt-agent@0.2.1` 등록과 모든 대체 관리 asset을 기록합니다. 이 기록이 성공한 뒤에만
인식된 legacy RTRT plugin을 제거하며, 이전 단계가 실패하면 legacy runtime을 보존합니다.
Setup은 `rtrt-agent`의 정확히 일치하는 bare, pinned, ranged, tuple, object 형식만 소유하고
정확한 `"rtrt-agent@0.2.1"` string 하나로 정규화합니다. 이전의 미게시 `rtrt`와 초안
`rtrt-opencode` 패키지 spec은 외부 값이며 순서를 유지합니다. 위 직접 등록과
`opencode plugin rtrt-agent@0.2.1 --global`은 계속 유효합니다.

관리 agent 소유권 상태는 다음 순서의 첫 비어 있지 않은 root에서 읽습니다.
`$OPENCODE_CONFIG_DIR`, `$XDG_CONFIG_HOME/opencode`, `~/.config/opencode`. 공존은 OMO
4.19.4를 대상으로 CI에서 검사하고 OpenCode 1.18.29에서 직접 검증했으며, 어느 쪽도
미래 버전 지원을 보장하지 않습니다. 통합 RTRT 릴리스 워크플로는 정확한 버전의
대시보드 플랫폼 패키지 5개를 `rtrt-agent@0.2.1`보다 먼저 게시한 뒤 일치하는 Rust
릴리스 artifact를 게시합니다.

### 기존 Windows state와 credential ACL

npm이 관리하는 기존 `.rtrt`, `dashboard`, `startup.lock`, `dashboard.env`,
`bootstrap.html` 경로는 사용 전에 검증됩니다. 새로 만들어진 directory/file은
민감한 byte를 쓰기 전에 owner 전용 permission 또는 ACL(현재 SID FullControl
전용, inheritance 비활성)을 부여받습니다. private credential 또는 state
file에 비-owner write ACE이거나 상속된 permissive ACE가 있는 기존 경로는
거부되며, 시작이 fail closed하고 OpenCode는 대시보드 없이 계속됩니다. 공개
대시보드 binary에 적용되는 SYSTEM/Admins 및 trusted-OS-binary 면제는 private
credential 또는 state ACL까지 확장되지 않습니다. 더 오래된 unsafe 기존 state는
"무통으로" 보이지 않게 다시 쓰여지지 않습니다. 다가오는 `v0.2.1` 후보
remediation은 fresh state 강화와 unsafe 기존 state 거절을 추가한 것이지,
삭제나 자동 ACL 복구를 도입한 것이 아니며 운영자 파일을 옮기는 painless
migration을 약속하지 않습니다. 이미 게시된 `v0.2.0` npm release는 immutable
이고 이 수정도 포함하지 않습니다.

## Native OpenCode 2.0.20 진입점

`v0.2.1` npm release는 native v2 server와 TUI 파일을 pack 목록에 동봉합니다. npm
registry의 이전 `rtrt-agent@0.1.7` release는 그 진입점을 포함하거나 등록하지 않으며
그대로입니다. native 소스의 검증 대상은 정확히 `@opencode/cli@2.0.20`과
`@opencode/plugin@2.0.20`이며, 1.x SDK의 v2 preview가 아닙니다. 패키지 root는 v1
이름 있는 export `RtrtProvenance`를 유지합니다 (이름만, v0.2.1에서도 변경 없음).

`v0.2.1` pack에 들어 있는 항목:

- `./server`의 native `Plugin.Definition` default export. 이름 있는 `createNativeServer`
  팩토리가 `{ id: "rtrt-agent", setup }` 형태로 생성합니다. v1 `RtrtProvenance`
  경로는 그대로 별도입니다.
- `package.json`의 `exports`에서 `./server`와 `./tui`에 사용하는 single-element 배열.
  OpenCode 1.18.33 legacy resolver는 배열을 무시하고 `main`인 `index.js`로 fallback
  하며, native 2.0.20은 정상적으로 해석합니다.
- v1 TUI statusline은 여전히 별도의 `app_bottom` 등록입니다. v2 native
  `rtrt-statusline.tsx`는 `tui/v2/` 아래에 있고 2.x에서만 동작하며 v1 연결은 그대로입니다.
- Native `prompt.footer.status` 등록과 server·TUI hook의 cleanup.

Tool-arg provenance, 대시보드 자동 시작, 명시적 `rtrt_dashboard_open`은 모두 native
server 뒤에 있습니다. Permission 정책은 fail-closed이며 명시적 `deny`는 v1 broker와
v2 evaluator 양쪽에서 그대로 보존됩니다. 외부 Claude CLI permission broker 지원과
per-call shell identity / provider-limit recovery는 native v2 build에서 **지원되지
않습니다**. 이는 2.0.20 public API에 해당 표면이 없기 때문이며 어떤 workaround로도
자동 승인되지 않습니다. 기본 deny 정책은 v1 broker에서와 동일합니다.

### Native server와 native TUI 모두 설정하기

Native OpenCode 2.0.20은 플러그인 설정을 위해 두 파일을 읽으며, 이 둘은 같은
표면이 아닙니다.

- `opencode.json`은 **server inventory**를 제어합니다. 패키지는 string source
  target을 노출하고 server options(2.0.20 schema 기준)은 이 표면에서 전달되지
  않습니다.
- `<OPENCODE_CONFIG_DIR>/cli.json`은 **native CLI options**를 제어합니다. native
  host는 project/server의 `opencode.json`과 별도로 `cli.json`(또는 CLI config content
  override)을 읽습니다. 패키지의 TUI `options`(특히 `bin`)는 여기에 들어가야 합니다.

`bin`은 explicit와 omitted를 구분합니다. **explicit** `options.bin`은 실제
`rtrt` 실행 파일의 absolute 경로여야 합니다. 설정되면 native 2.0.20은
`RTRT_BIN`, `PATH`, 그 외 후보로 절대 fallback하지 않습니다. invalid,
missing, non-absolute explicit bin은 footer에 `N/A | STATE UNKNOWN | COST N/A
| CTX N/A | DEGRADED`를 표시합니다. `options.bin`이 **omitted**이면 공유
runner가 먼저 `RTRT_BIN`을 보고 그 다음 `PATH`의 `rtrt`을 조회합니다. 둘 다
없으면 같은 unavailable 상태를 표시합니다. server options(예: `bin` 값)은
`opencode.json`에서 TUI로 전달되지 않으며, `cli.json`에도 함께 적혀 있을 때만
TUI에 도달합니다. 두 파일에 같은 `options.bin`을 설정하세요.

디스크에 있는 실제 release binary 예시입니다 (native fixture가 실행한 형식 —
두 파일 모두 같은 `plugins` 항목을 가집니다).

```jsonc
// /absolute/path/to/checkout/opencode.json  (server inventory)
{
  "plugins": [
    {
      "package": "/absolute/path/to/rtrt-agent",
      "options": { "bin": "/absolute/path/to/rtrt-0.2.1/rtrt" }
    }
  ]
}
```

```jsonc
// /absolute/path/to/<OPENCODE_CONFIG_DIR>/cli.json  (native TUI options)
{
  "plugins": [
    {
      "package": "/absolute/path/to/rtrt-agent",
      "options": { "bin": "/absolute/path/to/rtrt-0.2.1/rtrt" }
    }
  ]
}
```

두 파일은 서로 다른 host 표면을 설정합니다. native CLI는 TUI 옵션을 소유하며
`opencode.json`만으로 그 옵션을 가져오지 않습니다. server도 옵션을 전달하지
않습니다. `bin`을 상대 경로, `PATH` 검색, 자동 감지된 release 경로로 대체하는 것은
패키지에서 지원하지 않습니다. 기본 absolute binary는 운영자의 release 빌드 자체입니다.

### 로컬 소스 checkout (npm 설치 없음)

로컬 checkout으로 개발할 때는 OpenCode의 native `plugins` config가 디렉터리 참조를
받습니다. 정확한 경로는 checkout에 따라 다르므로 형식만 보여 줍니다 (두 파일 모두
실행 fixture를 그대로 반영).

```jsonc
// opencode.json (server inventory)
{
  "plugins": [
    {
      "package": "/absolute/path/to/rtrt/plugins/opencode",
      "options": { "bin": "/absolute/path/to/rtrt" }
    }
  ]
}
```

```jsonc
// <OPENCODE_CONFIG_DIR>/cli.json (native TUI options)
{
  "plugins": [
    {
      "package": "/absolute/path/to/rtrt/plugins/opencode",
      "options": { "bin": "/absolute/path/to/rtrt" }
    }
  ]
}
```

이 두 파일 형식은 격리된 HOME/XDG 환경에서 실제 `@opencode/cli@2.0.20` host로 로컬
검증했습니다. 그 정확한 버전에서 `server.js`가 active이고 native TUI 탐색 결과는
`features.tui: true`이며 RTRT stdio MCP가 연결됩니다. TUI strict typecheck, 번들
빌드, 120열·40열 headless footer 렌더링도 실제 release `rtrt` binary와 의도적으로
설치하지 않은 binary(missing → `N/A | STATE UNKNOWN | ...`)에 대해 통과합니다.
검증은 전체 interactive OpenCode TUI를 다루지 않으며 다른 OpenCode 버전을 보장하지
않습니다. 검증 대상은 정확히 테스트한 `@opencode/cli@2.0.20` host에 한정됩니다.

### Native MCP 형식 (정보 제공, OpenCode 2 공식 schema)

Native local MCP 형식은 `mcp.servers.rtrt.type: "local"`과
`["/absolute/path/to/rtrt-mcp", "--transport", "stdio"]` 같은 `command` 배열을
사용합니다. 옵션 이름은 `disabled`와 `codemode`이며 sandbox source config에서는 둘
다 `false`입니다. Release에 고정된 v1 등록은 `rtrt-agent@0.2.1`입니다.
`rtrt setup --agent opencode --apply`는 여전히 v1 설정을 기록하며 native v2 설치가
아닙니다. Setup 자체는 npm 패키지를 설치하지 않고, OpenCode가 시작할 때 설정된
패키지를 해석합니다.

게시된 `rtrt-agent@0.1.7`은 npm registry에서 그대로입니다. Sandbox의 기본 v2 설정은
MCP-only이며 staged native plugin·footer·forwarding adapter는 그 경로에서
활성화되지 않습니다. 이 소스 작업은 Docker 배포, `rtrt collector` 서비스, 운영자
측 `forward` 배선 또는 다른 운영 서비스를 promote, 활성화, 연결하지 않습니다 — 이
표면은 분리되어 있고 v0.2.1 소스 변경으로 켜지지 않습니다.

### Windows native 지원

npm 패키지의 dashboard supervisor는 Windows에서 동작합니다 (`.rtrt`, `dashboard`,
`startup.lock`, `dashboard.env`, `bootstrap.html` ACL 적용을 번들 PowerShell 정책으로
수행하고 binary trust, 일반 open 경로를 처리하며 `dashboard-acl.test.mjs`,
`dashboard-binary.test.mjs`, `dashboard-open.test.mjs`로 검증). supervisor test는
Unix에서 같은 표면을 검증하며 실제 Windows CI lane에는 포함되지 않습니다. 실제
Windows CI 실행은 아직 기록되어 있지 않습니다. npm 패키지의 Windows 동작 인정은
**`dashboard-acl.test.mjs`, `dashboard-binary.test.mjs`, `dashboard-open.test.mjs`
에서 Windows-only skip이 0개인 실제 `windows-latest` CI 실행에 좌우됩니다**. 이
README는 본 환경에서 해당 gate가 이미 통과했다고 주장하지 않습니다.
`rtrt-dashboard-open` 바이너리 경로는 Windows에서 출시되지만 Rust CLI의
`rtrt service open`은 Windows에서 거부되므로 <http://127.0.0.1:7311/>을 열고
대시보드 bootstrap prompt에만 token을 입력하세요.

## 라이선스

MIT
