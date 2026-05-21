# RTRT — 디자인 원칙

[English](../DESIGN.md) | **한국어**

RTRT는 두 가지 일을 한다.

1. **AI 에이전트가 낭비하는 토큰을 줄이고, 의미는 지킨다.**
2. **에이전트가 한 모든 일을 기억하고, 다음 세션이 지난 세션 끝지점에서 시작한다.**

압축 / 명령 출력 필터링 / 영구 메모리 / 멀티-프로바이더 라우팅 / 프로젝트 스캐폴드 — 전부 위 두 목표를 위해.

이 문서는 "왜 좁게 유지하는가"를 적은 규율 노트다.

## 1. 프레임워크가 아니라 Unix 도구

프레임워크는 죽고 도구는 산다.

- Atom · Meteor.js · CoffeeScript · Backbone — 한 시대를 풍미하고 5년 안에 사라진 프레임워크.
- `grep` · `awk` · `sed` · SQLite · `ripgrep` · `fd` · `jq` — 한 가지 일만 하는 도구들. 그 주변의 모든 프레임워크보다 오래 산다.

RTRT는 단일-목적 바이너리들로 나온다.

```bash
rtrt compress -l ultra < verbose.md
rtrt memory recall --project rtrt --query auth
rtrt signatures --lang rust < src/lib.rs
rtrt-mcp --transport stdio
rtrt-dashboard
```

각 도구는 stdin 읽고 stdout 쓰고 끝. `|` `>` `&`로 조합 가능. 데몬 불필요. 프레임워크 의존 없음. 한 번 추가된 CLI 플래그는 수년간 유지.

## 2. 안정된 기반 substrate

오래 살아남는 소프트웨어는 자신보다 오래된 기반 위에 짓는다.

| Substrate | 왜 오래 가나 | RTRT 사용처 |
|----------|-------------|------------|
| SQLite | Public domain, ~30년 | `rtrt-memory` 저장소 |
| FTS5 / BM25 | SQLite 네이티브 풀텍스트 | `recall_bm25` |
| Markdown | 평문, 버전관리 가능 | 템플릿 + 프롬프트 |
| JSON Lines | 줄 단위 레코드, grep 친화 | `memory export/import` |
| SHA-256 | 암호 기본형, 수십 년 안정 | dedup 인덱스 |
| Rust (edition 2024) | 메모리 안전, 안정 ABI | 모든 크레이트 |
| tree-sitter | 에디터급 파서 (GitHub / Neovim / Helix) | `signatures` / `repo-map` |
| MCP | 오픈 표준, 멀티 벤더 | `rtrt-mcp` |
| Server-Sent Events | RFC 6202, 광범위 지원 | `/api/stream` |
| Unix 파이프 / stdio | 이 프로젝트 저자보다 오래됨 | 모든 CLI |

다음에는 **베팅하지 않는다**:

- 단일 LLM 벤더 API (모두 `Provider` trait 뒤로).
- 단일 에이전트 프레임워크 (LangChain · AutoGen · CrewAI 등은 매주 변함).
- 클라우드 전용 서비스 (AWS · GCP · OpenAI 단일 의존 등).
- 2년 미만 실전 검증 안 된 hype 추상화 (스킬 시스템 · 가상 에이전트 · 멀티-에이전트 메시).

## 3. 3개 기둥만

모든 기능은 셋 중 하나에 들어가야 한다.

1. **토큰 절감.** 의미 안 잃고 프롬프트 줄임.
   - `rtrt-compress` 룰 + ML 엔진, tree-sitter 시그니처, 시크릿 redact, chroma-style 다중 포맷.
2. **영구 프로젝트 메모리.** 에이전트가 한 일 캡처 → 나중에 회수. 세션 / 머신 / 도구 넘어 생존.
   - `rtrt-memory` SQLite + FTS5 + 벡터 + RRF 하이브리드 + 그래프 + dedup + privacy + 시간당 콘솔리데이션.
3. **멀티-프로바이더 라우팅 (로컬 우선).** 모든 LLM 하나의 trait 뒤로. Ollama / OpenAI / Anthropic 교체 가능. 예산 인식. 응답 캐시.
   - `rtrt-providers` Gateway + Budget + retry / fallback + 응답 캐시.

세 버킷 중 어느 것에도 안 들면 **옵션 크레이트** (§5) 또는 거절.

## 4. 자동 캡처가 기본값

쓰기가 수동이면 메모리 저장소는 무용지물. 자동 캡처는 옵트인이 아니라 1급 기본 동작.

파이프라인 (이벤트 발생 시):

```
event fires
  ├─ 1. SHA-256 dedup (5분 윈도우, 설정 가능)
  ├─ 2. Privacy 필터 (AWS · GitHub · OpenAI · Anthropic · Slack · Bearer
  │     · private-key · api_key=… 저장 전 redact)
  ├─ 3. Raw 관찰 SQLite 저장 (FTS5 + BM25 자동 인덱싱)
  ├─ 4. Session id 태깅 (프로세스당 UUID, 훅은 자체 ID 통과)
  └─ 5. (옵션) 백그라운드 LLM 압축 → facts / concepts
```

환경 변수로 모든 knob 노출 ([`docs/USAGE.md`](USAGE.md) 참고):

| 환경 변수 | 기본 | 효과 |
|----------|------|------|
| `RTRT_AUTO_CAPTURE` | `1` | 마스터 스위치 |
| `RTRT_AUTO_REDACT` | `1` | Privacy 필터 ON/OFF |
| `RTRT_AUTO_DEDUP_WINDOW_SEC` | `300` | dedup 윈도우 |
| `RTRT_CONSOLIDATE_INTERVAL_SEC` | `3600` | 시간당 archive sweep |
| `RTRT_CONSOLIDATE_KEEP` | `1000` | sweep 후 프로젝트당 유지 row |

모든 레코드는 **사용자가 명시적으로 지우기 전까지 영구**. 콘솔리데이션은 요약하고 prune할 뿐, keep 임계치 안 넘긴 row를 조용히 버리지 않음.

## 5. 옵션 크레이트로 확장, 코어 폭주 금지

세 기둥에 안 맞는 매력적 기능은 **별도 크레이트** 또는 **기능 플래그** 뒤로.

현재:

- `rtrt-compress[treesitter]` — tree-sitter 그래머 (기본 OFF, 아티팩트 30MB ↑).
- `rtrt-compress[llm-compress]` — LLM 압축 경로.
- `rtrt-memory[embeddings]` — fastembed ONNX 런타임.
- `rtrt-memory[hnsw]` — `instant-distance` ANN.
- `rtrt-memory[llm]` — `LlmSummariser`.

코어 밖 후보:

- `rtrt-orchestrator` — 멀티-에이전트 코디네이션 (액션 / 신호 / 리스 / 메시 / 센티넬). 진화 중 아이디어를 SQLite 스키마에 묶으면 비싸짐. 자체 크레이트 + 옵트인.
- `rtrt-snapshot` — git 버전 메모리 스냅샷.
- `rtrt-eval` — 라벨링된 데이터셋 대비 회수 정확도 벤치.

기본 설치는 가벼움 유지.

## 6. 성능은 측정값으로만

"엄청 빠르다" 안 함. "p99 = 443µs at 100k rows, 커밋 a1b2c3 / 2024 노트북" 만. SLO 표 [`docs/PERF.md`](PERF.md).

릴리스마다 criterion 스위트 재실행. 10% 이상 회귀는 릴리스 블로커.

## 7. 로컬 우선, privacy 우선

- 기본 설치는 **외부 서비스 호출 0**. SQLite 로컬, 대시보드 루프백, compress / proxy / repo-map 완전 오프라인.
- LLM은 사용자가 가리킨 엔드포인트로 — Ollama `127.0.0.1:11434`, 자체 호스트 vLLM, 또는 클라우드. 선택.
- 자동 캡처 path에서 시크릿은 디스크 도달 전 redact.
- `rtrt-mcp` HTTP / `rtrt-dashboard` 베어러 미들웨어가 루프백 외 비인증 차단.

## 8. 발행된 인터페이스는 영원

태그된 릴리스에 한 번 들어간 CLI 플래그 · MCP 도구명 · SQLite 컬럼 · JSON 필드는 안 바꿈. 추가만. 데이터 포맷은 forward-portable — v0.1.0 이후 모든 메모리 저장소가 모든 미래 버전에서 열림.

스키마 마이그레이션은 `PRAGMA user_version` 올리고 **추가만** (컬럼 / 인덱스). 이름 변경 / 제거는 메이저 버전 + `CHANGELOG.md` 마이그레이션 플랜 필요.

## 9. 작게, 천천히, 깊게

- 분기당 진짜 기능 1-2개. 12개 아님.
- 폴리시 > 폭 넓힘. 버그 없는 `memory_save`가 반쪽짜리 5개보다 가치 높음.
- 문서는 코드와 같이. 나중에 아님.
- 한국어 / 영어 doc 동기화.

## 10. 수용 가능한 리스크

- MCP 표준이 어림. 추적은 하지만 베팅은 안 함. CLI + 라이브러리는 그대로.
- 벡터 임베딩은 서드파티 모델 의존. 기본은 로컬 모델. 클라우드 임베딩은 옵트인.
- 자동 캡처 path가 노이즈를 좀 수집. dedup + privacy로 완화, 콘솔리데이션으로 prune. "많은데 요약 가능" > "이벤트 놓침".

---

## 이건 안 한다

- **53 도구 MCP 표면.** 더 큰 메모리 플랫폼과 도구 수 경쟁 안 함. 10-15개 잘 다듬어진 도구로 멈춤.
- **코어에 멀티-에이전트 코디네이션.** 신호 · 리스 · 메시 · 센티넬 — 18개월 더 검증되면 `rtrt-orchestrator` 옵션 크레이트로.
- **클라우드 전용 / 유료 전용 기능.** 모든 것이 노트북에서 오프라인 동작.
- **프레임워크 위 프레임워크.** 에이전트 런타임 · 오케스트레이션 DSL · 플러그인 마켓플레이스 없음.

이 문서는 의도적으로 짧다. 새 최상위 기능 추가 전 다시 읽기.
