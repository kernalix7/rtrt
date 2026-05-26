# 사용법

[English](USAGE.md) | **한국어**

이 문서는 `rtrt` CLI, `rtrt-mcp` 서버, `rtrt-dashboard` 웹 UI 최신 사용법입니다.

## 빠른 차림표

- 룰 기반 압축: `rtrt compress -l ultra` (LLM 불필요)
- ML 압축: `rtrt compress --ml --ratio 0.4`
- LLM 압축: `rtrt compress --llm --provider openai-compat --model llama3.2`
- 메모리 리콜: `rtrt memory recall --project rtrt --query auth --filter "source=claude"`
- MCP HTTP: `rtrt mcp --transport http --http-token "$TOKEN"`
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

### MCP 자동 캡처

`rtrt-mcp`는 대시보드와 동일한 자동 캡처 파이프라인을 `compress` / `compress_ml` / `proxy` / `provider_chat` 성공마다 실행. 각 호출 후 `redact_secrets` → SHA-256 dedup → `memory.save` → `session_id` 태깅. 세션 id는 프로세스 당 UUID 1개. 환경 변수는 대시보드와 공유:

| Env | 기본 | 효과 |
|-----|------|------|
| `RTRT_AUTO_CAPTURE` | `1` | MCP 자동 캡처 마스터 스위치 |
| `RTRT_AUTO_REDACT` | `1` | 저장 전 `redact_secrets` 실행 |
| `RTRT_AUTO_DEDUP_WINDOW_SEC` | `300` | N초 이내 동일 body 해시 스킵 |
| `RTRT_DEFAULT_PROJECT` | 현재 디렉토리명 | 캡처 row의 프로젝트 버킷 |

HTTP 전송 옵션:

- `--http-token <T>` / `RTRT_MCP_HTTP_TOKEN` — 필수 베어러 토큰; 누락/오류 시 401 + `WWW-Authenticate`. 상수-시간 비교.
- `--allowed-origins host1,host2` / `RTRT_MCP_ALLOWED_ORIGINS` — `StreamableHttpServerConfig.allowed_origins`에 매핑 (RFC 6454).
- 비-루프백 바인드 + 토큰 미설정 시 시작 시 경고.

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

`rtrt mcp`는 `rtrt-mcp` 바이너리에 `--transport / --bind / --path / --http-token / --allowed-origins`를 그대로 넘기는 CLI 패스스루입니다.

## 대시보드 (`rtrt-dashboard`)

```text
RTRT_DASHBOARD_BIND=127.0.0.1:7311 \
  RTRT_DASHBOARD_TOKEN=$(openssl rand -hex 16) \
  rtrt-dashboard
```

| 경로 | 메서드 | 용도 |
|------|--------|------|
| `/` | `GET` | HTML 인덱스 — Metrics / Budget / Prompts / Memory / Templates / Compression / Proxy / Diagnose / RepoMap / Setup 탭 |
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

`RTRT_DASHBOARD_TOKEN` 환경변수 설정 시 `/api/*`는 베어러 토큰 미들웨어로 보호; `/`와 `/healthz`는 부트스트랩용으로 항상 통과. 비-루프백 바인드 + 토큰 미설정 시 경고.

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

예정(`~/.rtrt/config.toml`). 스키마는 `crates/rtrt-core/src/config.rs`에 있으며 현재는 `Config::default()`만 동작합니다.
