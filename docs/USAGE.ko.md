# 사용법

[English](USAGE.md) | **한국어**

이 문서는 v0.1.0 기준 `rtrt` CLI, `rtrt-mcp` 서버, `rtrt-dashboard` 웹 UI 사용법입니다.

## CLI

```text
rtrt --help
```

### `rtrt compress`

표준 입력을 읽어 압축 결과를 표준 출력에 씁니다.

```bash
echo "Sure, I'd be happy to help. The bug is really in the parser." \
  | rtrt compress -l ultra
```

플래그:

- `-l, --level <lite|full|ultra>` — 압축 강도. 기본값 `full`.

규칙:

- `lite` — 필러(`just`, `really`, `basically` …) 제거 + 다중 공백 압축.
- `full` — `lite` + 인사말(`sure`, `certainly`, `happy to` …) 제거.
- `ultra` — `full` + 관사(`a`, `an`, `the`) 제거.

코드 블록(` ``` `, ` ` `), URL, `"인용 문자열"`은 규칙 적용 전에 보호되어 원문 그대로 복원됩니다.

### `rtrt proxy`

명령 이름을 알려주면 그 명령의 표준 출력을 필터링합니다.

```bash
git status | rtrt proxy "git status"
cargo build 2>&1 | rtrt proxy "cargo build"
```

빌트인 필터는 `git status`, `git log`, `cargo build`, `cargo test`입니다. 매칭되지 않는 명령은 원문이 그대로 통과합니다.

### `rtrt templates`

사용 가능한 템플릿을 나열합니다(빌트인 + 커스텀).

커스텀 템플릿은 `~/.rtrt/templates/<name>/manifest.toml`에 두면 `[Custom]`으로 표시됩니다.

### `rtrt new`

템플릿으로 프로젝트를 만듭니다.

```bash
rtrt new rust-cli ./hello \
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

## MCP 서버 (`rtrt-mcp`)

```bash
# stdio (기본; Claude Code / Codex / Cursor / Windsurf가 사용)
rtrt-mcp --memory ~/.rtrt/memory.sqlite
```

공식 Rust MCP SDK [`rmcp`](https://crates.io/crates/rmcp) 기반. v0.2에서 제공하는 도구:

| 도구 | 래핑 | 비고 |
|------|------|------|
| `compress` | `Compressor::compress` | `level = lite \| full \| ultra` (기본 `full`) |
| `memory_save` | `MemoryStore::save` | FTS5 + BM25 인덱스에 삽입 |
| `memory_recall` | `MemoryStore::recall_bm25` | 프로젝트 스코프, BM25 랭킹 |
| `templates_list` | `rtrt_templates::list_all` | 빌트인 + 커스텀 템플릿 |
| `templates_scaffold` | `rtrt_templates::render::{plan,write}` | 템플릿 스캐폴드 |

`~/.claude.json` (또는 에이전트의 MCP 설정)에 등록:

```json
{
  "mcpServers": {
    "rtrt": {
      "command": "rtrt-mcp",
      "args": ["--memory", "/path/to/memory.sqlite"]
    }
  }
}
```

HTTP/SSE 전송, `provider_chat`, LLM 기반 `memory_extract` / `memory_compress` 도구는 v0.3 예정.

## 대시보드 (`rtrt-dashboard`)

```text
RTRT_DASHBOARD_BIND=127.0.0.1:3111 rtrt-dashboard
```

| 경로 | 메서드 | 용도 |
|------|--------|------|
| `/` | `GET` | HTML 인덱스 — 토큰 절감 통계 + 템플릿 갤러리 |
| `/healthz` | `GET` | 라이브니스(`ok`) |
| `/api/stats` | `GET` | JSON: 입출력 절감 토큰, 활성 프로바이더 |
| `/api/templates` | `GET` | JSON: 템플릿 목록(빌트인 + 커스텀) |
| `/api/templates/{name}` | `GET` | JSON: 템플릿 매니페스트 |
| `/api/templates/scaffold` | `POST` | 스캐폴드 실행 — `{ template, target, variables, overwrite }` |

기본 바인딩은 `127.0.0.1`. `RTRT_DASHBOARD_BIND`로 변경 가능합니다.

## 설정 파일

예정(`~/.rtrt/config.toml`). 스키마는 `crates/rtrt-core/src/config.rs`에 있으며 현재는 `Config::default()`만 동작합니다.
