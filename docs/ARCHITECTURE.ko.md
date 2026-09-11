# 아키텍처

[English](ARCHITECTURE.md) | **한국어**

## 다이어그램

```
+--------------------+      +--------------------+      +--------------------+
|     rtrt-cli       |----->|     rtrt-mcp       |      |  rtrt-dashboard    |
|  (원라이너 진입)    |     |  (MCP stdio/HTTP)  |      |     (axum 웹)      |
+---------+----------+      +----------+---------+      +----------+---------+
          |                            |                           |
          v                            v                           v
+----------------------------------------------------------------------------+
|                                rtrt-core                                   |
|   플러그인 트레이트 · 설정 · 에러 · 토큰 회계 · 텔레메트리                   |
+----------------------------------------------------------------------------+
   |           |           |            |              |           |
   v           v           v            v              v           v
+--------+ +--------+ +----------+ +------------+ +----------+ +----------+
| rtrt-  | | rtrt-  | | rtrt-    | | rtrt-      | | rtrt-    | | rtrt-    |
| compr. | | proxy  | | memory   | | providers  | | templ.   | | security |
+--------+ +--------+ +----------+ +------------+ +----------+ +----------+
```

`rtrt-eval`(옵트인 평가 하네스, 자체 `rtrt-eval` 바이너리)은 라이브러리
크레이트 옆에 위치하며 `rtrt-core` + `rtrt-compress` + `rtrt-memory`를 사용합니다.

## 크레이트 경계

| 크레이트 | 공개 API | 의존성 |
|----------|----------|--------|
| `rtrt-core` | `Error`, `Result`, `CompressionLevel`, `TokenCount`, `TokenStats`, `Plugin`, `PluginKind`, `PluginMetadata`, `Config` | `serde`, `serde_json`, `thiserror`, `async-trait` |
| `rtrt-compress` | `Compressor::new`, `Compressor::compress`, `redact_secrets` | `rtrt-core`, `regex`, `once_cell` |
| `rtrt-proxy` | `filter_for`, `CommandFilter`, `FILTERS` | `rtrt-core`, `regex`, `once_cell` |
| `rtrt-memory` | `MemoryStore` (`open`, `save`, `save_embedded`, `recall_bm25`, `recall_vector`, `recall_hybrid`, `list_by_project`, `delete`, `extract_and_save`, `compress_project`), `Embedder` 트레이트 (+ `FastEmbedder` `embeddings` 피처), `Summariser` 트레이트 (+ `LlmSummariser` `llm` 피처), `MemoryRecord`, `ScoredRecord` | `rtrt-core`, `rusqlite`(bundled), `serde`, `tokio`; 선택: `fastembed`, `rtrt-providers` |
| `rtrt-providers` | `Provider`, `ChatRequest`, `ChatResponse`, 어댑터 3종 | `rtrt-core`, `reqwest`, `serde`, `tokio` |
| `rtrt-templates` | `Template`, `RenderPlan`, `builtin::ALL`, `custom::scan_default_dir`, `render::plan`, `render::write` | `rtrt-core`, `toml`, `walkdir`, `dirs`, `once_cell` |
| `rtrt-security` | `Profile`, `Rule`, `Finding`, `ScanReport`, `Severity`, `Standards`, `Engine` 트레이트, `run`, `BUILTIN_PROFILES`, `list_profiles`, `load_profile` | `rtrt-core`, `regex`, `walkdir`, `toml`, `dirs`, `once_cell` |
| `rtrt-eval` | 바이너리 `rtrt-eval` + `evaluate_recall`, `evaluate_compression`, 픽스처 로더 (옵트인 `bertscore` 피처) | `rtrt-core`, `rtrt-compress`, `rtrt-memory` |
| `rtrt-mcp` | 바이너리 `rtrt-mcp` | core / compress / memory / proxy / providers / templates / security + tokio |
| `rtrt-dashboard` | 바이너리 `rtrt-dashboard` | core / compress / memory / proxy / providers / templates / security + axum + tower |
| `rtrt-cli` | 바이너리 `rtrt` | 모든 라이브러리 크레이트(security 포함) + clap |

## 소스 트리

```
.
├── Cargo.toml                       # 워크스페이스 · 공유 의존성 · 프로필
├── rust-toolchain.toml              # stable 핀
├── LICENSE                          # MIT
├── README.md / *.md                 # 영문 표준 문서
├── docs/                            # 다국어 문서
├── .github/                         # 워크플로우 · 템플릿
└── crates/                          # 11개 크레이트
```

## 데이터 흐름

### 압축

1. 호출자(`rtrt compress`, `rtrt-mcp`, 라이브러리)가 `Compressor::new(level)` 생성.
2. `compress(&input)`이 **보호 → 규칙 → 복원** 파이프라인을 실행.
3. 재작성된 `String` 반환.

`Compressor`는 `Copy`이며 호출 간 상태가 없습니다.

### 메모리

1. `MemoryStore::open(path)`이 필요 시 마이그레이션 실행.
2. `save(project, kind, body)`이 `memories`와 `memories_fts`에 동시 삽입.
3. `recall_bm25(project, query, limit)`이 FTS5 랭크 + `project` 필터를 조인.

벡터 회수는 `embeddings` 기능의 `recall_vector` / `recall_hybrid`로 제공됩니다. 방향 관계는 `edges`에 저장하며 `recall_via_graph`가 그래프 기반 회수를 위해 이를 순회합니다.

### 템플릿

1. `list_all()` = 빌트인 + `custom::scan_default_dir()`.
2. `find(name)`이 단일 템플릿 조회.
3. `render::plan(template, target_dir, vars)`이 필수 변수 검증 + 기본값 적용 + 치환된 `RenderPlan` 반환.
4. `render::write(plan, overwrite)`이 파일 기록 + 실행 비트 설정.
5. 포스트-인스톨 훅은 `std::process::Command`로 실행되며 라인을 공백 기준으로 분리합니다(셸 사용 안 함).

### 프로바이더 채팅

채팅은 Anthropic Messages, OpenAI Chat Completions, OpenAI 호환 엔드포인트(Ollama, llama.cpp, vLLM, LM Studio)의 실제 HTTP API에 연결됩니다. 스트리밍은 `Stream<Item = ChatStreamEvent>`를 반환하는 `chat_stream`으로 제공되며, 이벤트는 `Delta` / `Usage` / `Done`이고 공유 SSE 디코더는 `stream.rs`에 있습니다. 등록된 프로바이더 앞의 `Gateway`는 대시보드가 실시간 토큰 지출을 관찰할 수 있도록 요청마다 `RequestMetric { provider, model, started_at, latency_ms, usage, ok }`를 기록합니다.

## 동시성 모델

- 비동기 런타임: `tokio` 멀티스레드.
- 메모리 스토어는 동기(`rusqlite`가 블로킹)이므로 비동기 호출 측은 `tokio::task::spawn_blocking`을 씌웁니다.
- HTTP 서버: `axum` 0.8 / `hyper`.
- HTTP 클라이언트: `reqwest` 0.12 + `rustls-tls` (시스템 OpenSSL 의존 없음).

## 빌드 프로필

- `dev` — `opt-level = 0`, 디버그 정보 포함.
- `release` — `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `strip = "symbols"`. 배포 바이너리 기본값.

## Cargo 리졸버

워크스페이스는 `resolver = "3"` 사용(Rust 1.84에서 안정화). v3는 [워크스페이스 멤버별 feature unification](https://doc.rust-lang.org/cargo/reference/resolver.html#feature-unification)을 활성화하고 버전 선택 시 패키지별 MSRV를 존중합니다. MSRV는 `1.85`이며, `rust-toolchain.toml` 핀과 `stable` + `beta` CI 매트릭스 모두 이 조건을 만족합니다.
