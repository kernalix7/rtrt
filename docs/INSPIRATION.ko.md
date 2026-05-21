# 영감 출처 (Inspiration)

[English](INSPIRATION.md) | **한국어**

RTRT는 다른 토큰 절감 · 메모리 · 에이전트 도구 프로젝트에서 아이디어를 차용합니다. 본 문서는 출처 · 차용 아이디어 · 매핑 크레이트 · 우선순위를 기록합니다. 실제 채택 시 `CHANGELOG.md`에 `(inspired by [...])`로 인라인 크레딧, 법적 attribution은 [`THIRD_PARTY_LICENSES.md`](../THIRD_PARTY_LICENSES.md#reference-projects-inspiration-only-no-code-redistributed)에 둡니다.

우선순위 — **high** 명확한 이득, 다음 마이너 큐 / **medium** 향후, 표면 안정화 후 / **low** 욕심 / 부가 기능.

## 출력 압축

| Project | 아이디어 | 크레이트 | 우선순위 |
|---------|---------|----------|----------|
| [JuliusBrussee/caveman](https://github.com/JuliusBrussee/caveman) | 도구 설명을 감싸는 MCP 미들웨어(`caveman-shrink`) | `rtrt-mcp` | high |
| [JuliusBrussee/caveman](https://github.com/JuliusBrussee/caveman) | `/caveman-compress`로 메모리 파일 영구 재작성 | `rtrt-compress` + `rtrt-memory` | high |
| [JuliusBrussee/caveman](https://github.com/JuliusBrussee/caveman) | 누적 절감 토큰을 표시하는 상태바 배지 | `rtrt-dashboard` + `rtrt-proxy` | medium |
| [JuliusBrussee/caveman](https://github.com/JuliusBrussee/caveman) | 한문(wenyan) 변형 — 추가 압축 단계 | `rtrt-compress` | low |
| [microsoft/LLMLingua](https://github.com/microsoft/LLMLingua) | 소형 LM 토큰 분류기로 비중요 토큰 제거(~20×) | `rtrt-compress` | high |
| [microsoft/LLMLingua](https://github.com/microsoft/LLMLingua) | LongLLMLingua: 컨텍스트 재배치 + 동적 비율(RAG lost-in-middle 해결) | `rtrt-compress` + `rtrt-memory` | high |
| [microsoft/LLMLingua](https://github.com/microsoft/LLMLingua) | LLMLingua-2 BERT 급 증류 인코더, 3-6× 가속 | `rtrt-compress` | medium |
| [yamadashy/repomix](https://github.com/yamadashy/repomix) | tree-sitter `--compress` — 시그니처만 추출, 본문 제거 | `rtrt-compress` | high |
| [yamadashy/repomix](https://github.com/yamadashy/repomix) | 팩 전 secretlint 스캔(LLM 도달 전 시크릿 차단) | `rtrt-compress` + `rtrt-proxy` | high |
| [yamadashy/repomix](https://github.com/yamadashy/repomix) | 다중 출력 형식(XML/MD/Plain) + 파일별 토큰 카운트 | `rtrt-compress` + `rtrt-core` | medium |

## 명령 출력 필터링

| Project | 아이디어 | 크레이트 | 우선순위 |
|---------|---------|----------|----------|
| [rtk-ai/rtk](https://github.com/rtk-ai/rtk) | `discover` — 히스토리에서 놓친 절감 기회 스캔 | `rtrt-cli` + `rtrt-dashboard` | high |
| [rtk-ai/rtk](https://github.com/rtk-ai/rtk) | 에이전트별 설치 도구(`init --agent cursor/windsurf/…`) | `rtrt-cli` + `rtrt-templates` | high |
| [rtk-ai/rtk](https://github.com/rtk-ai/rtk) | `--ultra-compact` ASCII 아이콘 모드(추가 절감 단계) | `rtrt-proxy` + `rtrt-compress` | medium |
| [rtk-ai/rtk](https://github.com/rtk-ai/rtk) | `err <cmd>` / `test <cmd>` 범용 래퍼(에러만 출력) | `rtrt-proxy` | medium |
| [rtk-ai/rtk](https://github.com/rtk-ai/rtk) | 세션 채택 분석(`rtk session`, `gain --graph`) | `rtrt-dashboard` | medium |
| [Aider-AI/aider](https://github.com/Aider-AI/aider) | 자동 lint + test 루프, 실패만 LLM에 환류 | `rtrt-proxy` + `rtrt-core` | medium |

## 영구 메모리

| Project | 아이디어 | 크레이트 | 우선순위 |
|---------|---------|----------|----------|
| [rohitg00/agentmemory](https://github.com/rohitg00/agentmemory) | SQLite + FTS5 BM25 베이스라인 (v0.1 채택 완료) | `rtrt-memory` | shipped |
| [rohitg00/agentmemory](https://github.com/rohitg00/agentmemory) | `all-MiniLM-L6-v2` 기본 임베딩 (v0.2 채택 완료, fastembed 경유) | `rtrt-memory` | shipped |
| [rohitg00/agentmemory](https://github.com/rohitg00/agentmemory) | Reciprocal Rank Fusion 하이브리드 회수 (v0.2 채택 완료) | `rtrt-memory` | shipped |
| [rohitg00/agentmemory](https://github.com/rohitg00/agentmemory) | `edges` 테이블 기반 지식 그래프 엔티티 매칭 | `rtrt-memory` | high |
| [rohitg00/agentmemory](https://github.com/rohitg00/agentmemory) | 에이전트 간 메모리 공유 데몬(`:3111` HTTP) | `rtrt-memory` + `rtrt-mcp` | low (프로젝트별 SQLite가 기본 의도) |
| [mem0ai/mem0](https://github.com/mem0ai/mem0) | 다층 메모리(user / session / agent 범위) | `rtrt-memory` | high |
| [mem0ai/mem0](https://github.com/mem0ai/mem0) | 하이브리드 회수: 시맨틱 + BM25 + 엔티티 링킹 | `rtrt-memory` | high |
| [mem0ai/mem0](https://github.com/mem0ai/mem0) | 한 번에 ADD-only LLM 추출(저비용 · 저토큰) | `rtrt-memory` + `rtrt-providers` | medium |
| [chroma-core/chroma](https://github.com/chroma-core/chroma) | 삽입 시 자동 임베드 + 플러그블 임베딩 함수 | `rtrt-memory` | high |
| [chroma-core/chroma](https://github.com/chroma-core/chroma) | 컬렉션 CRUD + 메타데이터 필터 쿼리 API | `rtrt-memory` + `rtrt-mcp` | high |
| [letta-ai/letta](https://github.com/letta-ai/letta) | 메모리 블록(persona / human / context, 구조화) | `rtrt-memory` | high |
| [letta-ai/letta](https://github.com/letta-ai/letta) | 컨텍스트 윈도우 매니저: 오버플로우 → 아카이벌(FTS/임베드 회수) | `rtrt-compress` + `rtrt-memory` | high |
| [letta-ai/letta](https://github.com/letta-ai/letta) | 에이전트 도구 호출로 자체 편집되는 메모리 | `rtrt-memory` + `rtrt-mcp` | medium |
| [cpacker/MemGPT](https://github.com/cpacker/MemGPT) | 자체 편집 가능한 계층형 메모리 블록 | `rtrt-memory` | high |
| [cpacker/MemGPT](https://github.com/cpacker/MemGPT) | 가상 컨텍스트 페이징: 핫 컨텍스트 ↔ 아카이벌 | `rtrt-memory` | high |
| [qdrant/qdrant](https://github.com/qdrant/qdrant) | 벡터용 HNSW ANN 인덱스 | `rtrt-memory` | high |
| [qdrant/qdrant](https://github.com/qdrant/qdrant) | 스칼라 / 바이너리 양자화(RAM 최대 97% 절감) | `rtrt-memory` | medium |
| [qdrant/qdrant](https://github.com/qdrant/qdrant) | JSON 페이로드 필터 DSL(range / geo / bool) | `rtrt-memory` | medium |
| [lancedb/lancedb](https://github.com/lancedb/lancedb) | 벡터 + FTS + SQL 통합 쿼리 표면 | `rtrt-memory` + `rtrt-cli` | medium |
| [lancedb/lancedb](https://github.com/lancedb/lancedb) | 컬럼나 Lance 포맷 + 무복사 버저닝 | `rtrt-memory` | low |
| [neuml/txtai](https://github.com/neuml/txtai) | 파이프라인 / 워크플로우 DAG 합성 | `rtrt-core` + `rtrt-cli` | medium |
| [neuml/txtai](https://github.com/neuml/txtai) | 그래프 + 벡터 + 관계형 통합 저장소 | `rtrt-memory` | medium |
| [Aider-AI/aider](https://github.com/Aider-AI/aider) | 레포 맵(그래프 중심성 랭크 + 가지치기, tree-sitter 태그) | `rtrt-compress` + `rtrt-memory` | high |

## 멀티 프로바이더 라우팅

| Project | 아이디어 | 크레이트 | 우선순위 |
|---------|---------|----------|----------|
| [Helicone/helicone](https://github.com/Helicone/helicone) | 키 하나로 여러 프로바이더 게이트웨이 | `rtrt-providers` + `rtrt-proxy` | high |
| [Helicone/helicone](https://github.com/Helicone/helicone) | 요청별 자동 비용 / 지연 / 토큰 지표 | `rtrt-proxy` + `rtrt-dashboard` | high |
| [Helicone/helicone](https://github.com/Helicone/helicone) | 프로바이더 폴백 + 재시도 라우팅 | `rtrt-providers` | medium |
| [Helicone/helicone](https://github.com/Helicone/helicone) | 다중 턴 에이전트 흐름의 세션 트레이스 | `rtrt-dashboard` + `rtrt-mcp` | medium |
| [sobelio/llm-chain](https://github.com/sobelio/llm-chain) | 단일 트레이트 + 다중 모델 백엔드 | `rtrt-providers` | high |
| [sobelio/llm-chain](https://github.com/sobelio/llm-chain) | 재사용 가능한 프롬프트 템플릿 + 체이닝 기본 | `rtrt-templates` + `rtrt-core` | medium |
| [upstash/context7](https://github.com/upstash/context7) | `/org/lib` ID 기반 버전 고정 라이브러리 문서 페치 | `rtrt-providers` + `rtrt-mcp` | high |
| [upstash/context7](https://github.com/upstash/context7) | 이중 전달: MCP 서버 + CLI 스킬 모드(MCP 불필요) | `rtrt-mcp` + `rtrt-cli` | high |
| [upstash/context7](https://github.com/upstash/context7) | OAuth 키 설정 위저드(`rtrt setup` 원샷 와이어업) | `rtrt-cli` | medium |
| [mufeedvh/code2prompt](https://github.com/mufeedvh/code2prompt) | git diff/log/branch-compare를 컨텍스트에 주입 | `rtrt-core` + `rtrt-providers` | medium |

## 템플릿 & 스캐폴드

| Project | 아이디어 | 크레이트 | 우선순위 |
|---------|---------|----------|----------|
| [mufeedvh/code2prompt](https://github.com/mufeedvh/code2prompt) | Handlebars 템플릿으로 프롬프트 셰이핑 | `rtrt-templates` | high |
| [crewAIInc/crewAI](https://github.com/crewAIInc/crewAI) | LangChain-free 런타임 — 순수 Rust · 파이썬 의존 없음 미러 | `rtrt-core` | high |
| [crewAIInc/crewAI](https://github.com/crewAIInc/crewAI) | 특화 에이전트용 role / goal / backstory 스키마 | `rtrt-templates` | medium |
| [crewAIInc/crewAI](https://github.com/crewAIInc/crewAI) | Crews + Flows: 자율 에이전트 + 결정적 이벤트 기반 워크플로우 | `rtrt-core` + `rtrt-templates` | medium |
| [dust-tt/dust](https://github.com/dust-tt/dust) | 노코드 에이전트 빌더 UI | `rtrt-dashboard` | medium |
| [dust-tt/dust](https://github.com/dust-tt/dust) | JS SDK + 외부 통합용 API 문서 | `rtrt-core` + `rtrt-dashboard` | medium |

## 옵저버빌리티 & 비용 추적

| Project | 아이디어 | 크레이트 | 우선순위 |
|---------|---------|----------|----------|
| [langfuse/langfuse](https://github.com/langfuse/langfuse) | LLM 호출 트레이스 계측 | `rtrt-providers` + `rtrt-dashboard` | high |
| [langfuse/langfuse](https://github.com/langfuse/langfuse) | 버저닝되는 프롬프트 레지스트리 + 서버 캐시 | `rtrt-templates` + `rtrt-dashboard` | high |
| [langfuse/langfuse](https://github.com/langfuse/langfuse) | 평가 데이터셋 + LLM-as-judge 스코어링 | `rtrt-dashboard` | low |
| [Doriandarko/claude-engineer](https://github.com/Doriandarko/claude-engineer) | 라이브 토큰 예산 미터 + 컨텍스트 윈도우 매니저 | `rtrt-dashboard` + `rtrt-core` | high |
| [Doriandarko/claude-engineer](https://github.com/Doriandarko/claude-engineer) | 런타임에 핫로드되는 자체 생성 도구 | `rtrt-mcp` + `rtrt-core` | medium |

## 수렴 테마

여러 출처가 같은 방향을 가리키는 항목 — RTRT가 빠르게 채택해야 함:

1. **하이브리드 회수(BM25 + 벡터 + 엔티티 / 그래프)** — mem0, chroma, qdrant, lancedb, letta 모두 동의. `rtrt-memory` v0.2/v0.3 스키마 목표.
2. **tree-sitter 기반 압축** — repomix · aider. `rtrt-compress`에서 가장 큰 차용 효과; 시그니처-only 모드는 새 절감 단계.
3. **빌트인 옵저버빌리티 포함 멀티 프로바이더 게이트웨이** — Helicone + Langfuse + llm-chain 수렴. `rtrt-providers` + `rtrt-proxy` + `rtrt-dashboard`에 매핑.
4. **메모리 계층 + 가상 컨텍스트 페이징** — Letta · MemGPT. 기존 `rtrt-compress` 아카이벌 파이프라인과 자연스럽게 짝.
5. **에이전트별 설치 도구 / 설정 위저드** — rtk + context7. `rtrt` CLI 온보딩 마찰 최소화.

## 즉시 채택 후보 (v0.3 큐)

다음 조건을 모두 만족하는 항목 — 새 의존성 없이 기존 크레이트에 매핑, 독립적으로 사용 가능, 2개 이상 참조가 같은 형태 제안:

1. **`compress.tree_sitter` 모드** in `rtrt-compress` — 시그니처 추출, 본문 제거. 출처: repomix · aider.
2. **`memory.recall_hybrid`** in `rtrt-memory` — BM25 + 벡터 + 엔티티, Reciprocal Rank Fusion. 출처: mem0 · chroma · qdrant. (v0.2는 BM25 + 벡터 예정, 엔티티는 v0.3.)
3. **`providers.gateway`** in `rtrt-providers` — 여러 프로바이더 앞의 단일 키, 요청별 비용 / 지연 지표를 `rtrt-dashboard`로 흘림. 출처: Helicone · Langfuse · llm-chain.
4. **`rtrt setup --agent <name>`** in `rtrt-cli` — Claude Code / Cursor / Windsurf / Codex / Aider에 한 줄로 와이어업, rtk `init` 미러.
5. **`rtrt-compress secretlint` 사전 검사** in `rtrt-compress` — LLM 도달 전 시크릿 차단. 출처: repomix.

## 이 백로그에서 이미 출시된 항목

위 표에 `shipped`로 표시된 항목은 미래 작업이 아니라, RTRT 기능을 영감 출처로 추적할 수 있도록 명시한 것입니다. v0.2 기준:

- `rtrt-memory` SQLite + FTS5 BM25 (agentmemory).
- `rtrt-memory` fastembed 경유 `all-MiniLM-L6-v2` 임베딩 (agentmemory).
- `rtrt-memory` Reciprocal Rank Fusion 하이브리드 회수 (agentmemory).
- `rtrt-memory` LLM 기반 extract + compress (agentmemory의 cloud-only 압축을 RTRT의 Provider 트레이트로 로컬 Ollama까지 확장 — agentmemory 대비 RTRT의 부가가치).
- `rtrt-providers` 멀티 프로바이더 채팅 트레이트 (llm-chain).
- `rtrt-providers` OpenAI 호환 어댑터로 Ollama / llama.cpp / vLLM / LM Studio 커버 (helicone 게이트웨이 + 실용적 재사용).
- `rtrt-compress` 시크릿 패턴 자동 검열기 (repomix secretlint 검사).

## 사용 방법

이 페이지는 *영감 백로그*이며 로드맵이 아닙니다. 실제 로드맵은 [`README.md`](../README.md#roadmap)와 [`CHANGELOG.md`](../CHANGELOG.md)에 있습니다. 항목이 로드맵으로 이동하거나 릴리스에 반영되면 릴리스 노트에 `(inspired by [project-name](url))` 인라인 크레딧이 붙고 `THIRD_PARTY_LICENSES.md`의 "Reference projects" 섹션이 갱신됩니다.
