# 사용법

[English](USAGE.md) | **한국어**

이 문서는 v0.1.2 기준 `rtrt` CLI, `rtrt-mcp` 서버, `rtrt-dashboard` 웹 UI 사용법입니다.

## 빠른 차림표

- 룰 기반 압축: `rtrt compress -l ultra` (LLM 불필요)
- ML 압축: `rtrt compress --ml --ratio 0.4`
- LLM 압축: `rtrt compress --llm --provider openai-compat --model llama3.2`
- 메모리 리콜: `rtrt memory recall --project rtrt --query auth --filter "source=claude"`
- 비용 인지 라우팅: `rtrt route --explain "prompt"` / 사용량·헤드룸: `rtrt usage`
- 보안 스캔: `rtrt security scan --profile ai-default`
- 프로젝트 표준화: `rtrt migrate --apply` / `rtrt project refresh --apply`
- MCP HTTP: `RTRT_MCP_HTTP_TOKEN="$TOKEN" rtrt mcp --transport http`
- 벤치: `rtrt benchmark`

## CLI

```text
rtrt --help
```

### `rtrt compress`

표준 입력을 읽어 압축 결과를 표준 출력에 씁니다.

```bash
# 규칙 기반 (기본)
echo "Sure, I'd be happy to help. The bug is really in the parser." \
  | rtrt compress -l ultra

# LLM 기반 (어떤 프로바이더든; Ollama 예시)
echo "I think the bug is, perhaps, in the parser..." | rtrt compress --llm \
  --provider openai-compat --base-url http://127.0.0.1:11434/v1 --model llama3.2
```

플래그:

- `-l, --level <lite|full|ultra|extreme>` — 압축 강도. 기본값 `full`.

레벨별 규칙 (누적):

- `lite` — 필러(`just`, `really`, `basically`, `actually`, `simply`, `literally`, `honestly`, `frankly`, `truly`, `essentially`, `kind of`, `sort of`) + 다중 공백/개행 압축.
- `full` — `lite` + 인사말(`sure`, `certainly`, `of course`, `happy to`, `let me`, `I'll`, `I can`, `I would`) + 헤지(`I think`, `perhaps`, `maybe`, `probably`, `it seems`, `if I recall correctly` 등) + 담화 표지(`moreover`, `however`, `as you can see`, `needless to say`, `obviously`, `clearly` 등) + 메타 표현(`it is important to note that`, `as we mentioned earlier` 등).
- `ultra` — `full` + 관사(`a`/`an`/`the`) + 관용구 축약(`due to the fact that` → `because`, `in order to` → `to`, `at this point in time` → `now`, `a number of` → `several`, `the majority of` → `most`, `for instance` → `e.g.` 등).
- `extreme` — `ultra` + 강조 부사(`very`, `extremely`, `quite`, `rather`, `fairly`, `somewhat`, `highly`).

코드 블록(` ``` `, ` ` `), URL, `"인용 문자열"`은 규칙 적용 전에 보호되어 원문 그대로 복원됩니다. 시크릿 패턴(AWS / GitHub / OpenAI / Anthropic / Slack / Bearer / private-key / `api_key=…`)은 규칙 패스 **이전**에 `<REDACTED:<kind>>`로 치환됩니다.

### `rtrt signatures`

tree-sitter로 함수 body 제거, 최상위 시그니처만 남김. 코드 중심 LLM 컨텍스트에 최적.

```bash
rtrt signatures --lang rust < crates/rtrt-providers/src/anthropic.rs
# 8972 bytes → 1948 bytes  (실 파일 기준 78% 절감)
```

현재 `--lang rust`만 지원. 다른 언어는 해당 `tree-sitter-<lang>` 그래머를 활성화하면 됨. `crates/rtrt-compress/src/treesitter.rs` 참조.

### `rtrt proxy`

명령 이름을 알려주면 그 명령의 표준 출력을 필터링합니다.

```bash
git status | rtrt proxy "git status"
cargo build 2>&1 | rtrt proxy "cargo build"
```

### `rtrt proxy-run`

명령을 실행하고 캡처한 출력을 에이전트에 전달하기 전에 필터링합니다. `proxy-run`은 감싼 명령의 종료 코드를 보존합니다.

```bash
rtrt proxy-run git status
rtrt proxy-run cargo test -p rtrt-memory
rtrt proxy-run --errors-only npm test
rtrt proxy-run --ultra-compact docker ps
rtrt proxy-run --raw cargo build
```

플래그:

- `--raw` — 실행 기록은 남기되 캡처한 출력을 그대로 출력.
- `--errors-only` — 명령 전용 필터가 없을 때 오류/경고로 보이는 줄만 유지.
- `--ultra-compact` — 명령 전용 필터가 없을 때 ANSI escape 제거 + 반복 줄 접기.

빌트인 필터는 `git status/diff/show/branch/stash/log`, `cargo check/clippy/build/test/nextest`, `ls`, `grep`, `rg`, `find`, `cat`, `curl`, `wget`, `gh`, `docker`, `kubectl`, `pytest`, `go test`, `npm` / `npx` / `pnpm`, `pip`, `tsc`, `eslint`, `prettier` 전반의 34개 명령 패턴을 지원합니다. 필터는 도메인별 모듈로 나뉩니다.

`rtrt proxy`는 명시적 파이프 워크플로용으로 계속 사용할 수 있습니다. 매칭되지 않는 명령은 원문이 그대로 통과합니다.

### `rtrt hook proxy-rewrite`

Claude Code는 `PreToolUse` Bash 훅으로 Command Optimizer를 투명하게 실행할 수 있습니다.

```bash
rtrt setup --agent claude --apply
```

설치된 matcher는 Bash 도구 호출을 대상으로 하며, 축소 가능한 명령을 `rtrt proxy-run ...`으로 재작성합니다. 파이프, `&&`, 리다이렉트, 이미 `rtrt proxy-run`으로 감싼 명령은 건너뜁니다. Cursor, Codex, Windsurf, opencode 등 MCP 인지 에이전트는 Claude 전용 훅 대신 `rtrt-mcp`를 통해 Command Optimizer 도구를 받습니다.

훅 러너가 직접 호출할 수도 있습니다.

```bash
rtrt hook proxy-rewrite
```

### 멀티 에이전트 코디네이션 경계

RTRT는 team 명령, scheduler, roster, worker protocol 또는 `team_dispatch` MCP 도구를 제공하지 않습니다. 멀티 에이전트 코디네이션은 외부 에이전트 런타임이 담당합니다. RTRT는 압축, 메모리, 프로바이더 라우팅과 페일오버, 보안 스캔, setup 통합 및 provenance에 집중합니다.

### OpenCode npm 플러그인과 setup 마이그레이션

`rtrt-agent@0.1.2`를 `npm install rtrt-agent@0.1.2`로 설치하고 OpenCode의 단수 루트 `plugin` 키에 직접 등록할 수 있습니다.

```json
{ "plugin": ["rtrt-agent@0.1.2"] }
```

npm 패키지는 RTRT provenance 및 permission hook만 내보냅니다. TUI 스테이터스라인은 npm에 포함되지 않으며 계속 setup이 관리합니다.

전체 설치에는 다음을 권장합니다.

```bash
rtrt setup --agent opencode --apply
```

Setup 자체는 npm 설치를 수행하지 않습니다. 먼저 정확한 `rtrt-agent@0.1.2` 등록과 모든 대체 관리 asset을 기록합니다. 이 기록이 모두 성공한 뒤에만 인식 가능한 legacy RTRT plugin 항목을 마지막으로 정리하며, 그 전 단계에서 실패하면 legacy runtime을 보존합니다. 설정된 npm 패키지는 OpenCode가 시작할 때 설치합니다. 외부 plugin string, tuple, object와 인식할 수 없는 legacy 항목은 기존 순서와 내용을 유지합니다. Uninstall은 RTRT 소유 항목만 제거합니다. 해석된 config root는 비어 있지 않은 `OPENCODE_CONFIG_DIR`, 다음 `$XDG_CONFIG_HOME/opencode`, 마지막 HOME/USERPROFILE fallback root이며 HOME 기반 system에서는 `~/.config/opencode`입니다. 공존은 OMO 4.19.4를 대상으로 CI에서 검사하고 OpenCode 1.18.29에서 직접 검증했으며, 어느 쪽도 미래 버전 지원을 보장하지 않습니다. 통합 릴리스 워크플로는 버전이 일치하는 Rust artifact와 npm 패키지 게시를 담당하며, 이 문서는 게시가 이미 완료되었다고 주장하지 않습니다.

### OpenCode 영구 스테이터스라인

```bash
rtrt setup --agent opencode --apply
# Setup 후 OpenCode 재시작
```

Setup은 해석된 OpenCode config root 아래에 관리 대상 TUI file 두 개를 설치합니다.

- `tui/rtrt-statusline.tsx`
- `tui/rtrt-statusline-core.mjs`

또한 활성 OpenCode TUI config의 `plugin` 배열에 tuple 하나를 추가합니다.

```json
["./tui/rtrt-statusline.tsx", {"bin": "/absolute/path/to/rtrt"}]
```

Config resolver는 해당 root 아래 기존 `tui.json`을 우선하고, 없으면 기존 `tui.jsonc`, 둘 다 없으면 그곳에 새 `tui.json`을 선택합니다. Setup은 plugin 배열을 교체하지 않고 document를 parse/merge하므로 외부 plugin, 관련 없는 key, RTRT tuple의 기존 `bin` 외 option을 보존합니다. 반복 setup은 idempotent합니다. 관리 대상 TUI 경로에 인식할 수 없는 기존 file이 있으면 덮어쓰지 않습니다.

Plugin은 application 아래쪽의 영구 `app_bottom` surface 하나만 등록합니다. `session_prompt_right`는 의도적으로 등록하지 않습니다. statusline을 OpenCode prompt 렌더링 경로 밖에 두어 streaming update가 키 입력이나 interrupt 처리를 지연하지 않게 합니다.

Line은 시작 즉시, 범위가 지정된 project 및 session lifecycle/status event 후(연속 event는 750 ms debounce), 그리고 15초마다 갱신됩니다. Session event는 활성 application line만 갱신합니다. Session economics는 이 범위 이벤트에서 비반응형 snapshot으로 읽으므로, 빈도가 높은 file 및 message-part update는 statusline을 다시 render하거나 statusline 작업을 실행하지 않습니다. Refresh는 겹쳐 실행되지 않습니다. TUI plugin은 OpenCode process 시작 시 load되므로 설치 또는 upgrade 후 OpenCode를 재시작해야 합니다. 이미 실행 중인 process에는 statusline이 추가되지 않습니다.

#### OpenCode JSON contract

TUI는 shell 없이 `rtrt statusline --opencode`를 호출합니다. 같은 compact single-line JSON contract를 직접 확인할 수 있습니다.

```bash
rtrt statusline --opencode --cwd "$PWD" --session session-id --width 120
rtrt statusline --opencode --cwd "$PWD" --width 80 --budget-ms 120 --no-git
rtrt statusline --opencode --cwd "$PWD" --width 120 --refresh
```

Command는 stdin을 기다리거나 읽지 않습니다. Version 1 object는 `v`, `ts`, `took_ms`, `stale`, `degraded`, `project`, `cwd`, `data`, priority가 있는 `segments`를 포함하며 각 segment는 `id`, `text`, `tone`, `pri`를 가집니다. `--refresh`는 선호하는 fresh snapshot을 우회하고, `--no-git`은 충분한 폭에서도 Git 수집을 끕니다.

| 폭 | 표시 가능한 segment |
|----|---------------------|
| `< 60` | 가능한 경우 전체 savings(`Σ`) + Output Optimizer style |
| `60-99` | Project, style, savings, provider headroom |
| `>= 100` | 위 항목 + Git, optional model, session, memory aggregate |

TUI는 실제 폭에 맞을 때까지 낮은 priority segment를 추가로 제거합니다. OpenCode SDK 1.18.13은 현재 선택을 `Session.model`로 노출합니다. Plugin은 session state에서 provider와 model을 해석하고 안전한 `provider/model`만 argv-only `--model`로 snapshot에 전달합니다. 과거 message에서 선택 model을 추론하지 않습니다. 수동 caller도 `--model <provider/model>`을 전달할 수 있습니다.

#### Session 지표

Session 지표는 RTRT CLI usage ledger와 독립적으로 OpenCode SDK state에서 파생됩니다.

| 표시 | 의미 |
|------|------|
| `MODEL` | SDK provider catalog로 해석한 현재 `Session.model`입니다. 안전한 `provider/model`을 RTRT snapshot에 전달합니다. |
| `COST` | OpenCode가 보고하거나 계산한 `Session.cost` 추정치입니다. 양수는 `~$<amount>`, 0은 포함/무료/가격 미책정을 구분할 수 없다는 뜻의 `$0?`, 값이 없으면 `N/A`로 표시합니다. |
| `CTX` | Output이 있고 error 없이 가장 최근에 완료된 assistant turn의 `(input + output + reasoning + cache read + cache write) / context limit` 비율입니다. 해당 turn의 정확한 provider/model limit를 사용하며 100%로 clamp하지 않아 overflow가 그대로 보입니다. 정확한 model limit가 없으면 `N/A`입니다. |
| `STATE` | 현재 session 상태인 `BUSY`, `RETRY`, `IDLE`입니다. OpenCode는 active status map에서 idle session을 생략하므로 알려진 session에 active status가 없으면 `IDLE`로 표시합니다. |
| `5H` | Bridge cache가 fresh하고 reset이 만료되지 않았을 때 공식 Claude Code `rate_limits.five_hour` 사용률과 reset countdown을 표시하며, 그 외에는 `N/A`입니다. |
| `WEEK` | 같은 freshness/reset 규칙을 적용한 공식 Claude Code `rate_limits.seven_day` 사용률과 reset countdown이며, 그 외에는 `N/A/not exposed`입니다. |

`COST`와 `CTX`는 위 session semantics를 그대로 유지합니다. Rate-limit bridge는 두 값을 파생하지 않고 account data로 대체하지도 않습니다.

#### 공식 Claude Code rate-limit bridge

Claude Code는 설정된 statusline command에 공식 `rate_limits` object를 전달합니다. `rtrt setup --agent claude --apply`가 이 source/writer를 설치하고 OpenCode setup은 reader/display만 설치하므로, OpenCode만 실행해서는 cache가 갱신되지 않습니다. `rtrt statusline --rich`가 payload를 받으면 cache version, capture time, 각 window의 숫자형 `used_percentage`와 `resets_at`만 기록합니다. Credential, OAuth token, session/prompt 식별자, transcript path/content, context/token count 또는 다른 Claude statusline field는 저장하지 않습니다. OpenCode의 `rtrt statusline --opencode` collector는 이 local cache만 읽으며 credential을 조회하거나 network request를 만들지 않습니다.

기본 cache는 사용자 전용 `~/.rtrt/statusline/claude-rate-limits.json`입니다. Unix에서 RTRT는 mode `0700` directory 아래 mode `0600` file로 기록하고, 읽을 때 안전하지 않은 link나 permission을 거부합니다. Claude statusline이 기본 최대 age인 15분 안에 갱신됐고 `resets_at`이 아직 미래인 window만 OpenCode에 표시할 수 있습니다. 각 window는 독립적으로 검사합니다. Stale, expired, absent, malformed, unsafe cache data는 0이나 추정 quota가 아니라 `N/A`로 처리합니다.

| 환경 변수 | 기본값 | 목적 |
|-----------|--------|------|
| `RTRT_CLAUDE_RATE_LIMIT_CACHE` | `~/.rtrt/statusline/claude-rate-limits.json` | Claude writer와 OpenCode reader가 함께 쓰는 private bridge-cache path 재정의. |
| `RTRT_CLAUDE_RATE_LIMIT_MAX_AGE_SEC` | `900` | 허용 cache age를 초 단위로 재정의(`1`-`86400`). 잘못된 값은 `900` 사용. |

이는 별도 quota API가 아니라 Claude Code 자체가 제공하는 동일한 공식 `rate_limits` data의 bridge입니다. RTRT는 문서화되지 않은 OAuth endpoint를 의도적으로 polling하지 않습니다. 그런 방식은 Claude credential을 획득, 저장 또는 전송해야 하며 지원되지 않는 Terms of Service 동작과 불안정한 response schema에 의존합니다.

`WEEK`은 `rate_limits.seven_day`에서 온 공식 provider window입니다. 반면 RTRT rolling 7d provider-usage ledger는 local에서 관측한 invocation만 기록합니다. 이 ledger는 activity history이지 provider quota가 아니며 `5H`나 `WEEK`을 채우거나 fallback으로 사용하지 않습니다.

#### Local 수집과 실패 상태

수집은 local-only best-effort입니다. CLI 기본 전체 budget은 120 ms이고, TUI는 응답하지 않는 CLI child를 1.5초 후 종료합니다. Collector는 유효 local config, 크기가 제한된 local savings/usage file과 cache, busy timeout 0 및 deadline interrupt를 적용한 read-only SQLite, 최대 50 ms local Git status만 읽습니다. Git은 optional lock, filesystem monitor, untracked-file 열거, ahead/behind, submodule 작업을 끕니다. Statusline 수집은 network request를 만들지 않고 OpenCode/Claude transcript를 scan하지 않습니다.

`degraded`와 `stale`의 의미는 다릅니다.

- `degraded`는 폭 tier상 수집 대상이지만 사용할 수 없거나 budget 안에 완료하지 못한 collector와 non-canonical `cwd`, 만료된 `budget` 같은 상태를 나열합니다. 다른 유효 segment는 계속 렌더링되므로 partial result는 command failure가 아닙니다.
- `stale: true`는 전체 budget이 만료됐거나 stale Git cache를 사용했음을 뜻합니다. TUI는 line 전체를 흐리게 하고 `stale`을 붙입니다.
- 이후 child 호출이 timeout, non-zero exit, invalid JSON으로 실패하면 TUI는 마지막 정상 payload를 유지하고 흐리게 표시하며 `stale`을 붙입니다.
- 정상 payload를 받은 적이 없으면 흐린 `rtrt · n/a` fallback을 표시합니다.

#### 제거

```bash
rtrt uninstall --agent opencode --apply
# Uninstall 후 OpenCode 재시작
```

Uninstall 순서는 의도적입니다. 먼저 RTRT service와 integration을 중지·제거한 뒤 managed binary/file을 제거합니다. OpenCode uninstall은 해석된 config root 아래 `tui.json` / `tui.jsonc`에서 RTRT tuple을 제거하고, 그곳의 두 TUI file에서 인식 가능한 RTRT 관리 block만 제거합니다. 외부 plugin, 관련 없는 config key, RTRT 소유가 아닌 file content는 유지됩니다. 수정됐거나 인식할 수 없는 관리 file content도 삭제하지 않고 보존합니다. 이 command는 다른 RTRT 관리 OpenCode rules, provenance plugin/bridge, `mcp.rtrt` 항목도 제거합니다. 실행 중인 OpenCode process가 TUI plugin을 unload하려면 재시작이 필요합니다. Typed managed path와 MCP entry는 symlink 및 안전하지 않은 ownership/type 변경을 거부합니다.

### OpenCode-to-Claude provenance

```bash
rtrt setup --agent opencode --apply
```

Setup은 전역 OpenCode plugin을 설치합니다. OpenCode setup은 전역 `~/.claude.json`을 읽거나 쓰거나 요구하지 않으며, 전역 Claude provenance hook도 설치하지 않습니다. 각 exact direct `claude -p` invocation은 global/user/project Claude setting source를 비활성화한 뒤 exact `SessionStart` provenance hook을 포함한 하나의 ephemeral strict settings object를 주입합니다. 또한 strict permission-only RTRT MCP config 하나를 주입하며, 기존 foreign/shared Claude MCP config는 무관하고 보존됩니다. Plugin은 tool call별 안정적인 invocation UUID를 만들고 부모 project/session/call, 활성 agent, cwd, worktree를 RTRT MCP argument와 직접 shell 환경에 전달합니다. RTRT가 실행하는 call에는 명시적인 child session ID가 붙고, 주입된 hook은 child의 최초 부모 소유자를 저장하며 이후 resume가 덮어쓰지 못하게 합니다. Transcript capture와 boot-time 재귀속은 경로 추정보다 이 영구 join을 먼저 사용하고, MCP 자동 캡처는 전달된 부모 project를 fallback으로 사용할 수 있습니다. 설치 후 OpenCode를 재시작해야 합니다.

### OpenCode-to-Claude permission prompt bridge

직접 Claude CLI lane은 RTRT Linux bwrap shell confinement 밖에서 다음 표준 flag를 사용합니다.

```text
--permission-prompt-tool mcp__rtrt__permission_prompt
```

각 exact argv launch는 global/user/project setting source를 비활성화하고
Claude Code 공식 sandbox의 `enabled=true`,
`failIfUnavailable=true`, `allowUnsandboxedCommands=false`, strict network
allowlist, project-only home-read exception, credential/environment scrub,
exact `SessionStart` provenance hook, strict permission-only RTRT MCP config 하나를
포함한 ephemeral strict settings object를 주입합니다. Claude Linux dependency가
없으면 fail closed하며 RTRT는 설치하지 않습니다. `socat`은 Claude Code host의
optional prerequisite일 뿐 bundle/install되지 않고 setup이 사용자 승인을
뜻하지도 않습니다.

#### 엄격한 Linux OpenCode shell confinement

`rtrt setup --agent opencode --sandbox --apply`는 VM이 아닌 setup 소유
confinement입니다. Operator가 설치한 고정 경로 `/usr/bin/bwrap` 또는
`/bin/bwrap`만 사용하며 namespace/network 격리, nested user namespace 비활성화,
환경 정리, private `/tmp`, read-only system/tool cache, canonical
project/Git metadata 쓰기를 적용합니다. 실행 사용자와 실제 Git-worktree
경계를 검증하고, 지원되지 않거나 사용할 수 없는 host는 fail closed합니다.
이는 OpenCode shell만 제한하며 직접 Claude launch는 RTRT bwrap 안에서 실행하지
않습니다.

RTRT 전용 bridge입니다. OpenCode setup은 Claude global config나 기존 foreign/shared MCP entry를 건드리지 않습니다. Broker는 기존 provenance plugin 안에 있으며 standalone daemon, script, service, third-party plugin이 아니고 dependency도 추가하지 않습니다. `127.0.0.1` ephemeral port에 bind하고 invocation마다 random token/nonce와 부모 session/call identity를 사용합니다.

`rtrt-mcp`는 Claude tool request의 제한된 field만 전달합니다. Raw prompt, credential, token, nonce, raw tool input을 자동 capture하거나 persist하지 않습니다. OpenCode v2 native permission은 기존 project/global policy를 먼저 평가하고 필요할 때 native once/always/reject UI를 표시합니다. **always** persistence는 OpenCode만 소유하며 RTRT는 별도 persistence policy를 만들지 않습니다. Approval은 wall-clock timeout이 없고 native OpenCode처럼 decision 또는 lifecycle cancellation까지 기다립니다. Connect establishment만 짧게 제한합니다. Malformed data, auth/connect failure, tool/session cancellation, disconnect, disposal은 기본 deny입니다.

Native Task inheritance는 바뀌지 않았습니다. OpenCode setup은 writable project root 밖의 executable을 요구합니다. 설치된 `~/.cargo/bin/rtrt`를 사용하고 project `target/` binary는 절대 사용하지 마세요. Claude Code permission-prompt-tool support는 2.1.219부터 2.1.221까지 검증했으며, 설치된 OpenCode SDK contract는 1.18.11입니다. 더 넓은 minimum compatibility는 주장하지 않습니다. 어느 도구든 upgrade 후 OpenCode를 재시작하고 `rtrt setup --agent opencode --apply`를 다시 실행하세요.

### 프로젝트 로컬 임시 파일

### 프로젝트 전용 OpenCode launcher

외부 terminal에서 OpenCode를 RTRT 경유로 실행합니다. OpenCode 인자는 반드시 `--` 뒤에 둡니다.

```bash
rtrt opencode --project /path/to/checkout -- --model provider/model
# checkout 안에서는:
rtrt opencode --
```

Launcher는 `--project` 또는 cwd에서 immutable project identity를 파생합니다. Linked worktree는 identity/data를 공유하지만 OpenCode는 선택한 writable checkout boundary에서 시작합니다. 서로 다른 linked-worktree boundary는 각각 승인하며 basename이 같은 repository도 구분합니다.

Eligible Linux/WSL 설치는 `rtrt setup --agent opencode --sandbox --machine-only --apply`를 자동 실행합니다. 설치된 RTRT와 fixed root-owned usable bubblewrap을 검증하고 project가 빈 machine registry를 만들며 cwd는 승인하지 않습니다. `--no-setup` / `RTRT_NO_SETUP=1`로 끕니다. 이후 `rtrt opencode --`는 exact managed state를 재검증하고 shared registry lock 아래 명시적으로 실행한 canonical checkout만 승인합니다. Tampered 상태는 fail closed하며 launcher는 global config나 repository를 repair하지 않습니다.

`~/.rtrt/projects/<slug>/opencode/` 아래 project-private `XDG_DATA_HOME`, `XDG_STATE_HOME`, `OPENCODE_DB`를 설정하고 global XDG config는 유지하므로 설치된 config, agent, plugin을 계속 사용합니다. Private directory mode는 `0700`, launcher가 만든 file은 `0600`입니다. 안전한 regular global `opencode/auth.json`은 private destination이 없을 때 한 번만 복사합니다. Nested OpenCode/model-shell session, unsafe/symlink executable, 다른 project를 고르는 directory 인자를 거부합니다. Shell 없이 검증된 OpenCode executable과 argv를 직접 실행합니다.

첫 launch 전에 어느 cwd에서든 machine-wide global SQLite session graph를 migration할 수 있습니다.

```bash
rtrt opencode sessions status   # read-only probe
rtrt opencode sessions dry-run  # 정확한 계획, write 없음
rtrt opencode sessions apply    # lock·atomic·idempotent migration
```

Migration은 원본 global DB를 WAL visibility가 있는 read-only mode로 직접 열며 DB/WAL 전체 snapshot을 만들지 않고 원본을 backup으로 유지합니다. `session.directory`를 우선하고 안전한 project metadata를 fallback으로 사용합니다. Canonical RTRT identity로 linked worktree는 합치고 basename이 같은 독립 repository는 분리합니다. Session ID, parent/child graph, message, part, todo, workspace/share/projection/event row, explicit index와 지원되는 resume data를 prompt content 검사 없이 opaque copy합니다. Primary key가 없는 table의 duplicate row multiplicity도 정확히 유지합니다. Account, credential, control-account, persistent permission/approval row는 제외합니다. 삭제됐거나 귀속할 수 없는 session은 private `legacy-global`에 보존합니다. Prompt-history JSONL은 별도 보존 대상이며 project 귀속 data라고 주장하지 않습니다. 명시적 `apply`는 strict conflict 시 rollback하며 지원하지 않는 trigger/view를 누락하지 않고 fail closed합니다. Launch 전 incremental catch-up은 충돌한 private row를 유지하면서 안전한 missing row를 복사하지만, 모든 catch-up error와 lock contention은 content-free warning만 내고 유효한 private launch를 막지 않습니다. Content-free DB/WAL generation stamp로 변경 없는 source read를 생략하며 이후 WAL 증가는 놓치지 않습니다.

직접 `opencode` 실행은 계속 global state를 사용합니다. Setup의 `history_previous=none`, `history_next=none`은 TUI history navigation만 끄며 global history write를 막지 않습니다. 알려진 prompt-history file만 확인하거나 명시적으로 quarantine할 수 있습니다.

```bash
rtrt opencode history-status
rtrt opencode history-quarantine          # dry-run
rtrt opencode history-quarantine --apply  # rename; delete하지 않음
```

Quarantine은 exact known path와 `.rtrt-quarantine` sibling만 사용합니다. Home을 scan하거나 OpenCode data를 destructive migration하지 않습니다.

임시 디렉터리 해석 순서는 고정입니다. `RTRT_TMP_DIR`가 모든 기본값을 재정의하고, 그렇지 않으면 발견된 project는 `<main-linked-repository-root>/.rtrt/tmp`를 사용하며(linked worktree는 main repository로 해석), project가 없는 실행은 공유 `<OS temp>/rtrt` 대신 OS temp 아래의 사용자 전용 디렉터리(Unix는 `<OS temp>/rtrt-<uid>`, 기타 플랫폼은 동등한 사용자 전용 디렉터리)를 사용합니다. RTRT는 symlink와 디렉터리가 아닌 후보를 거부하며, Unix에서는 현재 사용자 소유권과 `0700` mode도 검증합니다. RTRT가 project를 발견했지만 로컬 임시 디렉터리를 만들 수 없으면 OS temp로 조용히 빠져나가지 않고 오류를 반환합니다.

OpenCode plugin은 session마다 `<main-linked-repository-root>/.rtrt/tmp/opencode/<session>`을 `TMPDIR`, `TEMP`, `TMP`에 지정합니다. 이는 child process의 임시 파일 범위만 제어합니다. 현재 plugin SDK에는 OpenCode native Task의 내부 worktree root를 바꾸는 기능이 없습니다. 이미 `/tmp/opencode`에서 실행 중인 session은 영향을 받지 않으며, 새 session 환경에 plugin 변경을 적용하려면 OpenCode를 재시작해야 합니다.

### `rtrt gain`

`~/.rtrt/proxy-stats.sqlite`에 저장된 Command Optimizer 절감량을 보여줍니다. 토큰 수는 `chars / 4` 기준의 라벨 달린 추정치입니다.

```bash
rtrt gain
rtrt gain --project rtrt
rtrt gain --history
rtrt gain --daily --graph
rtrt gain --weekly
rtrt gain --monthly
rtrt gain --format json
rtrt gain --reset --yes
```

리포트에는 전체 합계, 상위 명령, 프로젝트별 합계, 선택적 최근 히스토리, 일간/주간/월간 버킷 뷰가 포함됩니다.

### `rtrt discover`

Claude Code transcript를 스캔해 Command Optimizer로 축소 가능한 명령을 찾고 예상 절감량을 계산합니다.

```bash
rtrt discover
rtrt discover --project rtrt
rtrt discover --all --since 2026-06-01
rtrt discover --format json
```

### `rtrt route`

프롬프트에 가장 저렴하면서 쓸모 있는 라우트를 고르고(옵션으로 호출까지). 랭킹은 비용 계층 우선(로컬-무료 → 구독-정액 → API-종량), 계층 내부는 헤드룸 가중: `[limits]` 잔여 헤드룸이 ~15% 미만인 후보는 감점, 소진된 타깃은 최후 폴백으로 강등됩니다.

```bash
rtrt route --dry-run "summarise this diff"          # 호출 없이 결정만
rtrt route --explain "summarise this diff"          # 결정 + 랭크된 대안 + 헤드룸
rtrt route --prefer local "quick sanity check"      # --prefer cheapest|quality|local
rtrt route --failover "must succeed"                # 재시도 가능 오류 시 랭크 순회
rtrt route --target ollama --model llama3.2 "hi"    # 명시적 타깃 지정
```

### `rtrt call`

크로스-툴 브리지로 감지된 로컬 에이전트/프로바이더를 호출합니다.

```bash
rtrt call claude "explain this error"               # 타깃은 `rtrt detect` 결과
rtrt call ollama --model llama3.2 "ping"
rtrt call codex --mode cli --timeout 60 "review"    # --mode auto|cli|api
rtrt call claude --failover "must succeed"          # 다음 랭크 타깃으로 페일오버
rtrt call claude --format json "ping"
```

`--failover`는 재시도 가능 실패(rate-limit / quota / 429 / 5xx / 타임아웃)에서 다음 랭크 타깃으로 넘어갑니다.

### `rtrt usage`

타깃별 윈도우 사용량(5h / 24h / 7d)과 `[limits]` 일일 상한 대비 잔여 헤드룸을 표시합니다([설정 파일](#설정-파일) 참고). 추정 토큰 수(CLI 셸-아웃, ~chars/4) 행은 `~`로 표시됩니다.

```bash
rtrt usage
rtrt usage --format json
```

원장은 `~/.rtrt/provider-usage.tsv`(경로 재정의: `RTRT_PROVIDER_USAGE_PATH`), 최근 5000행 상한.

### `rtrt security`

프로파일 기반 보안 + 라이선스 스캔(secrets / licenses / deps / patterns / AI-artifact 엔진). 엔진 · 프로파일 상세는 [FEATURES.ko.md](FEATURES.ko.md#보안--라이선스-스캔) 참고.

```bash
rtrt security scan --profile ai-default --path . --json
rtrt security profile list
rtrt security profile show owasp-top-10
rtrt security gate --profile ai-default    # CI 게이트: 임계 이상이면 non-zero 종료
rtrt security init                         # 빌트인 프로파일을 ~/.rtrt/security/profiles/로 복사
```

### `rtrt migrate` / `rtrt project`

기존 저장소를 rtrt 프로젝트 표준으로 이관하고 일관성을 유지합니다. `migrate`와 `project refresh`는 기본 dry-run — `--apply`로 실제 기록합니다.

```bash
rtrt migrate                        # 계획만 (dry-run)
rtrt migrate --apply                # 이관 적용
rtrt project refresh --apply        # 원커맨드 별칭: 컨트랙트 렌더 → 표준 설정 → 감사
rtrt project status                 # 컨트랙트 · 에이전트 · 훅 · 스테이터스라인 · 메모리 연결 상태
rtrt project health                 # status + 더 깊은 라이프사이클 일관성 검사
rtrt project repair --dry-run       # 누락된 관리 섹션 추가 / 누락 에이전트 설치
```

`migrate` / `project refresh`는 프로젝트 레벨의 rtrt 소유 키 섀도(예: 프로젝트 `.claude/settings.json`의 `statusLine` 재선언)를 `.bak` 백업과 함께 제거해 프로젝트가 글로벌 베이스 커널을 따르게 합니다.

### `rtrt templates`

사용 가능한 템플릿을 나열합니다(빌트인 + 커스텀).

```text
design              [BuiltIn]  디자인 키트 문서 체인
dev                 [BuiltIn]  개발 시작 문서 체인
plan                [BuiltIn]  계획 문서 체인
standardization     [BuiltIn]  CLAUDE.md와 에이전트 정의를 담은 프로젝트 컨트랙트
```

커스텀 템플릿은 `~/.rtrt/templates/<name>/manifest.toml`에 두면 `[Custom]`으로 표시됩니다.

### `rtrt new`

템플릿으로 프로젝트를 만듭니다.

```bash
rtrt new dev ./hello \
  --var project_name=hello \
  --var author="Kim DaeHyun"
```

플래그:

- `--var key=value` — 템플릿 변수 지정(중복 가능).
- `--overwrite` — 대상 경로의 기존 파일 덮어쓰기.
- `--no-hooks` — 포스트-인스톨 훅(`git init`, `npm install` 등) 건너뛰기.

`--var project_name`이 없으면 대상 디렉터리 이름을 사용합니다.

### `rtrt info`

버전과 워크스페이스 크레이트 목록을 출력합니다.

## 게이트웨이 (`rtrt gateway serve`)

OpenAI 호환 클라이언트를 `http://127.0.0.1:7412/v1`에 연결하면, rtrt가 감지된
프로바이더들 사이로 요청을 자동 라우팅합니다. 환경 변수 하나로 Cursor, OpenAI
SDK, `llm`, Continue, 임의의 curl 스크립트가 rtrt 클라이언트가 됩니다. 기본은
루프백 바인딩입니다.

```bash
rtrt gateway serve --port 7412 --host 127.0.0.1
export OPENAI_BASE_URL=http://127.0.0.1:7412/v1
export OPENAI_API_KEY=unused      # 또는 설정한 RTRT_GATEWAY_TOKEN
```

엔드포인트:

- `POST /v1/chat/completions` — OpenAI Chat Completions(요청 + 응답; `stream:true`면 `chat.completion.chunk` SSE 스트림, 마지막에 `data: [DONE]`).
- `GET /v1/models` — 라우팅용 의사(pseudo) 모델과 감지된 모든 타깃/모델.
- `GET /healthz` — 라이브니스 프로브(토큰이 설정돼 있어도 항상 열림).

`model` 필드가 라우팅 전략을 고릅니다:

| `model` | 동작 |
|---------|------|
| `auto` / `""` / `rtrt/auto` | 전체 라우팅: 요청에서 능력(capability)을 추론한 뒤 헤드룸 인지 `select_route` + 랭크된 타깃에 대한 자동 페일오버. |
| `rtrt/cheapest` | 같은 랭크 목록, 가장 저렴한 비용 티어 우선. |
| `rtrt/best` | 같은 랭크 목록, 능력이 가장 높은 티어 우선. |
| `anthropic/claude-…`, `openai/gpt-…`, `ollama/…`, 또는 순수 모델 id | 모델 id 접두사로 기존 프로바이더 게이트웨이를 통해 디스패치. |

`auto` 계열의 능력 추론은 의도적으로 단순합니다: 코드 펜스(```` ``` ````)는
**code**, 그 외 긴 요청(약 2000자 초과)은 **reasoning**, 나머지는 일반 **chat**.

모든 디스패치는 라우터가 균형을 잡는 것과 동일한 사용량 원장(ledger)에
(재사용하는 기존 경로를 통해) 기록되므로, 별도 회계나 이중 기록이 없습니다.

보안:

- 기본 바인딩은 `127.0.0.1`. 토큰 없이 비루프백으로 바인딩하면 경고를 로깅합니다.
- `--token <T>` / `RTRT_GATEWAY_TOKEN`을 설정하면 `/v1/*`에 `Authorization: Bearer <T>`가 필요합니다(누락 시 401 + `WWW-Authenticate`; 상수 시간 비교). `/healthz`는 계속 열려 있습니다.

한계(정직하게): 텍스트 전용 브리지입니다. 라우팅 경로에서는 요청이 단일 프롬프트로
평탄화되므로, 툴 콜링 / 함수 콜링 / 비전 콘텐츠는 아직 전달되지 않습니다.
스트리밍은 버퍼링 방식입니다 — 라우팅된 답변을 전부 계산한 뒤 SSE 청크로
내보냅니다(CLI 모드 타깃은 전체 텍스트만 반환). 즉 `stream:true`는 와이어
호환이지만 토큰 단위 스트리밍은 아닙니다.

```bash
# 자동 라우팅, 비스트리밍
curl http://127.0.0.1:7412/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"auto","messages":[{"role":"user","content":"hello"}]}'

# 명시적 로컬 모델, 스트리밍
curl -N http://127.0.0.1:7412/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"ollama/gemma3:4b","stream":true,"messages":[{"role":"user","content":"hi"}]}'
```

## MCP 서버 (`rtrt-mcp`)

```bash
# stdio (기본; Claude Code / Codex / Cursor / Windsurf / opencode가 사용)
rtrt-mcp --admin --memory ~/.rtrt/memory.sqlite

# Streamable HTTP (MCP 2025-06-18) — axum 라우터
RTRT_MCP_HTTP_TOKEN=$(openssl rand -hex 16) \
  rtrt-mcp --transport http --bind 127.0.0.1:7312 --path /mcp
```

공식 Rust MCP SDK [`rmcp`](https://crates.io/crates/rmcp) 기반. 현재 제공하는 도구:

| 도구 | 래핑 | 비고 |
|------|------|------|
| `compress` | `Compressor::compress` | `level = lite \| full \| ultra` (기본 `full`) |
| `compress_ml` | `MlCompressor::compress` | LLMLingua-style 토큰 중요도 압축, `ratio` ∈ (0.05, 1.0] |
| `proxy` | `rtrt_proxy::{filter_for, errors_only, ultra_compact}` | mode = `command \| errors_only \| ultra_compact` |
| `memory_save` | `MemoryStore::save` | FTS5 + BM25 |
| `memory_recall` | `MemoryStore::recall_bm25[_with_filter]` | qdrant-style 페이로드 필터 옵션 (`source=claude,topic~^auth`) |
| `memory_timeline` | `MemoryStore::recent_paged` + `count_by_project` | 페이지네이션 히스토리; `{items, total}` |
| `memory_profile` | `MemoryStore::projects` + 카운트 | 프로젝트별 row 수 + 최신 타임스탬프 |
| `memory_relations` | `MemoryStore::project_edges` BFS | seed id에서 그래프 탐색, 깊이 제한 |
| `memory_smart_search` | BM25, 임베더 부착 시 하이브리드 | 단일 쿼리 엔트리 포인트 |
| `memory_export` | `MemoryStore::export_jsonl` | JSON Lines export (한 줄 한 행) |
| `memory_consolidate` | `MemoryStore::archive_overflow_no_llm` | 최신 N 유지, 나머지 archive (LLM-free) |
| `memory_sessions` | `MemoryStore::sessions` / `session_records` | 세션 요약 또는 세션 내 행 리스트 |
| `memory_set_block` / `memory_get_block` / `memory_list_blocks` | `MemoryStore::*_block` | Letta-style persona / human / context 블록 |
| `repo_map` | `tree-sitter` 시그니처 추출 | Rust / Python / TypeScript 시그니처 덤프 |
| `templates_list` | `rtrt_templates::list_all` | 빌트인 + 커스텀 |
| `templates_scaffold` | `rtrt_templates::render::{plan,write}` | 스캐폴드 |
| `provider_chat` | `Gateway::chat` | 멀티-프로바이더 라우팅 |
| `agent_call` | 프로바이더 호출 브리지 | 선택한 에이전트 타깃 호출 |
| `agent_route` | `select_route` | 비용과 헤드룸을 고려한 에이전트 라우트 선택 |
| `security_scan` | `rtrt_security::run` | 이름으로 지정한 보안 프로파일로 고정 프로젝트 스캔 |
| `permission_prompt` | 로컬 RTRT permission broker | Claude 권한 결정 요청, 기본값은 deny |

### MCP 자동 캡처

`rtrt-mcp`는 대시보드와 동일한 자동 캡처 파이프라인을 `compress` / `compress_ml` / `proxy` / `provider_chat` 성공마다 실행. 각 호출 후 `redact_secrets` → SHA-256 dedup → `memory.save` → `session_id` 태깅. 세션 id는 프로세스 당 UUID 1개. 환경 변수는 대시보드와 공유:

| Env | 기본 | 효과 |
|-----|------|------|
| `RTRT_AUTO_CAPTURE` | `1` | MCP 자동 캡처 마스터 스위치 |
| `RTRT_AUTO_REDACT` | `1` | 저장 전 `redact_secrets` 실행 |
| `RTRT_AUTO_DEDUP_WINDOW_SEC` | `300` | N초 이내 동일 body 해시 스킵 |
| `RTRT_DEFAULT_PROJECT` | 현재 디렉토리명 | 캡처 row의 프로젝트 버킷 |

로컬 stdio MCP 자동 캡처는 linked worktree를 main Git 저장소 project로 귀속합니다. 공유 HTTP MCP server의 자동 캡처 tool은 명시적 `project`를 받을 수 있으며, 없으면 잘못된 project를 만들거나 오염시키지 않고 캡처를 건너뜁니다.

HTTP 전송 옵션:

- `RTRT_MCP_HTTP_TOKEN` — 프로세스 인자에 노출되지 않도록 환경에서만 읽는 필수 베어러 토큰; 누락/오류 시 401 + `WWW-Authenticate`. 상수-시간 비교.
- `--allowed-origins host1,host2` / `RTRT_MCP_ALLOWED_ORIGINS` — RFC 6454 Origin 허용 목록. 지정하지 않으면 `Origin` 헤더를 포함한 요청을 403으로 거부합니다. 네이티브 클라이언트는 `Origin`을 보내지 않으므로 영향이 없습니다.
- `RTRT_MCP_HTTP_TOKEN`이 없거나 비어 있으면 HTTP 시작에 실패합니다.

HTTP MCP는 비어 있거나 누락된 bearer token을 거부합니다. Process/network
tool은 명시적인 HTTP opt-in 없이는 사용할 수 없고 filesystem tool은 canonical
project에 bound되어 HTTP caller가 redirect할 수 없습니다.

Standalone MCP 등록은 `~/.claude.json`(또는 에이전트의 MCP 설정)에 등록:

```json
{
  "mcpServers": {
    "rtrt": {
      "command": "rtrt-mcp",
      "args": ["--admin", "--memory", "/path/to/memory.sqlite"]
    }
  }
}
```

이는 OpenCode direct-Claude lane과 별개입니다. OpenCode setup은 `~/.claude.json`을 읽거나 쓰거나 요구하지 않고, invocation별 ephemeral settings와 strict RTRT MCP config를 직접 주입합니다.

`rtrt mcp`는 `rtrt-mcp` 바이너리에 `--transport / --bind / --path / --allowed-origins`를 그대로 넘기는 CLI 패스스루입니다. `RTRT_MCP_HTTP_TOKEN`은 환경에서 상속하며 하위 프로세스 인자에는 복사하지 않습니다.

## 대시보드 (`rtrt-dashboard`)

```bash
~/.local/bin/rtrt service install --apply
~/.local/bin/rtrt service open
```

| 경로 | 메서드 | 용도 |
|------|--------|------|
| `/` | `GET` | HTML 인덱스. Project 페이지: Overview, Memory, Compression, Command, Statusline, Settings, Templates, Prompts, Diagnose, Security. Tools 페이지: LLM, Chat, Limits, Environment, Usage, Failover, Connect. |
| `/healthz` | `GET` | 라이브니스(`ok`) |
| `/api/metrics` | `GET` | 게이트웨이 요약 + 최근 메트릭 (SVG 스파크라인 데이터원) |
| `/api/budget` | `GET` | `{ cap_usd, spent_usd, remaining_usd }` |
| `/api/prompts` / `/api/prompts/{name}` / `/api/prompts/{name}/{version}` | `GET` | langfuse-style 버전 프롬프트 |
| `/api/templates` / `/api/templates/{name}` | `GET` | 템플릿 |
| `/api/templates/scaffold` | `POST` | 스캐폴드 |
| `/api/chat` | `POST` | 게이트웨이 chat |
| `/api/compress` | `POST` | 룰 또는 ML 압축 |
| `/api/proxy` | `POST` | rtrt-proxy 필터 |
| `/api/diagnose` | `POST` | aider-style 진단 (errors_only + LLM) |
| `/api/memory/save` | `POST` | 메타데이터 옵션 |
| `/api/memory/recall` | `POST` | BM25 + 페이로드 필터 |
| `/api/memory/blocks` | `GET` / `POST` | Letta 블록 |
| `/api/memory/blocks/{name}` | `GET` | 단일 블록 |
| `/api/repo-map` | `POST` | tree-sitter 시그니처 맵 |
| `/api/setup` | `POST` | 에이전트 MCP 설정 스니펫 (dry-run) |

Dashboard는 `rtrt-dashboard --machine --state-dir <home>/.rtrt/dashboard` machine invocation만 허용하고 private `dashboard.env`에서 256-bit hexadecimal bearer를 읽습니다. Long-lived token은 environment, argv, URL, log에 들어가지 않습니다. 정확한 POST-only bootstrap exchange를 제외한 모든 `/api/*`는 constant-time bearer 검증이 필수입니다. `/healthz`와 bundled SPA asset은 공개 상태지만 token을 노출하지 않습니다. Browser API Origin은 설정된 고정 bind/loopback authority와 일치해야 하므로 Host 기반 DNS rebinding을 차단합니다. Origin 없는 bearer API client는 계속 지원됩니다.

`~/.local/bin/rtrt service install --apply`는 OS CSPRNG의 256-bit machine token을 정확히 `~/.rtrt/dashboard/dashboard.env`에 생성하거나 기존 소유 token을 promote합니다. 서비스는 `rtrt-dashboard --machine --state-dir ~/.rtrt/dashboard`만 실행하며 project cwd/slug, `RTRT_MEMORY_PATH`, token argv를 사용하지 않습니다. Dashboard는 `~/.rtrt/projects`의 검증된 store를 표시합니다. **All projects**는 writable project가 아닌 aggregate selector이므로 project-specific 작업 전에는 구체적 project를 선택해야 합니다. 설치는 cwd-independent/idempotent이며 token을 출력하지 않습니다.

### Failover 범위

Tools의 **Failover** 페이지는 `/api/failover/config`로 `[failover]` 정책을 관리합니다. 프로젝트를 선택하지 않았을 때만 글로벌 정책을 편집합니다. 프로젝트를 선택한 상태에서 상속된 정책은 읽기 전용입니다. **Custom**을 선택하면 프로젝트 오버라이드를 기록하고, **Follow global**을 선택하면 그 오버라이드를 제거해 다시 상속합니다.

Linux/macOS의 `~/.local/bin/rtrt service open`은 private file과 dashboard health를 검증한 뒤 60초 one-time HMAC bootstrap으로 엽니다. 자동 open이 안 되면 `~/.local/bin/rtrt service open --print-bootstrap`으로 short-lived URL만 출력합니다. Windows는 `rtrt service open` 미지원이므로 <http://127.0.0.1:7311/>을 열고 bootstrap prompt에만 token을 입력합니다. Long-lived token은 URL/opener argv/log에 들어가지 않고, SPA는 fragment를 즉시 지우며 bearer를 해당 tab의 `sessionStorage`에만 저장합니다. Unix uninstall과 `install.ps1 -Uninstall`은 owned definition만 제거하고 token/project DB를 보존합니다.

## 자동 캡처 파이프라인

대시보드는 성공한 `/api/chat`, `/api/compress`, `/api/diagnose`, `/api/proxy` 호출마다 메모리 스토어에 자동 저장. [`plugins/claude-code/rtrt/`](../plugins/claude-code/rtrt/)의 Claude Code 플러그인은 훅 12종 발화마다 동일 파이프라인 수행: PreToolUse / PostToolUse / PostToolUseFailure / PreCompact / UserPromptSubmit / PostUserPromptSubmit / Notification / Stop / SubagentStart / SubagentStop / SessionStart / SessionEnd. 대시보드 활동 피드는 `/api/stream` (Server-Sent Events) 구독으로 실시간 알림 수신, SSE 미지원 시 5초 폴링 폴백.

캡처 이벤트는 다음 파이프라인 통과:

```
이벤트 발화
  ├─ 1. SHA-256 dedup       (5분 윈도우, 설정 가능)
  ├─ 2. 프라이버시 필터     (AWS / GitHub / OpenAI / Anthropic / Slack /
  │                          Bearer / 개인 키 / api_key=… 모두 검열)
  ├─ 3. SQLite 저장         (FTS5 + BM25 자동 인덱싱)
  ├─ 4. 세션 id 태깅        (프로세스 당 UUID 1개)
  └─ 5. 옵션 LLM 압축       (백그라운드 태스크, 기본 off)
```

### 설정

| Env | 기본 | 효과 |
|-----|------|------|
| `RTRT_AUTO_CAPTURE` | `1` | 대시보드 자동 캡처 마스터 스위치 |
| `RTRT_AUTO_REDACT` | `1` | 저장 전 `redact_secrets` 실행 |
| `RTRT_AUTO_DEDUP_WINDOW_SEC` | `300` | N초 이내 동일 body 해시 스킵 |
| `RTRT_DEFAULT_PROJECT` | `default` | 대시보드 캡처의 프로젝트 버킷 |
| `RTRT_CONSOLIDATE_INTERVAL_SEC` | `3600` | 시간당 archive sweep 주기 (0 비활성) |
| `RTRT_CONSOLIDATE_KEEP` | `1000` | sweep 후 프로젝트별 유지 row 수 |
| `RTRT_AUTO_COMPRESS_LLM` | `0` | 옵트인 LLM 압축 데몬; `1`로 활성화 |
| `RTRT_AUTO_COMPRESS_MODEL` | `claude-haiku-4-5` | 게이트웨이가 사용할 모델 id |
| `RTRT_AUTO_COMPRESS_INTERVAL_SEC` | `1800` | sweep 주기 (초) |
| `RTRT_AUTO_COMPRESS_AGE_SEC` | `3600` | 이보다 오래된 row만 압축 대상 |
| `RTRT_AUTO_COMPRESS_MIN_CHARS` | `512` | 이보다 짧은 row 스킵 |
| `RTRT_AUTO_COMPRESS_BATCH` | `20` | 프로젝트당 sweep당 최대 압축 수 |
| `RTRT_AUTO_COMPRESS_MAX_TOKENS` | `512` | compress 호출당 최대 출력 토큰 |

LLM 압축 데몬이 다시 쓴 row는 `metadata.compressed_at`, `compressed_model`, `compressed_from_chars`, `compressed_to_chars`로 태깅. LLM 출력이 비었거나 원본보다 짧지 않으면 본문은 그대로 두고 `compressed_skip=no-shrink`만 기록 — 데몬이 재시도하지 않음. 임베딩은 의도적으로 재생성하지 않음. `set_body`가 BM25 인덱스를 동기화하므로 recall은 그대로 작동.

**로컬 모델 선택.** 기본 `claude-haiku-4-5`는 클라우드 키 대상. Ollama / OpenAI 호환 엔드포인트로 완전 로컬 구성 시 `RTRT_AUTO_COMPRESS_MODEL=gemma3:4b` 권장 — 비교 테스트에서 최고 로컬 압축기(전 길이 견고, 작은 GPU에 적재). 모델 비교 표는 [`docs/PERF.ko.md`](PERF.ko.md#llm-자동-압축--로컬-모델-비교--2026-05-26) 참고. `granite4.1:8b`(초장문 실패) / `llama3.1:8b`(사실 조작)는 피할 것.

## ONNX token-importance 백엔드 (옵트인)

`--features onnx`로 빌드 시 휴리스틱 `MlCompressor`가 진짜 LLMLingua-2 스타일 스코어러로 교체됨:

```bash
cargo build --release -p rtrt-cli --features onnx
rtrt compress --ml --ratio 0.5 \
    --onnx-model     ~/.rtrt/models/llmlingua2.onnx \
    --onnx-tokenizer ~/.rtrt/models/tokenizer.json \
    < verbose.md
```

두 파일은 RTRT에 동봉 안 됨 — 사용자가 직접 제공. 모델 계약은 `crates/rtrt-compress/src/ml_onnx.rs`에 문서화 (입력 `input_ids` + `attention_mask` shape `[1, seq_len]`, 출력 `[1, seq_len, 2]` per-token keep-probability 또는 `[1, seq_len]` saliency). `ort`는 `load-dynamic` 모드 — ONNX Runtime 공유 라이브러리는 시작 시 해석. 시스템 전역 설치 (`libonnxruntime.so` / `onnxruntime.dll`) 또는 `ORT_DYLIB_PATH` 설정.

## BERTScore 품질 측정 (옵트인)

`rtrt-eval`은 `bertscore` 피처 뒤에 BERTScore 평가기 동봉. BERT 계열 ONNX 인코더 + 매칭되는 `tokenizer.json` 전달하면 fixture 샘플마다 `Compressor::compress` 출력에 대해 점수 산출:

```bash
cargo run --release -p rtrt-eval --features bertscore -- bertscore \
    --model     ~/.rtrt/models/bert-mini.onnx \
    --tokenizer ~/.rtrt/models/tokenizer.json \
    --level full
```

출력은 샘플당 한 줄 (precision / recall / F1) + mean. 인코더는 `[1, seq_len, hidden]` 출력. 점수는 subword 임베딩 greedy 코사인 정렬 (special 토큰 스킵). 실 라벨링 코퍼스는 `--fixture path/to/dataset.json` (내장 smoke fixture와 동일 스키마)로 드랍 — `docs/PERF.ko.md` 장기 정확도 목표가 기준 삼는 신뢰 가능 수치 게시.

## 설정 파일

설정은 2단 구조입니다:

1. **글로벌** — `~/.rtrt/config.toml`(경로 재정의: `RTRT_CONFIG`). `rtrt config init`으로 생성, `rtrt config path`로 경로 확인. 베이스 커널(훅 · MCP 배선 · 스테이터스라인 커맨드 바인딩)이 여기 있으며 `rtrt setup`이 관리합니다.
2. **프로젝트별** — `<repo>/.rtrt/config.toml`. 오버라이드 전용: 출력 레벨(`off` / `lite` / `full` / `ultra`), 압축, 프로젝트별 에이전트 + 프로바이더 활성화, 스테이터스라인, Failover. 비어 있는 필드는 글로벌 값을 상속; 유효 설정 = 글로벌 ⊕ 프로젝트. 모든 오버라이드가 "글로벌 따름"으로 돌아가면 파일을 삭제해 저장소를 깨끗하게 유지합니다. 대시보드의 **글로벌 따름 / 커스텀** 스코프 토글이 이 층을 편집합니다.

주요 글로벌 섹션 예:

```toml
# 라우팅 타깃별 옵션 일일 상한 — `rtrt usage` 헤드룸과 라우터의
# 헤드룸 가중 선택에 사용. 항목이 없는 타깃은 상한 없음(지어내지 않음).
[limits.openai]
daily_tokens = 1_000_000
daily_requests = 2_000

[limits.ollama]
daily_tokens = 250_000
```

전체 스키마는 `crates/rtrt-core/src/config.rs` 참고(`[compression]`, `[memory]`, `[dashboard]`, `[providers]`, `[agents]`, `[capture]`, `[auto_compress]`, `[embeddings]`, `[security]`, `[limits]`, `[[projects]]`).
