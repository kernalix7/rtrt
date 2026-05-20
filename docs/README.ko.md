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
rtrt proxy "git status" < git-status-output
rtrt templates
rtrt new rust-cli ./hello --var project_name=hello
rtrt-dashboard
rtrt-mcp
```

전체 명령은 [USAGE.ko.md](USAGE.ko.md)에 있습니다.

## 핵심 기능

- **출력 압축** — `lite`/`full`/`ultra` 단계, 코드 블록 보호.
- **명령 출력 필터링** — `git`/`cargo` 등 빌트인 필터, MCP 도구로도 노출.
- **영구 메모리** — SQLite + FTS5 기반 BM25 회수, 벡터/그래프는 v0.2 예정.
- **멀티 프로바이더 라우팅** — Anthropic / OpenAI / OpenAI 호환(Ollama, llama.cpp 등).
- **프로젝트 스캐폴드** — 빌트인 6종 + `~/.rtrt/templates/`의 커스텀 템플릿.
- **MCP + 대시보드** — `rtrt-mcp`(stdio 예정), axum 기반 `rtrt-dashboard`.

## 문서

| 문서 | 내용 |
|------|------|
| [INSTALL.ko.md](INSTALL.ko.md) | 설치 경로 — 소스 / crates.io(예정) / 바이너리(예정) |
| [USAGE.ko.md](USAGE.ko.md) | CLI / MCP / 대시보드 사용법 |
| [FEATURES.ko.md](FEATURES.ko.md) | 압축 규칙 / 필터 / 메모리 스키마 / 템플릿 |
| [ARCHITECTURE.ko.md](ARCHITECTURE.ko.md) | 크레이트 경계 · 데이터 흐름 |
| [COMPARISON.ko.md](COMPARISON.ko.md) | 참조 프로젝트들과의 비교 |
| [CHANGELOG.ko.md](CHANGELOG.ko.md) | 전체 변경 이력 |
| [CONTRIBUTING.ko.md](CONTRIBUTING.ko.md) | 개발 환경 설정과 워크플로우 |
| [SECURITY.ko.md](SECURITY.ko.md) | 보안 신고 절차 |

## 로드맵

- [x] 워크스페이스 스캐폴드 (9개 크레이트)
- [x] `rtrt-compress` 규칙 엔진
- [x] `rtrt-proxy` git / cargo 필터
- [x] `rtrt-memory` SQLite + FTS5 BM25 회수
- [x] `rtrt-templates` 빌트인 6종 + 커스텀 로더
- [x] `rtrt-dashboard` 최소 axum UI
- [ ] `rtrt-compress` 벤치 하니스
- [ ] `rtrt-memory` 벡터/그래프 · `all-MiniLM-L6-v2` 임베딩
- [ ] `rtrt-providers` Anthropic / OpenAI 실제 채팅 구현
- [ ] `rtrt-mcp` stdio 전송 계층 구현
- [ ] 원라이너 설치 스크립트(`install.sh` / `install.ps1`)
- [ ] Claude Code 플러그인 매니페스트

## 라이선스

[MIT](../LICENSE) — Kim DaeHyun (kernalix7@kodenet.io)
