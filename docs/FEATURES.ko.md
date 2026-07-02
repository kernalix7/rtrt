# 기능

[English](FEATURES.md) | **한국어**

각 토큰 절감 표면과 스캐폴딩 기능의 구현 세부 사항입니다.

## 출력 압축

`rtrt-compress`는 정규식 기반 재작성기로, 세 단계로 동작합니다.

1. **검열 단계** — 시크릿 패턴(AWS 키, GitHub PAT, OpenAI / Anthropic / Slack 토큰, Bearer 인증, `api_key=…` 일반 패턴, PEM private-key 블록)을 `<REDACTED:<kind>>`로 치환. 규칙 패스나 다운스트림 LLM에 시크릿이 전혀 닿지 않음.
2. **보호 단계** — `PROTECT`가 코드 펜스(` ``` `), 인라인 코드(` ` `), `https?://…` URL, `"…"` 인용 문자열을 찾아 불투명한 플레이스홀더(`\u{0001}RTRT_PROTECT_<n>\u{0002}`)로 교체합니다. 원문은 슬롯 테이블에 저장됩니다.
3. **규칙 단계** — 레벨별 순서가 있는 규칙 집합이 `Regex::replace_all`로 적용됩니다.
   - `lite` — 필러 + 다중 공백/개행 압축.
   - `full` — `lite` + 인사말 + 헤지(`I think`, `perhaps`, …) + 담화 표지(`moreover`, `however`, …) + 메타 표현(`it is important to note that`, …).
   - `ultra` — `full` + 관사(`a`/`an`/`the`) + 관용구 축약(`due to the fact that` → `because`, `in order to` → `to`, `a number of` → `several`, `for instance` → `e.g.` 등).
   - `extreme` — `ultra` + 강조 부사(`very`, `extremely`, `quite`, `rather`, …).
4. **복원 단계** — 플레이스홀더를 원문으로 되돌립니다.

보호 대상은 의도적으로 보수적이므로, 코드 / URL / 오류 메시지 등 기술 콘텐츠는 절대 재작성되지 않습니다.

API:

```rust
use rtrt_compress::Compressor;
use rtrt_core::CompressionLevel;

let c = Compressor::new(CompressionLevel::Ultra);
let out = c.compress("I think the bug is, perhaps, in the parser.");
// out: "bug is, in parser."
```

### 압축 절감률

`scripts/bench.sh`가 `crates/rtrt-compress/benches/fixtures/`의 fixture를 가지고 측정한 글자 수 감소율.

| Fixture | `lite` | `full` | `ultra` | `extreme` |
|---------|-------:|-------:|--------:|----------:|
| `short` (대화체 AI 답변) |  6% | 25% | **32%** | 32% |
| `mixed` (산문+코드 혼합) |  3% | 12% | 18% | **19%** |
| `long`  (긴 설명문) |  2% | 10% | **15%** | 15% |
| `code`  (코드 중심) |  2% |  3% |  6% | 6% |

규칙 기반 패스의 한계:

- **할 수 있는 것**: 필러, 인사말, 헤지, 담화 표지, 관사, 강조 부사 제거 + 관용구 축약.
- **할 수 없는 것**: 자연어 산문에서 caveman의 60-75% 클레임 달성은 불가능. 그 수치는 "LLM이 처음부터 짧게 쓰기로 합의"한 결과이지, 사후 정규식 삭제 결과가 아님.

caveman급 절감률은 LLM 모드(`llm-compress` 피처)로 달성. [`LlmCompressor`](https://docs.rs/rtrt-compress/latest/rtrt_compress/struct.LlmCompressor.html)는 모든 `Provider`(로컬 Ollama 포함)를 통해 모델에게 재작성을 위탁. caveman과 동일 원리이지만 기존 문자열에 사후 적용 가능.

```bash
# 로컬 Ollama (무료 · 첫 풀 이후 오프라인)
ollama pull llama3.2
echo "I think the bug is, perhaps, in the parser..." | rtrt compress --llm \
  --provider openai-compat --base-url http://127.0.0.1:11434/v1 --model llama3.2

# 클라우드 Anthropic
ANTHROPIC_API_KEY=... rtrt compress --llm \
  --provider anthropic --model claude-haiku-4-5 < passage.md
```

### tree-sitter 시그니처 추출 (`treesitter` 피처)

코드 중심 응답에서는 `SignatureExtractor`가 AST를 순회하며 최상위 시그니처만 출력 — `fn` 헤더, `struct` / `enum` / `trait` / `type` / `const` 선언, `impl` 블록 헤더 + 메서드 시그니처. 모든 함수 본문을 `{ /* body */ }`로 치환.

```bash
rtrt signatures --lang rust < src/anthropic.rs
# 일반 Rust 소스 기준 70–80% 바이트 절감
```

현재 그래머: Rust(`tree-sitter-rust`). 다른 언어는 해당 그래머 크레이트를 활성화하고 [`Language`](https://docs.rs/rtrt-compress/latest/rtrt_compress/enum.Language.html)를 확장하면 됨. CLI 바이너리는 `treesitter`가 기본 활성. 라이브러리 사용자는 `features = ["treesitter"]`로 옵트인.

### 시크릿 검열

검열기는 규칙 패스 **이전**에 실행되므로 `lite`에서도 시크릿이 제거됩니다. 패턴:

- `aws-access-key`: `AKIA…` / `ASIA…` 20자 키.
- `aws-secret`: `aws_secret_access_key=…` 40자 base64.
- `github-pat`: `ghp_…` 40자 PAT.
- `github-token`: `gh[opsur]_…` (fine-grained 등).
- `openai-key`: `sk-…` / `sk-proj-…`.
- `anthropic-key`: `sk-ant-…`.
- `slack-token`: `xox[abprs]-…`.
- `bearer-token`: `Authorization: Bearer …`.
- `private-key`: `-----BEGIN … PRIVATE KEY-----` 블록.
- `generic-api-key`: `api_key=…` / `apikey=…` (문맥 필요).

각 매치는 `<REDACTED:<kind>>`로 치환. 멱등 — 이미 검열된 텍스트 재실행은 no-op.

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

BM25 회수는 `memories_fts`를 통합니다.

```rust
let store = MemoryStore::open(".rtrt/memory.sqlite")?;
store.save("my-project", "note", "Rust is a systems language")?;
let hits = store.recall_bm25("my-project", "rust", 5)?;
```

벡터 및 하이브리드 회수는 `Embedder`가 필요합니다. 기본은 [`fastembed`](https://crates.io/crates/fastembed) 기반 `all-MiniLM-L6-v2`(384차원, ONNX, 첫 다운로드 후 오프라인). 기능 게이트:

```toml
[dependencies]
rtrt-memory = { version = "0.2", features = ["embeddings"] }
```

사용:

```rust
use rtrt_memory::{MemoryStore, FastEmbedder};

let store = MemoryStore::open(".rtrt/memory.sqlite")?;
let embedder = FastEmbedder::new_default()?;
store.save_embedded("my-project", "note", "Rust is a systems language", &embedder)?;
let hits = store.recall_hybrid("my-project", "rust toolchain", 5, &embedder)?;
```

회수 세부:

- **`recall_bm25`** — FTS5 내장 BM25 랭크, 프로젝트 스코프, 임베더 불필요.
- **`recall_vector`** — 쿼리를 임베드하고 프로젝트 메모리 전체를 코사인 유사도로 채점 후 정렬. 저장된 임베딩 수에 선형; 향후 HNSW 인덱스로 교체.
- **`recall_hybrid`** — BM25 + 벡터의 Reciprocal Rank Fusion(`rrf_k = 60`). 단일 스트림에만 등장하는 항목도 떠오르도록 각 스트림을 `limit * 2`만큼 가져옴.

`edges` 테이블은 그래프 순회 예약.

**첫 사용 주의**: `FastEmbedder::new_default()`는 fastembed 캐시 디렉터리로 모델(~90 MB)을 처음에 다운로드. 이후는 오프라인.

### LLM 기반 추출 + 압축 (`llm` 피처)

LLM이 필요한 두 가지 메모리 연산:

- **Extract** — 긴 텍스트를 원자 사실 리스트로 분해, 한 항목당 한 행 저장. 미리 가공된 prose 보관 회피.
- **Compress** — 프로젝트의 가장 오래된 메모리들을 한 아카이벌 요약으로 응축, 원본 삭제. 워킹 풀이 커지면 사용.

둘 다 [`Summariser`](https://docs.rs/rtrt-memory/latest/rtrt_memory/summarise/trait.Summariser.html) 트레이트로 흐릅니다. 빌트인 구현 `LlmSummariser`는 `rtrt_providers::Provider`라면 무엇이든 감싸므로 Anthropic / OpenAI / OpenAI 호환 로컬 엔드포인트 동일 코드.

#### 로컬 LLM via Ollama (무료 / 오프라인 권장)

Ollama가 `/v1/chat/completions`를 OpenAI 포맷으로 노출 → 새 어댑터 불필요. 기존 `OpenAICompatibleProvider`로 바로 동작:

```bash
# 일회 설정
ollama pull llama3.2          # qwen2.5:7b, gemma2:9b 등도 가능
ollama serve                  # 기본 127.0.0.1:11434

# 긴 텍스트를 프로젝트 "p1"의 원자 사실로 추출
echo "RTRT 아키텍처 설명…" | rtrt memory extract \
  --project p1 \
  --provider openai-compat \
  --base-url http://127.0.0.1:11434/v1 \
  --model llama3.2

# 압축: 최근 20개 유지, 나머지 요약
rtrt memory compress \
  --project p1 \
  --keep 20 \
  --provider openai-compat \
  --base-url http://127.0.0.1:11434/v1 \
  --model llama3.2
```

#### 클라우드 LLM (Anthropic / OpenAI)

```bash
ANTHROPIC_API_KEY=… rtrt memory extract \
  --project p1 --provider anthropic --model claude-haiku-4-5 \
  < passage.txt

OPENAI_API_KEY=… rtrt memory compress \
  --project p1 --keep 10 --provider openai --model gpt-5.4-mini
```

CLI 명령은 라이브러리 API의 `MemoryStore::extract_and_save` / `MemoryStore::compress_project`로 라우팅됩니다.

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

`chat` / `chat_stream`은 세 어댑터 모두 실제 HTTP API로 구현되어 있으며, `Gateway`가 요청별 메트릭 · 옵션 예산 캡 · 응답 캐시 · 재시도/폴백을 얹어 모든 프로바이더를 감쌉니다.

### 사용량 원장 + 윈도우 헤드룸 (`rtrt usage`)

모든 프로바이더 호출은 로컬 사용량 원장 `~/.rtrt/provider-usage.tsv`에 한 줄씩 기록됩니다(`RTRT_PROVIDER_USAGE_PATH`로 경로 재정의):

```text
epoch_ts \t target \t model \t input_tokens \t output_tokens \t est \t ok
```

- 파일은 최근 5000행으로 상한; 기록은 best-effort라 호출을 실패시키지 않습니다.
- CLI 셸-아웃은 텍스트만 반환하므로 토큰 수는 추정치(~chars/4)이며 끝까지 `est`로 표시 — 추정 행은 `~`로 렌더링됩니다.
- 사용량은 타깃별 롤링 **5h / 24h / 7d** 윈도우로 집계; `rtrt usage`가 표를 출력합니다(`--format json` 지원).
- **헤드룸**은 24h 윈도우를 `~/.rtrt/config.toml`의 옵션 일일 상한과 비교합니다:

```toml
[limits.openai]
daily_tokens = 1_000_000
daily_requests = 2_000

[limits.ollama]
daily_tokens = 250_000
```

`[limits]` 항목이 없는 타깃은 상한 없음으로 보고 — RTRT는 상한을 지어내지 않습니다.

### 헤드룸 가중 라우팅 + 자동 페일오버 (`rtrt route`)

`rtrt route`는 프롬프트에 가장 저렴하면서 쓸모 있는 타깃을 고릅니다. 랭킹은 비용 계층 우선 — 로컬-무료 → 구독-정액 → API-종량 → 미상 — 그리고 계층 내부에서 헤드룸을 반영합니다:

- 제한된 차원(토큰 또는 요청) 중 가장 빠듯한 쪽이 **~15% 미만** 남은 후보는 같은 비용 계층 안에서 감점.
- 어느 한 차원이라도 완전히 **소진**(잔여 0)된 타깃은 계층과 무관하게 맨 밑으로 강등 — 최후의 폴백으로만 사용.
- 동률은 잔여 헤드룸 비율이 큰 타깃이 승리.

`rtrt route --explain`은 결정 · 랭크된 대안 · 판단에 쓰인 사용량/헤드룸을 출력; `--dry-run`은 호출 없이 결정만 출력합니다.

`--failover` 지정 시 `rtrt route`(그리고 `rtrt call --failover`)는 랭크된 후보 목록을 순회합니다: **재시도 가능** 실패(rate-limit / quota / 429 / 5xx / 타임아웃)는 다음 랭크 타깃으로 넘어가고, 종결성 오류는 순회를 멈추며, 결과에 어떤 타깃이 왜 넘어갔는지 요약이 담깁니다.

대시보드는 `GET /api/usage`(타깃별 윈도우 사용량 + 헤드룸)와 `GET /api/route/preview`(프롬프트 없이 *다음* 요청의 로드밸런싱 결정)를 사용량/헤드룸 게이지와 라우팅 프리뷰(Tools 쪽)로 렌더링합니다.

## 보안 & 라이선스 스캔

`rtrt-security`는 AI 생성 산출물을 위한 프로파일 기반 스캐너로, OpenSCAP식 선언적 프로파일을 모델로 합니다. 스캔마다 다섯 엔진이 실행됩니다:

| 엔진 | 검사 내용 |
|------|-----------|
| `secrets` | 빌트인 시크릿 패턴 + Shannon 엔트로피 게이트; 발췌는 마스킹 |
| `licenses` | SPDX 매니페스트 정책(허용/금지 목록), 옵션 헤더 검사, 워크스페이스 상속 인지 |
| `deps` | Cargo.lock / package-lock 위생(git / 와일드카드 / yanked) + 옵션 오프라인 RustSec advisory 매칭 |
| `patterns` | 언어 + 경로 필터를 갖춘 정규식 소스 스캐너 |
| `ai` | AI 산출물 특화: 환각-임포트 / slopsquatting, base64 블롭, eval 사용, TODO-시크릿, unsafe 블록 |

빌트인 프로파일 6종(`ai-default`, `ai-strict`, `owasp-top-10`, `asvs-l2`, `cis-baseline`, `nist-ssdf`)은 모든 룰을 산업 표준(CWE / OWASP / NIST / CIS / SLSA / EU AI Act)에 매핑합니다. 프로파일은 선언적 TOML — `~/.rtrt/security/profiles/`에 직접 넣어 빌트인을 재정의하거나 추가할 수 있습니다.

```bash
rtrt security scan --profile ai-default [--path DIR] [--json]
rtrt security profile list
rtrt security profile show ai-strict
rtrt security gate --profile ai-default        # 임계 이상이면 non-zero 종료 — CI 게이트
rtrt security init                             # 빌트인을 ~/.rtrt/security/profiles/로 복사
```

같은 스캐너가 대시보드 보안 페이지(프로젝트 인지: 선택된 프로젝트 경로를 바인딩된 프로파일로 스캔)와 MCP `security_scan` 도구를 지탱합니다.

## 2단 설정 & 프로젝트 라이프사이클

설정은 두 층으로 나뉩니다:

1. **글로벌 베이스 커널** — `~/.rtrt/config.toml` + 에이전트 배선(훅 / MCP / 스테이터스라인 커맨드 바인딩). `rtrt setup`이 관리하며 프로젝트가 재정의할 수 없습니다.
2. **프로젝트별 오버라이드** — `<repo>/.rtrt/config.toml`(`ProjectConfig`): 출력 레벨(`off` / `lite` / `full` / `ultra`), 압축, 프로젝트별 에이전트 + 프로바이더 활성화, 스테이터스라인 — 모두 옵션. 비어 있는 필드는 글로벌 값을 상속; 유효 설정 = 글로벌 ⊕ 프로젝트(`Config::load_effective`). 오버라이드가 전부 기본값이면 파일을 삭제해 저장소를 깨끗하게 유지합니다.

대시보드는 프로젝트별 표면마다 **글로벌 따름 / 커스텀** 스코프 토글을 제공합니다.

라이프사이클 명령:

```bash
rtrt migrate [--path DIR] [--apply]      # 기존 저장소를 rtrt 프로젝트 표준으로 이관 (기본 dry-run)
rtrt project refresh [--apply]           # 원커맨드 별칭: 컨트랙트 렌더 → 표준 설정 활성화 → 일관성 감사
rtrt project status | health | repair    # 표준화 컨트랙트 점검 / 검증 / 복구
```

`rtrt migrate` / `rtrt project refresh`는 프로젝트 레벨의 rtrt 소유 키 섀도(예: 프로젝트 `.claude/settings.json`이 `statusLine`을 재선언)를 감지해 `.bak` 백업과 함께 제거하고, 프로젝트가 글로벌 베이스 커널을 따르게 합니다.

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

`rtrt-mcp`는 stdio + Streamable HTTP를 지원하는 실제 rmcp 기반 MCP 서버로, 메모리 / 압축 / 프록시 / 템플릿 / 프로바이더 / 보안 도구 표면을 노출합니다(전체 도구 표는 [USAGE.ko.md](USAGE.ko.md) 참고).

`rtrt-dashboard`는 기본적으로 `127.0.0.1:7311`에 바인딩하는 axum 서버입니다.

- `/` — HTML 인덱스(절감 통계 + 템플릿 갤러리)
- `/api/stats` — JSON 절감 통계
- `/api/templates` — JSON 템플릿 목록
- `/api/templates/{name}` — 매니페스트 전체
- `/api/templates/scaffold` — POST 스캐폴드 엔드포인트

POST 본문 형식은 CLI `rtrt new`와 동일한 `{ template, target, variables, overwrite }`입니다.
