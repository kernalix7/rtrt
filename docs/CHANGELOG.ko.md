# 변경 이력

[English](../CHANGELOG.md) | **한국어**

이 문서는 프로젝트의 주요 변경 사항을 모두 기록합니다.

형식은 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) 1.1.0을 따르고, 버전 규칙은 [Semantic Versioning](https://semver.org/spec/v2.0.0.html)을 지향합니다.

## [Unreleased]

## [0.1.1] - 2026-09-10

### Highlights

**RTRT 0.1.1은 로컬 우선 툴킷 릴리스를 완성하며, 멀티 에이전트 오케스트레이션은 호스트 런타임이 담당합니다.**

- 제품 바이너리 3개, 옵트인 `rtrt-eval`, MCP 도구 23개, 빌트인 템플릿 `dev`, `design`, `plan`, `standardization` 4종을 갖춘 11개 크레이트 워크스페이스입니다.
- OpenCode 통합은 정확한 `rtrt-agent@0.1.1` npm 패키지를 등록하며 OMO와 호환됩니다.
- 쌍 태그와 의존성 순서를 따르는 자동화가 하나의 버전 릴리스에서 Rust 크레이트와 npm 패키지를 릴리스합니다.
- Setup 강화로 관리 OpenCode 상태에 엄격한 사전 점검, 심볼릭 링크 보호, 원자적 쓰기를 적용합니다.

### 추가

- 단수 루트 `plugin` 키와 정확한 `rtrt-agent@0.1.1` 등록을 사용하는 OpenCode npm 플러그인 통합을 추가했습니다. Setup 관리 스테이터스라인은 npm 패키지에 포함되지 않습니다.
- 프로젝트 전용 OpenCode 세션 마이그레이션과 launcher, setup 소유 Linux shell sandbox를 추가했습니다.

### 변경

- 프로바이더 라우팅과 페일오버는 툴킷 기능으로 유지합니다. 네이티브 오케스트레이션, team, scheduler, roster, `team_dispatch`는 제거하고 호스트 런타임에 위임합니다.
- 대시보드 Orchestration 페이지를 Failover로 변경했습니다. 경로는 `/failover`, 백엔드는 `/api/failover/config`이며, `/orchestration`은 overview로 이동하고 `assets/js/orchestration.js`는 더 이상 존재하지 않습니다.
- 프로젝트를 선택하지 않아도 페일오버 정책을 편집할 수 있습니다. 셀렉터가 없으면 전역 `[failover]` 정책을 읽고 쓰며, 선택한 프로젝트는 이를 읽기 전용으로 상속합니다. **Custom**은 `<repo>/.rtrt/config.toml`에 기록하고, **Follow global**은 그 override만 제거합니다.
- OpenCode setup은 Rules, TUI, MCP를 쓰기 전에 엄격한 legacy 정리를 검증하고, 마지막에 인식 가능한 항목을 정리합니다. 그 이전 단계가 실패하면 legacy runtime을 보존합니다.
- 통합 릴리스 워크플로는 쌍 태그와 의존성 순서를 따라 하나의 versioned release에서 Rust/npm 릴리스를 자동화합니다. 각 크레이트는 패키징, 게시, 레지스트리 노출 확인을 마친 뒤에야 다음 의존 크레이트를 게시합니다.

### 수정

- Setup, provenance, permission, filesystem, HTTP MCP 경계를 강화하고 보안 수정을 위해 `h2`를 0.4.16으로 업데이트했습니다.
- 레거시 프로젝트 파일을 정리할 때 배타적 생성으로 백업하고, 대상이 바이트와 소유권이 그대로인 일반 파일인지 확인하며, 해당 파일을 변경하기 직전에 다시 검증합니다. 교체 내용은 같은 디렉터리의 임시 파일에 쓴 뒤 rename으로 옮기므로, 마지막 순간에 끼워 넣은 심볼릭 링크는 따라가지 않고 대체됩니다.
- `<repo>/.rtrt`를 생성 후 권한을 좁히는 대신 처음부터 `0700`으로 만듭니다. 뒤따르는 권한 변경이 다른 디렉터리로 유도될 수 있는 창을 없앴습니다.
- `--allowed-origins` 없이 실행한 `rtrt-mcp --transport http`는 이제 모든 origin을 허용하지 않고, `Origin` 헤더를 포함한 요청을 전부 거부합니다. `Origin`을 보내지 않는 네이티브 클라이언트는 영향이 없습니다.
- OpenCode 플러그인은 경로를 인자로 받는 shell 명령을 더 이상 자동 승인하지 않습니다. 권한 판정 시점과 shell 실행 시점의 경로 해석이 달라, 승인된 파일이 그 사이에 심볼릭 링크로 교체될 수 있기 때문입니다. `cat`, `ls`, `head`, `tail`, `wc`, `stat`은 일반 확인 절차로 돌아가며 `pwd`만 자동 승인됩니다.
- `rtrt-core`가 Windows에서 다시 빌드됩니다. 동일 파일 검사가 아직 불안정한 `volume_serial_number`/`file_index`를 사용해 nightly에서만 컴파일됐습니다. 이제 Windows에서는 안정 속성 전부를 비교하며, 이는 교체를 탐지하지만 진정한 파일 신원 확인이 아닌 변조 검사입니다.
- 대시보드 페일오버 편집기는 `transient_retries`와 `backoff_divisor`가 `u32::MAX`를 넘으면 조용히 잘라내지 않고 400으로 거부합니다. 정책 로드가 실패하면 폼을 비우고 잠가, 이전 프로젝트 값이 전역 정책으로 저장되는 일이 없습니다.
- Setup 멱등성, 프로젝트 전용 세션 migration과 catch-up, sandbox 검증, uninstall 순서, 외부 설정 보존을 개선했습니다.

<!--
새 버전 섹션을 만들 때 이 블록을 복사합니다.
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
