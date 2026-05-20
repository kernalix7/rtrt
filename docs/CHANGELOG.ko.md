# 변경 이력

[English](../CHANGELOG.md) | **한국어**

이 문서는 프로젝트의 주요 변경 사항을 모두 기록합니다.

형식은 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) 1.1.0을 따르고, 버전 규칙은 [Semantic Versioning](https://semver.org/spec/v2.0.0.html)을 지향합니다.

## [Unreleased]

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
- `rtrt-memory`는 아직 임베딩 없음. `embeddings`/`edges` 테이블은 v0.2 예약.
- `install.sh` / `install.ps1`은 README에 명시되어 있으나 트리에 아직 없음.
