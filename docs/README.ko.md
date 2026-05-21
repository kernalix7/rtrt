<div align="center">

# RTRT

### 토큰은 줄이되, 의미는 유지. 러스트 기반 통합 툴킷.

<p>출력 단순화, 명령 출력 필터링, 영구 프로젝트 메모리,<br>
멀티 프로바이더 라우팅, 표준화된 프로젝트 스캐폴드 —<br>
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

<sub>[English](../README.md) &nbsp;·&nbsp; **한국어** &nbsp;·&nbsp; [설치](INSTALL.ko.md) &nbsp;·&nbsp; [사용법](USAGE.ko.md) &nbsp;·&nbsp; [기능](FEATURES.ko.md) &nbsp;·&nbsp; [아키텍처](ARCHITECTURE.ko.md) &nbsp;·&nbsp; [비교](COMPARISON.ko.md)</sub>

</div>

---

> ### 상태: 알파
> RTRT는 초기 단계입니다. **v0.1.0**은 스캐폴드 릴리스입니다. 워크스페이스가 컴파일되고, 출력 압축 / 명령 출력 필터링 / SQLite-FTS5 BM25 회수 / 템플릿 스캐폴딩은 모두 동작합니다. 다만 MCP 전송 계층, 프로바이더 채팅 클라이언트, 벡터 임베딩, 원라이너 설치 스크립트는 [로드맵](#로드맵)에 명시된 스텁입니다. 이슈는 <https://github.com/kernalix7/rtrt/issues>에 올려주세요.

RTRT는 네 가지 토큰 절감 기법을 하나의 CLI · MCP 서버 · 웹 대시보드로 통합합니다. 전체가 러스트(edition 2024)로 작성되었고, 핵심 크레이트에는 `unsafe`가 없습니다. 참조 프로젝트의 코드를 벤더링하지 않고 러스트로 재구현합니다.

## 빠른 설치

소스 빌드(현재 권장):

```bash
git clone https://github.com/kernalix7/rtrt
cd rtrt
cargo install --path crates/rtrt-cli
```

원라이너 설치는 예정입니다(아직 미연결):

```bash
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh
```

자세한 설치 경로는 [INSTALL.ko.md](INSTALL.ko.md)를 참고하세요.

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

## 핵심 기능

- **출력 압축** — `lite` / `full` / `ultra` / `extreme` 4단계 + 코드 블록 보호 + 시크릿 자동 검열 + LLM 압축 모드(Ollama OK).
- **명령 출력 필터링** — `git` / `cargo` 빌트인 필터; MCP 도구로도 노출.
- **영구 메모리** — SQLite + FTS5 BM25 + 벡터 + 그래프 + HNSW + LLM extract/compress; 메모리 스코프(user/agent/session/project).
- **멀티 프로바이더 라우팅** — Anthropic / OpenAI / OpenAI 호환(Ollama, llama.cpp 등); `Gateway` + 예산 + 트레이스; `Context7Client` 라이브러리 문서 페치.
- **프로젝트 스캐폴드 + 프롬프트** — 빌트인 6종 + 커스텀 + handlebars 렌더링; 버저닝되는 `PromptRegistry`.
- **MCP + 대시보드** — `rtrt-mcp` rmcp stdio + Streamable HTTP, 11 도구 (compress / compress_ml / proxy / memory_*(letta blocks 포함) / templates_* / provider_chat), `--http-token` 베어러 가드 + RFC 6454 Origin 검증. `rtrt-dashboard` axum 10탭 (Metrics SVG 스파크라인 / Budget / Prompts / Memory / Templates / Compression / Proxy / Diagnose / RepoMap / Setup), 다크모드, `RTRT_DASHBOARD_TOKEN` 미들웨어.
- **에이전트 와이어업** — `rtrt setup --agent claude/cursor/codex/windsurf/aider --apply`.
- **개발자 도구** — `rtrt signatures`, `rtrt repo-map`, `rtrt discover`(셸 히스토리 스캔).

## 문서

| 문서 | 내용 |
|------|------|
| [INSTALL.ko.md](INSTALL.ko.md) | 설치 경로 — 소스 / crates.io(예정) / 바이너리(예정) |
| [USAGE.ko.md](USAGE.ko.md) | CLI / MCP / 대시보드 사용법 |
| [FEATURES.ko.md](FEATURES.ko.md) | 압축 규칙 / 필터 / 메모리 스키마 / 템플릿 |
| [ARCHITECTURE.ko.md](ARCHITECTURE.ko.md) | 크레이트 경계 · 데이터 흐름 |
| [COMPARISON.ko.md](COMPARISON.ko.md) | 참조 프로젝트들과의 비교 |
| [INSPIRATION.ko.md](INSPIRATION.ko.md) | 15개 이상 AI 도구 프로젝트의 아이디어 백로그 |
| [CHANGELOG.ko.md](CHANGELOG.ko.md) | 전체 변경 이력 |
| [CONTRIBUTING.ko.md](CONTRIBUTING.ko.md) | 개발 환경 설정과 워크플로우 |
| [SECURITY.ko.md](SECURITY.ko.md) | 보안 신고 절차 |

## 로드맵

- [x] 워크스페이스 스캐폴드 (9 크레이트, edition 2024)
- [x] `rtrt-compress` 규칙 + extreme + 시크릿 검열 + tree-sitter 시그니처 + LLM 압축 모드
- [x] `rtrt-proxy` git / cargo 필터
- [x] `rtrt-memory` BM25 + 벡터 + RRF 하이브리드 + 그래프 + HNSW + 메모리 스코프
- [x] `rtrt-memory` LLM extract / compress / archival (Ollama 등 모든 Provider)
- [x] `rtrt-templates` 빌트인 6종 + handlebars + 버저닝되는 `PromptRegistry`
- [x] `rtrt-providers` Anthropic / OpenAI / OpenAI-compat HTTP + 스트리밍 + Gateway + Budget + Context7
- [x] `rtrt-mcp` rmcp stdio 6 도구
- [x] `rtrt-dashboard` axum + REST API (chat / metrics / templates / stats)
- [x] `install.sh` + `install.ps1` 원라이너 + `release.yml` 5-target 매트릭스
- [x] `rtrt setup --agent <name>` Claude / Cursor / Codex / Windsurf 와이어업
- [x] criterion 벤치 + per-fixture 절감률 테이블
- [ ] MCP HTTP / SSE 전송 (stdio는 출시 완료)
- [ ] `caveman-shrink` MCP 도구 설명 압축 미들웨어
- [ ] LLM 엔티티 추출 기반 `recall_via_graph` (mem0 엔티티 링킹)
- [ ] Helicone 스타일 재시도 / 폴백 라우팅
- [ ] 첫 태그 릴리스 (`v0.2.0-rc1`)

## 라이선스

[MIT](../LICENSE) — Kim DaeHyun (kernalix7@kodenet.io)
