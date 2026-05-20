# 기능

[English](FEATURES.md) | **한국어**

각 토큰 절감 표면과 스캐폴딩 기능의 구현 세부 사항입니다.

## 출력 압축

`rtrt-compress`는 정규식 기반 재작성기로, 두 단계로 동작합니다.

1. **보호 단계** — `PROTECT`가 코드 펜스(` ``` `), 인라인 코드(` ` `), `https?://…` URL, `"…"` 인용 문자열을 찾아 불투명한 플레이스홀더(`\u{0001}RTRT_PROTECT_<n>\u{0002}`)로 교체합니다. 원문은 슬롯 테이블에 저장됩니다.
2. **규칙 단계** — 레벨별 순서가 있는 규칙 집합이 `Regex::replace_all`로 적용됩니다.
   - `lite` — 필러 + 다중 공백 압축.
   - `full` — `lite` + 인사말.
   - `ultra` — `full` + 관사.
3. **복원 단계** — 플레이스홀더를 원문으로 되돌립니다.

보호 대상은 의도적으로 보수적이므로, 코드 / URL / 오류 메시지 등 기술 콘텐츠는 절대 재작성되지 않습니다.

API:

```rust
use rtrt_compress::Compressor;
use rtrt_core::CompressionLevel;

let c = Compressor::new(CompressionLevel::Ultra);
let out = c.compress("the bug is `really` in the parser");
// out: "bug is `really` in parser"
```

## 명령 출력 필터링

`rtrt-proxy`는 작은 디스패치 테이블을 제공합니다. 각 `CommandFilter`는 `command` 접두사와 `apply` 함수를 가집니다.

현재 빌트인 필터:

| 명령 접두사 | 전략 |
|-------------|------|
| `git status` | `On branch …`, `Your branch …`, `(use …)`, `nothing to commit …` 줄 제거 + 빈 줄 압축 |
| `git log` | `Author:` / `Date:` 줄 제거 + 빈 줄 압축 |
| `cargo build` | `Compiling …`, `Finished …`, `Downloading …`, `Downloaded …` 줄 제거 |
| `cargo test` | `cargo build`와 동일 |

`filter_for("<command>")`는 첫 매칭 필터를 반환합니다. 매칭되지 않으면 원문이 그대로 전달됩니다.

## 영구 메모리

`rtrt-memory`는 SQLite 파일(기본 `.rtrt/memory.sqlite`)을 열고 첫 실행 시 마이그레이션을 적용합니다. 스키마:

```sql
CREATE TABLE memories (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project     TEXT NOT NULL,
    kind        TEXT NOT NULL,
    body        TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE INDEX idx_memories_project ON memories(project);

CREATE VIRTUAL TABLE memories_fts
    USING fts5(body, content='memories', content_rowid='id');

CREATE TABLE embeddings (
    memory_id   INTEGER PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
    model       TEXT NOT NULL,
    vector      BLOB NOT NULL
);

CREATE TABLE edges (
    src_id      INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    dst_id      INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation    TEXT NOT NULL,
    PRIMARY KEY (src_id, dst_id, relation)
);
```

v0.1.0은 `memories_fts`를 통한 BM25 회수를 구현합니다.

```rust
let store = MemoryStore::open(".rtrt/memory.sqlite")?;
store.save("my-project", "note", "Rust is a systems language")?;
let hits = store.recall_bm25("my-project", "rust", 5)?;
```

`embeddings`와 `edges` 테이블은 v0.2 예약 영역입니다. 임베딩은 로컬에서 `all-MiniLM-L6-v2`(오프라인)를 사용하고, 회수는 BM25 + 벡터 코사인 + 그래프 순회를 Reciprocal Rank Fusion으로 결합할 예정입니다.

## 멀티 프로바이더 라우팅

`rtrt-providers`는 `Provider` 트레이트를 정의합니다.

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn supported_models(&self) -> &[&'static str];
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse>;
}
```

빌트인 어댑터:

- `AnthropicProvider` — 기본 URL `https://api.anthropic.com/v1`. 모델: `claude-opus-4-7`, `claude-sonnet-4-6`, `claude-haiku-4-5`.
- `OpenAIProvider` — 기본 URL `https://api.openai.com/v1`. 모델: `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.3-codex-spark`.
- `OpenAICompatibleProvider` — 사용자 지정 베이스 URL. Ollama, llama.cpp, vLLM, LM Studio 등 OpenAI 호환 엔드포인트 대상.

v0.1.0의 `chat` 구현은 모두 `Error::Provider("... not implemented yet")`을 반환합니다. 실제 채팅 라우팅은 로드맵 항목입니다.

## 프로젝트 스캐폴드

`rtrt-templates`는 빌트인 6종을 코드 상수로 제공합니다(외부 파일 임베딩 없음). 각 템플릿은 `Template { name, description, source, variables, files, post_hooks }` 구조입니다.

빌트인:

| 이름 | 결과 |
|------|------|
| `rust-cli` | `clap` + `anyhow` + `tracing` 기반 러스트 바이너리; `git init` 훅 |
| `rust-lib` | `add` 예제 테스트가 포함된 러스트 라이브러리 |
| `rust-axum` | `axum` + `tokio` 기반 HTTP 서비스 |
| `node-typescript` | `tsx`를 쓰는 ESM 타입스크립트; `npm install` 훅 |
| `python-uv` | `uv sync` 친화적 `pyproject.toml` |
| `go-cli` | `go.mod`을 갖춘 최소 Go CLI; `go mod tidy` 훅 |

공용 변수:

- `project_name` (필수)
- `author` (기본 `Unknown`)
- `license` (기본 `MIT`)

변수 치환은 `{{key}}`. 경로에도 치환이 적용되어 `src/{{project_name}}/__init__.py` → `src/hello/__init__.py`로 풀립니다.

### 커스텀 템플릿

```
~/.rtrt/templates/
└── my-template/
    ├── manifest.toml
    ├── Cargo.toml.tmpl
    └── src/main.rs.tmpl
```

`manifest.toml`:

```toml
name = "my-template"
description = "My custom Rust scaffold"
post_hooks = ["git init"]

[[variables]]
name = "project_name"
description = "프로젝트 이름"
required = true

[[files]]
path = "Cargo.toml"
source = "Cargo.toml.tmpl"

[[files]]
path = "src/main.rs"
source = "src/main.rs.tmpl"
```

각 `[[files]]` 항목은 `source`(매니페스트 디렉터리 기준 상대 경로) 또는 인라인 `content`를 사용합니다. 둘 다 변수 치환이 적용됩니다.

## MCP와 대시보드

`rtrt-mcp`는 현재 스텁이며 예정 도구(`compress`, `memory.save`, `memory.recall`, `provider.chat`) 목록만 출력합니다. stdio 전송 계층은 로드맵.

`rtrt-dashboard`는 기본적으로 `127.0.0.1:3111`에 바인딩하는 axum 서버입니다.

- `/` — HTML 인덱스(절감 통계 + 템플릿 갤러리)
- `/api/stats` — JSON 절감 통계
- `/api/templates` — JSON 템플릿 목록
- `/api/templates/{name}` — 매니페스트 전체
- `/api/templates/scaffold` — POST 스캐폴드 엔드포인트

POST 본문 형식은 CLI `rtrt new`와 동일한 `{ template, target, variables, overwrite }`입니다.
