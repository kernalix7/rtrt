# 변경 이력

[English](../CHANGELOG.md) | **한국어**

이 문서는 프로젝트의 주요 변경 사항을 모두 기록합니다.

형식은 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) 1.1.0을 따르고, 버전 규칙은 [Semantic Versioning](https://semver.org/spec/v2.0.0.html)을 지향합니다.

## [Unreleased]

### Highlights — 방향성 정리

**RTRT는 Unix 도구 모음 방향으로 정식 commit. 최상위 `DESIGN.md`가 10개 원칙 문서화, `docs/PERF.md`가 SLO 표 + 첫 측정값 게시. 자동 캡처는 옵션이 아닌 기본 동작 — 모든 dashboard `/api/*` 호출과 모든 Claude Code 훅 발화가 SHA-256 dedup + privacy 필터 + 세션 태깅 파이프라인을 거쳐 SQLite에 도달. 시간당 콘솔리데이션 데몬이 프로젝트별 row 캡 유지. 새 메모리 MCP 도구 6개 (timeline / profile / relations / smart_search / export / consolidate) + SSE 라이브 스트림 + 토큰 집계 엔드포인트로 표면 보강.**

- 신규 `DESIGN.md` + `docs/DESIGN.ko.md`: 10개 원칙 — 프레임워크 아닌 도구, 안정된 substrate, 3 기둥만, 자동 캡처 기본, 옵션 크레이트로 확장, 측정값 성능, 로컬 우선, 발행 인터페이스 영원, 작게 천천히 깊게.
- 신규 `docs/PERF.md` + `docs/PERF.ko.md`: SLO 표 + `recall_bench` 첫 측정값. 10% 이상 회귀는 릴리스 차단.
- `rtrt-memory` v4 스키마: `session_id` + `body_sha` 컬럼 + 인덱스. `body_sha()` / `body_seen_at()` / `tag_row()` / `archive_overflow_no_llm()` 헬퍼.
- `rtrt-dashboard` 자동 캡처 파이프라인: `/api/{chat,compress,diagnose,proxy}` 성공마다 `redact_secrets` → SHA-256 dedup (기본 5분) → 저장 → 세션 태깅. 환경 변수: `RTRT_AUTO_CAPTURE` / `RTRT_AUTO_REDACT` / `RTRT_AUTO_DEDUP_WINDOW_SEC` / `RTRT_DEFAULT_PROJECT`. 저장마다 `/api/stream` (SSE) JSON 이벤트 브로드캐스트.
- 시간당 콘솔리데이션 데몬 — `archive_overflow_no_llm`로 프로젝트별 `RTRT_CONSOLIDATE_KEEP` (기본 1000) 최신 row 유지. 주기 `RTRT_CONSOLIDATE_INTERVAL_SEC` (기본 3600, 0 비활성).
- `GET /api/memory/projects` + `GET /api/memory/timeline?project=X&limit=N&offset=M` — 대시보드 프로젝트 픽커 + 페이지네이션 히스토리.
- `GET /api/tokens/summary` — 게이트웨이 요청 이력 시간/일 단위 집계.
- `GET /api/stream` SSE + 256-슬롯 tokio broadcast — 캡처마다 `{type, id, kind, project, session}` 이벤트.
- 새 MCP 메모리 도구 6개: `memory_timeline` / `memory_profile` / `memory_relations` / `memory_smart_search` / `memory_export` / `memory_consolidate`. MCP 서버 총 17개 도구.
- `plugins/claude-code/rtrt/` — Claude Code 플러그인 스캐폴드, 훅 스크립트 6개. `rtrt` CLI 우선, `POST /api/memory/save` 폴백. Best-effort: 캡처 실패해도 에이전트 안 멈춤.
- `crates/rtrt-memory/benches/recall_bench.rs` — criterion 벤치 (1K / 10K / 100K). 첫 측정값 `docs/PERF.md`.
- 워크스페이스 의존성: `sha2` (dedup), `uuid` (세션), `tokio-stream` (SSE).

### Highlights — 같은 브랜치 이전 묶음

- `rtrt-mcp`: rmcp Streamable HTTP 전송 + axum 라우터. 새 도구 `compress_ml`, `proxy`, `memory_set_block/get_block/list_blocks`. `memory_recall`에 qdrant DSL 필터 파라미터. `--http-token` 상수-시간 베어러 가드, `--allowed-origins` RFC 6454.
- `rtrt-memory`: v3 마이그레이션 `metadata` 컬럼, `PayloadFilter` DSL (`source=claude,topic~^auth`), `recall_bm25_with_filter`, `save_with_metadata`, `export_jsonl` / `import_jsonl`.
- `rtrt-providers`: `Gateway::with_cache(cap)` Helicone 응답 캐시 — 키 `(model, messages, max_tokens, temperature)`, 히트는 재시도/메트릭/예산 우회.
- `rtrt-compress`: `MlCompressor` + `TokenImportance` 트레이트 + 휴리스틱 백엔드 (ONNX 백엔드 deferred), `compress_to(Plain/Markdown/Xml/Json)`, tree-sitter Python + TypeScript 그래머.
- `rtrt-templates`: `agent-role` 빌트인 (crewAI role/goal/backstory 트리아드).
- `rtrt-dashboard`: 10탭 (Metrics / Budget / Prompts / Memory / Templates / Compression / Proxy / Diagnose / RepoMap / Setup), SVG 스파크라인, parent_id 그룹 트레이스, 다크/라이트 토글, 캐시 KPI. 신규 라우트 `/api/{prompts*, budget, memory/{recall,save,blocks,blocks/{name}}, compress, proxy, diagnose, repo-map, setup}`. `RTRT_DASHBOARD_TOKEN` 베어러 미들웨어.
- `rtrt-cli`: `rtrt diagnose`, `rtrt mcp`, `rtrt benchmark`, `rtrt memory export/import`, `rtrt memory blocks {set,get,list}`. 기존 확장: `compress {--ml --ratio --format}`, `memory recall --filter`, `memory save --meta key=val`, `signatures --lang python|typescript`, `repo-map` 다중 언어 자동 감지.

자세한 항목은 [영문 CHANGELOG](../CHANGELOG.md#unreleased) 참고.

---

**첫 번째 스윕 (트레이서빌리티용 유지) — INSPIRATION 백로그 HIGH 12개:**

- `rtrt-providers`: `Gateway` + `Budget` + `RequestMetric { id, parent_id, cost_usd, … }`; `Context7Client`.
- `rtrt-memory`: `MemoryScope`, `add_edge` + `recall_via_graph`, `with_embedder` 자동 임베드, `archive_overflow`, `hnsw` 피처.
- `rtrt-compress`: `Extreme` 레벨, 헤지/담화/메타 표현 규칙, `redact_secrets`, `LlmCompressor`, Rust 시그니처 추출기.
- `rtrt-templates`: handlebars 렌더링, `PromptRegistry`.
- `rtrt-cli`: `compress --llm` / `memory {extract,compress}` / `prompt {save,get,list,versions}` / `signatures` / `repo-map` / `discover` / `docs` / `setup --agent`.
- `rtrt-mcp`: 6번째 도구 `provider_chat`.

<!--
릴리스 절단 시 이 스탠자를 복사해 새 버전 섹션을 만드세요.
`### Highlights`를 항상 섹션 최상단에 두는 것이 중요합니다 — 릴리스 페이지에서
사용자가 가장 먼저 보는 영역이며, 릴리스 워크플로우가 이 섹션을 그대로 추출합니다.

### Highlights

**한 줄 헤드라인.** 필요하면 1-2 문장 보충.

- 가장 중요한 사용자 가시 변경
- 두 번째로 중요한 변경
- (총 3~6 불릿, 산문 블록 금지)

### Added
### Changed
### Fixed
-->

## [0.1.0] - 2026-05-20

### Highlights

**초기 워크스페이스 스캐폴드. 출력 압축 · 명령 출력 필터링 · SQLite-FTS5 BM25 회수 · 프로젝트 템플릿 스캐폴딩은 모두 동작합니다. MCP 전송 계층, 프로바이더 채팅, 설치 스크립트는 명시적 스텁입니다.**

- edition 2024 기반 Cargo 워크스페이스, 크레이트 9개(`rtrt-core`, `rtrt-compress`, `rtrt-proxy`, `rtrt-memory`, `rtrt-providers`, `rtrt-templates`, `rtrt-mcp`, `rtrt-dashboard`, `rtrt-cli`).
- `rtrt-compress`는 `lite`/`full`/`ultra` 3단계 케이브맨 스타일 재작성기를 제공합니다. 코드 블록, 인라인 코드, URL, 인용 문자열은 규칙 단계 전에 보호되었다가 복원됩니다.
- `rtrt-proxy`는 `git status`, `git log`, `cargo build`, `cargo test`용 필터를 제공합니다. CLI에서는 `rtrt proxy "<cmd>"`로 stdin → 필터링된 stdout 처리.
- `rtrt-memory`는 SQLite + FTS5 스키마(`memories / memories_fts / embeddings / edges`)와 `recall_bm25` API를 제공합니다.
- `rtrt-templates`는 빌트인 6종과 `~/.rtrt/templates/<name>/manifest.toml`에서 로드하는 커스텀 템플릿을 제공합니다. E2E 검증: `rtrt new rust-cli`로 생성한 프로젝트의 `cargo check`가 통과합니다.
- `rtrt-dashboard`는 `/`, `/healthz`, `/api/stats`, `/api/templates`, `/api/templates/{name}`, `/api/templates/scaffold`를 노출하는 axum 서버입니다.

### Added

- 워크스페이스 스캐폴드, MIT LICENSE, GitHub 표준화(이슈/PR 템플릿, FUNDING.yml, CI 워크플로우), 다국어 `docs/` 트리(INSTALL/USAGE/FEATURES/ARCHITECTURE/COMPARISON 영문 + 한국어 미러).
- `Compressor::compress` 규칙 보호 파이프라인.
- `rtrt_proxy::filter_for` 디스패치, `git_status` / `git_log` / `cargo_noise` 필터, `collapse_blanks` 헬퍼.
- `MemoryStore::open`, `MemoryStore::open_in_memory`, `MemoryStore::save`, `MemoryStore::recall_bm25`.
- `Provider` 트레이트 + Anthropic / OpenAI / OpenAI 호환 어댑터 스텁.
- `rtrt-templates`의 `Template`, `TemplateFile`, `TemplateVariable`, `RenderPlan`, 빌트인 정의, 매니페스트 로더, `{{var}}` 치환, 선택적 포스트-인스톨 훅.
- `rtrt` CLI 서브커맨드: `compress`, `proxy`, `templates`, `new`, `info`.
- 템플릿 갤러리 + 스캐폴드 엔드포인트를 갖춘 axum 대시보드.

### Notes

- MCP stdio 전송 계층은 미구현. `rtrt-mcp`는 예정 도구 목록을 로깅하고 종료.
- 프로바이더 `chat`은 `Error::Provider("... not implemented yet")` 반환. 모델 목록과 어댑터 형태만 연결됨.
- `rtrt-memory`는 아직 임베딩 없음. `embeddings`/`edges` 테이블은 예약.
- `install.sh` / `install.ps1`은 README에 명시되어 있으나 트리에 아직 없음.
