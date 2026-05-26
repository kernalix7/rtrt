# 변경 이력

[English](../CHANGELOG.md) | **한국어**

이 문서는 프로젝트의 주요 변경 사항을 모두 기록합니다.

형식은 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) 1.1.0을 따르고, 버전 규칙은 [Semantic Versioning](https://semver.org/spec/v2.0.0.html)을 지향합니다.

## [Unreleased]

### Highlights — 로컬 LLM 압축 모델 비교

- `docs/PERF.md` + `docs/PERF.ko.md`에 LLM 자동 압축 경로의 로컬 Ollama 모델 길이별 비교 게시 (티어당 현실 캡처 20개 × 6티어, XS ~16자 ~ XXL ~6000자). 결론: 압축률은 모델보다 입력 길이가 좌우 — 짧은 행은 거의 안 줄어 `RTRT_AUTO_COMPRESS_MIN_CHARS=512`가 올바르게 스킵, dense 중간 길이 ~25-30%, 긴 장황한 캡처 40%+.
- 로컬 권장 기본 `gemma3:4b`: 전 길이 견고(XXL 42%), 4.3GB로 GPU 100% 적재, 짧은 행 안전 스킵. `granite4.1:8b`는 초장문 부적합(6000자 전부 미압축), `llama3.1:8b`는 사실 조작, `qwen3.5:9b`(thinking)는 verbatim 반환.
- `docs/USAGE.md` + `docs/USAGE.ko.md`에 `RTRT_AUTO_COMPRESS_MODEL=gemma3:4b` 로컬 오버라이드 명시; 코드 기본값은 클라우드 키 사용자 위해 `claude-haiku-4-5` 유지.

### Highlights — MCP Prompts/Resources + ONNX 백엔드 + BERTScore

**남은 로드맵 3개 항목 한 묶음에 착륙. MCP 서버는 핸들러 트라이어드 (tools / prompts / resources) 완전 노출; 휴리스틱 `MlCompressor`는 옵션 실 ONNX-runtime 백엔드 (LLMLingua-2 계약 일치)로 졸업; `rtrt-eval`은 동일 인코더 로딩 머신을 공유하는 BERTScore 평가기 추가. 신규 코드 전부 피처 게이트, 모델 파일은 무동봉.**

- `rtrt-mcp`가 `enable_prompts()` + `enable_resources()` 선언, 4개 핸들러 구현. `prompts/list`는 로컬 `PromptRegistry`의 모든 이름 (기본 `~/.rtrt/prompts/`, `RTRT_PROMPTS_DIR`로 오버라이드); `prompts/get`은 handlebars 인자 치환과 함께 최신 버전 반환. `resources/list`는 프로젝트당 `memory://<project>/timeline` 1개 + Letta 블록당 `memory://<project>/block/<name>` 1개씩 노출; `resources/read`는 JSONL 타임라인 row 또는 블록 본문 반환. 에러는 `McpError::invalid_params` / `internal_error`로 매핑, 서버는 절대 크래시 안 함.
- `rtrt-templates::render::render_str` 신설로 handlebars 렌더러 공개 — MCP와 다른 컨슈머가 스캐폴더와 동일 `{{var}}` 엔진 공유.
- `rtrt-compress::OnnxImportance` — 옵트인 `onnx` 피처가 `ort = 2.0.0-rc.12` (`load-dynamic`), HuggingFace `tokenizers`, `ndarray` 픽업. `MlCompressor::onnx(model, tokenizer)`가 세션 구성, 사용자 제공 모델을 `input_ids` + `attention_mask`로 실행, per-subword keep-probability를 tokenizer의 offsets 통해 whitespace 토큰에 매핑. 기본 빌드는 `ort` 링크 안 함 — 룰 엔진만 쓰는 사용자의 워크스페이스 사이즈 동일.
- 신규 CLI 배선: `rtrt compress --ml --onnx-model <path> --onnx-tokenizer <path>` (`rtrt-cli --features onnx`로 게이트, `rtrt-compress/onnx`에 포워드). env 변수 (`RTRT_ONNX_MODEL` / `RTRT_ONNX_TOKENIZER`) 둘 다 수용.
- `rtrt-eval::bertscore` — 옵트인 `bertscore` 피처. `BertScoreScorer::new(encoder.onnx, tokenizer.json)`이 L2-normalised per-subword 임베더 구성; `score(reference, hypothesis)`가 greedy-aligned `(P, R, F1)` 반환; `evaluate_fixture(fixture, level)`이 compressor 실행 후 per-sample + mean 점수 보고. CLI: `rtrt-eval bertscore --model ... --tokenizer ... [--level full]`.
- `docs/USAGE.md` + `docs/USAGE.ko.md`에 ONNX 모델 계약, BERTScore 워크플로, 두 표면의 env 변수 / 피처 플래그 문서화. README 로드맵 (EN + KO) 3 항목 done으로 전환, deferred 멀티 에이전트 라인 별도 불릿 유지.

### Highlights — rtrt-eval 옵션 하니스

**10번째 워크스페이스 크레이트 `rtrt-eval` 도입. 두 표면 (recall 정확도 + 압축 ratio) JSON fixture를 단일 숫자로 환원 → 대시보드 게시 가능. 내장 smoke fixture는 의도적으로 작음 — 동일 스키마 외부 fixture 받음, LongMemEval-S / 인하우스 코퍼스 드랍 시 코드 변경 없이 적용. Smoke 코퍼스에서 R@5 = 0.857 + MRR = 0.857, 내장 floor 테스트로 강제.**

- 신규 크레이트 `crates/rtrt-eval/`: 라이브러리 + `rtrt-eval` 바이너리. 서브커맨드 `recall` / `compress`, JSON 또는 사람 표 출력, `--fixture <path>`로 내장 smoke 셋 덮어쓰기.
- 라이브러리 API: `RecallFixture`, `CompressFixture`, `evaluate_recall(&fixture, k) -> RecallReport`, `evaluate_compression(&fixture, level) -> CompressReport`. 내장 fixture는 `RECALL_SMOKE` / `COMPRESS_SMOKE` const로 노출.
- Smoke fixture: `crates/rtrt-eval/fixtures/recall_smoke.json` (12 docs, 7 라벨 query) + `compress_smoke.json` (3 prose 샘플). BM25가 R@5 ≥ 0.80 floor 클리어하도록 손-튜닝; floor 미달 시 `recall_at_5_on_smoke_fixture_clears_floor` 테스트가 머지 차단.
- `docs/PERF.md` + `docs/PERF.ko.md`에 smoke fixture 첫 측정값 게시. 명시적으로 smoke (경쟁 벤치 아님) — 실수치는 실제 라벨링 코퍼스 필요.
- README 로드맵 (EN + KO): rtrt-eval + smoke 스크립트는 done으로 전환; BERTScore 수치 / ONNX 백엔드 / 정식 태그는 open 유지.

### Highlights — LLM 자동 압축 + 라이브 키 스모크 게이트

- `rtrt-dashboard`에 옵트인 LLM 압축 데몬. `RTRT_AUTO_COMPRESS_LLM=1` 설정 시 백그라운드 tokio 태스크가 `RTRT_AUTO_COMPRESS_AGE_SEC`보다 오래되고 `RTRT_AUTO_COMPRESS_MIN_CHARS`보다 긴 body row를 스윕, 게이트웨이 모델 (`RTRT_AUTO_COMPRESS_MODEL`, 기본 `claude-haiku-4-5`)에 의미 보존 압축 요청 후 본문 덮어쓰기. 재작성된 row는 `metadata.compressed_at` / `compressed_model` / `compressed_from_chars` / `compressed_to_chars`로 태깅 — 다음 스윕은 스킵. 모델 출력이 비었거나 원본보다 짧지 않으면 `compressed_skip=no-shrink`만 기록하고 본문은 유지.
- `MemoryStore::set_body` (외부 콘텐츠 FTS5의 `'delete' + insert` 패턴으로 인덱스 동기화) + `MemoryStore::compress_candidates` (age / min-chars / not-yet-compressed 필터) — 데몬의 토대. `auto_compress_primitives` 회귀 테스트로 커버.
- `scripts/smoke.sh` — 라이브 키 스모크 하니스. `rtrt --version` / `compress` / `proxy` / `templates` / `new` / `repo-map`은 무조건 실행; Anthropic / OpenAI / OpenAI-compat 채팅은 환경 변수 있을 때만 (없으면 SKIP); 루프백 포트에 `rtrt-dashboard` + `rtrt-mcp` 띄워서 `/healthz` / `/api/templates` / `/api/stats` + MCP HTTP 응답성 + 베어러 가드 401 검증. 실제 실행된 검사가 실패한 경우에만 non-zero. `0.1.0` 정식 태그 승격 전 게이트.
- `docs/USAGE.md` + `docs/USAGE.ko.md`에 `RTRT_AUTO_COMPRESS_*` 환경 변수 7개와 데몬이 기록하는 메타데이터 필드 문서화.

### Highlights — 대시보드 / 문서 / 회귀 커버리지

- 대시보드 활동 피드가 `EventSource`로 `/api/stream` 구독. SSE 미지원 환경에서만 5초 폴링 폴백. 캡처가 새로고침 없이 실시간 표시.
- `docs/USAGE.md` + `docs/USAGE.ko.md`에 MCP 18개 도구 전부 문서화 (`memory_timeline` / `memory_profile` / `memory_relations` / `memory_smart_search` / `memory_export` / `memory_consolidate` / `memory_sessions` / `repo_map` 표 추가) + MCP가 인식하는 `RTRT_AUTO_*` 환경 변수 4개. 한국어 USAGE는 영문에만 있던 대시보드 자동 캡처 파이프라인 섹션도 보강.
- `rtrt-memory` 회귀 테스트 `auto_capture_pipeline_primitives` — 대시보드 / MCP가 공통 사용하는 빌딩 블록 검증: 결정론적 `body_sha`, `body_seen_at` dedup 윈도우 (프로젝트별 스코핑), `tag_row` 세션 + sha 기록, `sessions` / `session_records` 그룹화, `archive_overflow_no_llm` 최신 N 유지.

### Highlights — 방향성 정리 후속

**스키마 v5가 타임라인 페이저용 커버링 인덱스 추가 (`recent_paged` 100K rows p50 71ms → ~32µs, 2200× 가속). Claude Code 플러그인 훅 6 → 12개. MCP에 7번째 메모리 도구 (`memory_sessions`) 추가 — v4 `session_id` 컬럼 노출. MCP `compress` / `compress_ml` / `proxy` / `provider_chat` 핸들러 4개가 대시보드와 동일한 자동 캡처 파이프라인 통과. PR 시점 perf 게이트 (`.github/workflows/perf.yml` + `scripts/perf-gate.sh`)가 베이스라인 대비 10% 이상 회귀 거부. 한국어 README가 Unix-toolkit 포지셔닝으로 재정렬.**

- `rtrt-memory` 스키마 v5: `idx_memories_timeline` 커버링 인덱스 `(project, created_at DESC, id DESC)`. 신규 `sessions()` + `session_records()` 헬퍼 — `session_id` 기준 그룹화로 리플레이/익스포트 가능. `recent_paged` p50 모든 크기에서 sub-50 µs.
- `rtrt-mcp`에 `memory_sessions` (프로젝트별 세션 요약 또는 세션 행 리스트) 추가, 총 18개 도구. `RtrtState`에 `auto_capture()` 헬퍼 — 대시보드와 동일 파이프라인 (`redact_secrets` → SHA-256 dedup → save → 세션 태그). `compress` / `compress_ml` / `proxy` / `provider_chat` 성공 시마다 실행. 환경 변수: `RTRT_AUTO_CAPTURE` / `RTRT_AUTO_REDACT` / `RTRT_AUTO_DEDUP_WINDOW_SEC` / `RTRT_DEFAULT_PROJECT` (대시보드와 동일).
- Claude Code 플러그인 (`plugins/claude-code/rtrt/`) 훅 12개: PreToolUse / PostToolUse / PostToolUseFailure / PreCompact / UserPromptSubmit / PostUserPromptSubmit / Notification / Stop / SubagentStart / SubagentStop / SessionStart / SessionEnd.
- `.github/workflows/perf.yml` — PR base ref 기준 `--save-baseline` / `--baseline`로 `rtrt-memory` 벤치, `scripts/perf-gate.sh`가 criterion `estimates.json` 파싱 후 10% 이상 p50 회귀 시 exit 1. `docs/PERF.ko.md` 정책 자동화.
- `docs/PERF.md` + `docs/PERF.ko.md`에 v5 인덱스 적용 후 측정값 갱신.
- `docs/README.ko.md` 재작성 — Unix-toolkit 포지셔닝, 3 기둥 블록, DESIGN/PERF 링크, 18 MCP 도구 반영.

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
