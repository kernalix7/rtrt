<div align="center">

# RTRT

### AI 에이전트용 유닉스 툴킷 — 토큰은 줄이고, 기억은 남기고.

<p>세 기둥, 그 이상도 이하도 아님:<br>
<strong>토큰 절감</strong> (룰 + ML 압축 + 시그니처 + 시크릿 검열),<br>
<strong>영구 프로젝트 메모리</strong> (자동 캡처 · SQLite · BM25 + 벡터 + 그래프),<br>
<strong>멀티 프로바이더 라우팅</strong> (로컬 우선; Anthropic / OpenAI / Ollama / vLLM / LM Studio).<br>
하나의 CLI · 하나의 MCP 서버 · 하나의 웹 대시보드.</p>

<pre><code># 설치 (예정)
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh

# 소스 빌드
git clone https://github.com/kernalix7/rtrt
cd rtrt
cargo install --path crates/rtrt-cli</code></pre>

[![Alpha](https://img.shields.io/badge/status-alpha-orange?style=for-the-badge)](#상태-알파)
[![Latest](https://img.shields.io/github/v/release/kernalix7/rtrt?include_prereleases&style=for-the-badge&label=latest&color=2962FF)](https://github.com/kernalix7/rtrt/releases)

[![license](https://img.shields.io/github/license/kernalix7/rtrt?style=flat-square&color=blue)](../LICENSE)
[![rust](https://img.shields.io/badge/rust-1.85%2B-CE412B?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![edition](https://img.shields.io/badge/edition-2024-CE412B?style=flat-square)](https://doc.rust-lang.org/edition-guide/)
[![CI](https://img.shields.io/github/actions/workflow/status/kernalix7/rtrt/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/kernalix7/rtrt/actions/workflows/ci.yml)
[![stars](https://img.shields.io/github/stars/kernalix7/rtrt?style=flat-square&color=FFD93D&logo=github&logoColor=white)](https://github.com/kernalix7/rtrt/stargazers)

<sub>[English](../README.md) &nbsp;·&nbsp; **한국어** &nbsp;·&nbsp; [설치](INSTALL.ko.md) &nbsp;·&nbsp; [사용법](USAGE.ko.md) &nbsp;·&nbsp; [기능](FEATURES.ko.md) &nbsp;·&nbsp; [아키텍처](ARCHITECTURE.ko.md) &nbsp;·&nbsp; [디자인](DESIGN.ko.md) &nbsp;·&nbsp; [성능](PERF.ko.md) &nbsp;·&nbsp; [비교](COMPARISON.ko.md)</sub>

</div>

---

> ### 상태: 알파
> RTRT는 초기 단계입니다. **v0.1.0**으로 머무릅니다. 워크스페이스가 컴파일되고, 출력 압축 · 명령 출력 필터링 · SQLite-FTS5 BM25 회수 · 자동 캡처 파이프라인 (`redact_secrets` → SHA-256 dedup → save → 세션 태깅) · 시간당 콘솔리데이션 데몬 · MCP 18개 도구 · 대시보드 SSE 라이브 스트림이 모두 동작합니다. 첫 정식 태그 (`v0.2.0-rc1`)는 라이브 키 + 브라우저 스모크 통과 후. 회귀 게이트는 `docs/PERF.ko.md` SLO 기반.

RTRT는 세 기둥만 합칩니다. [`DESIGN.ko.md`](DESIGN.ko.md)에 10개 원칙, [`PERF.ko.md`](PERF.ko.md)에 SLO 표 + 최신 측정값. 프레임워크가 아닌 유닉스 도구 모음 — 안정된 substrate (SQLite / Markdown / JSONL / SHA-256 / Rust / tree-sitter / MCP / SSE / 파이프) 위에 얇은 러스트 레이어. 멀티 에이전트 코디네이션은 옵션 크레이트로 분리 (scope creep 거부). 핵심 크레이트 zero-`unsafe`, edition 2024.

## 빠른 설치

원라이너 (Linux / macOS / WSL):

```bash
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | bash
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/kernalix7/rtrt/main/install.ps1 | iex
```

또는 빌드 채널 선택:

```bash
# main HEAD
curl -fsSL .../install.sh | bash -s -- --main

# 특정 태그 / 브랜치 / 커밋
curl -fsSL .../install.sh | bash -s -- --ref my-feature

# 로컬 클론 (오프라인 / 에어갭)
sh install.sh --source ~/code/rtrt
```

전체 플래그 매트릭스, 환경 변수 (`RTRT_REF` / `RTRT_SOURCE` / `RTRT_SKIP_DEPS`), 소스 빌드, 버전 고정, 제거는 [INSTALL.ko.md](INSTALL.ko.md) 참고.

## 실행

```bash
rtrt compress -l ultra < verbose.md
rtrt compress --llm --provider openai-compat \
   --base-url http://127.0.0.1:11434/v1 --model llama3.2 < verbose.md
rtrt proxy "git status" < git-status-output
rtrt signatures --lang rust < src/file.rs
rtrt repo-map crates/rtrt-core
rtrt discover
rtrt templates
rtrt new rust-cli ./hello --var project_name=hello
rtrt setup --agent claude --apply
rtrt memory save --project p --kind note "fact"
rtrt memory recall --project p --query rust
rtrt prompt save greet "say hi"
rtrt prompt get greet
rtrt docs facebook/react --topic hooks
rtrt provider chat --model claude-haiku-4-5 "ping"
rtrt-dashboard
rtrt-mcp --memory ~/.rtrt/memory.sqlite
```

전체 명령은 [USAGE.ko.md](USAGE.ko.md)에 있습니다.

## 세 기둥

- **토큰 절감** — `lite` / `full` / `ultra` / `extreme` 룰 압축 + 코드 블록 보호 + 시크릿 자동 검열 + 토큰-중요도 ML 압축 (휴리스틱 스코어러, ONNX 백엔드 추후) + tree-sitter 시그니처 (Rust / Python / TypeScript) + `git` / `cargo` 명령 출력 필터.
- **영구 프로젝트 메모리** — SQLite + FTS5 BM25 + 벡터 + 그래프 + HNSW + Reciprocal Rank Fusion 하이브리드 + qdrant 스타일 페이로드 필터 DSL + Letta 블록 (persona / human / context) + JSONL export/import. **자동 캡처가 기본**: `/api/*` 호출과 모든 Claude Code 훅(12종) 발화가 `redact_secrets` → SHA-256 dedup (5분 윈도우) → save → `session_id` 태깅 파이프라인을 거침. 시간당 콘솔리데이션 데몬이 프로젝트별 row 캡 유지 (`RTRT_CONSOLIDATE_KEEP`).
- **멀티 프로바이더 라우팅** — Anthropic / OpenAI / OpenAI 호환 (Ollama / vLLM / LM Studio / llama.cpp) — 로컬 우선. `Gateway` + 예산 + 응답 캐시 (Helicone 스타일) + exponential 재시도 + Anthropic 프롬프트 캐시 휴리스틱. `Context7Client` 라이브러리 문서 페치. 자동 캡처 + 콘솔리데이션은 옵트인 LLM 압축 모드와 결합 가능.

세 기둥을 감싸는 표면:

- **`rtrt-mcp`** — rmcp 1.7 stdio + Streamable HTTP. 18개 도구 (compress / compress_ml / proxy / memory_save / memory_recall / memory_timeline / memory_profile / memory_relations / memory_smart_search / memory_export / memory_consolidate / memory_sessions / memory_set_block / memory_get_block / memory_list_blocks / repo_map / templates_list / templates_scaffold / provider_chat). `--http-token` 상수-시간 베어러 가드 + RFC 6454 Origin 검증. 모든 핸들러가 자동 캡처 파이프라인 통과.
- **`rtrt-dashboard`** — axum 10탭 (Metrics SVG 스파크라인 / Budget / Prompts / Memory / Templates / Compression / Proxy / Diagnose / RepoMap / Setup). `/api/stream` SSE 라이브 활동, `/api/tokens/summary` 게이트웨이 시간/일 집계, `/api/memory/{projects,timeline}` 페이지네이션. `RTRT_DASHBOARD_TOKEN` 베어러 미들웨어, 다크모드.
- **Claude Code 플러그인** — `plugins/claude-code/rtrt/` 훅 12종 (PreToolUse / PostToolUse / PostToolUseFailure / PreCompact / UserPromptSubmit / PostUserPromptSubmit / Notification / Stop / SubagentStart / SubagentStop / SessionStart / SessionEnd). CLI 우선, 대시보드 POST 폴백.
- **에이전트 와이어업** — `rtrt setup --agent claude/cursor/codex/windsurf/aider --apply`.
- **개발자 도구** — `rtrt signatures`, `rtrt repo-map`, `rtrt discover`, `rtrt benchmark`.

## 문서

| 문서 | 내용 |
|------|------|
| [INSTALL.ko.md](INSTALL.ko.md) | 설치 경로 — 소스 / crates.io(예정) / 바이너리(예정) |
| [USAGE.ko.md](USAGE.ko.md) | CLI · MCP · 대시보드 사용법 + 자동 캡처 파이프라인 |
| [FEATURES.ko.md](FEATURES.ko.md) | 압축 규칙 / 필터 / 메모리 스키마 / 템플릿 |
| [ARCHITECTURE.ko.md](ARCHITECTURE.ko.md) | 크레이트 경계 · 데이터 흐름 |
| [DESIGN.ko.md](DESIGN.ko.md) | 10개 원칙 (프레임워크 아닌 도구 · 세 기둥 · 측정값 성능) |
| [PERF.ko.md](PERF.ko.md) | SLO 표 + 최신 측정값 + 10% 회귀 정책 |
| [COMPARISON.ko.md](COMPARISON.ko.md) | 참조 프로젝트들과의 비교 |
| [INSPIRATION.ko.md](INSPIRATION.ko.md) | 15개 이상 AI 도구 프로젝트의 아이디어 백로그 |
| [CHANGELOG.ko.md](CHANGELOG.ko.md) | 전체 변경 이력 |
| [CONTRIBUTING.ko.md](CONTRIBUTING.ko.md) | 개발 환경 설정과 워크플로우 |
| [SECURITY.ko.md](SECURITY.ko.md) | 보안 신고 절차 |

## 로드맵

- [x] 워크스페이스 스캐폴드 (9 크레이트, edition 2024)
- [x] `rtrt-compress` 규칙 + extreme + 시크릿 검열 + tree-sitter 시그니처 (Rust / Python / TypeScript) + LLM 압축 모드 + 토큰-중요도 ML 압축
- [x] `rtrt-proxy` git / cargo 필터 (MCP 도구 노출)
- [x] `rtrt-memory` BM25 + 벡터 + HNSW + RRF 하이브리드 + 그래프 + 메모리 스코프 + 페이로드 필터 DSL + Letta 블록 + JSONL export/import
- [x] `rtrt-memory` v5 스키마 (`session_id` + `body_sha` + 커버링 인덱스) — 100K rows 타임라인 32µs
- [x] `rtrt-templates` 빌트인 6종 + agent-role + handlebars + 버저닝되는 `PromptRegistry`
- [x] `rtrt-providers` Anthropic / OpenAI / OpenAI-compat HTTP + 스트리밍 + Gateway + Budget + 응답 캐시 + exponential 재시도 + Anthropic 프롬프트 캐시 휴리스틱 + Context7
- [x] `rtrt-mcp` rmcp stdio + Streamable HTTP, 18 도구, 베어러 가드, RFC 6454 Origin
- [x] `rtrt-dashboard` axum 10탭 + SSE 라이브 + 토큰 집계 + 자동 캡처 파이프라인 + 시간당 콘솔리데이션 데몬
- [x] Claude Code 플러그인 12개 훅 (`plugins/claude-code/rtrt/`)
- [x] `install.sh` + `install.ps1` 원라이너 + `release.yml` 5-target 매트릭스
- [x] `rtrt setup --agent <name>` Claude / Cursor / Codex / Windsurf 와이어업
- [x] criterion 벤치 + per-fixture 절감률 테이블 + `recall_bench` (1K/10K/100K)
- [x] CI 회귀 게이트 (`.github/workflows/perf.yml` + `scripts/perf-gate.sh`) — 10% p50 회귀 차단
- [x] `DESIGN.md` + `PERF.md` — 10개 원칙 + SLO 표 + 측정값 정책
- [x] LLM 자동 압축 백그라운드 태스크 (옵트인) — `RTRT_AUTO_COMPRESS_LLM=1` 활성화, `set_body` + `compress_candidates` 기반
- [x] `rtrt-eval` 옵션 크레이트 — R@K / MRR / compression ratio + 내장 smoke fixture (외부 fixture 드랍 가능)
- [x] `scripts/smoke.sh` — 라이브 키 스모크 하니스 (PASS / FAIL / SKIP 표 + 베어러 가드 401 검증)
- [x] ONNX token-importance 백엔드 (`MlCompressor` 옵트인 `--features onnx`; 사용자 제공 모델 + 토크나이저)
- [x] `rtrt-eval` BERTScore 평가기 (옵트인 `--features bertscore`; 사용자 제공 인코더 + 토크나이저)
- [x] MCP Prompts + Resources — 로컬 PromptRegistry 기반 `prompts/list` / `prompts/get` (handlebars 인자) + 프로젝트 타임라인 + Letta 블록 기반 `resources/list` / `resources/read`
- [ ] 옵션 멀티 에이전트 코디네이션 크레이트 (DESIGN.md 명시 deferred)
- [ ] 첫 정식 태그 릴리스 — 라이브 키 스모크 + 브라우저 투어 통과 후, 사용자 승인 받고 진행. 그때까지 버전 라벨은 `0.1.0` 유지

## 라이선스

[MIT](../LICENSE) — Kim DaeHyun (kernalix7@kodenet.io)
