# 비교

[English](COMPARISON.md) | **한국어**

RTRT는 기존의 여러 토큰 절감 기법을 하나의 러스트 툴킷에 모았습니다. 각 참조 프로젝트와의 차이를 정리합니다.

## RTRT vs caveman (JuliusBrussee/caveman)

| | caveman | RTRT (`rtrt-compress`) |
|---|---|---|
| 언어 | JavaScript + Python | 러스트 |
| 배포 | Claude Code 스킬 | Cargo 크레이트 + CLI 서브커맨드 + MCP 도구 |
| 레벨 | `lite`, `full`, `ultra`, `wenyan*` | `lite`, `full`, `ultra` (`wenyan`은 예정) |
| 출력 감소 | 평균 약 65% | 동등 목표; 벤치 하니스 예정 |
| 규칙 엔진 | 마크다운 스킬 지시 | 정규식 + 보호(코드/URL/인용) 파이프라인 |
| MCP 통합 | `caveman-shrink` 미들웨어 | `rtrt-mcp`의 일급 도구 |
| 풋프린트 | Node.js ≥ 18 필요 | 단일 정적 바이너리 |

## RTRT vs agentmemory (rohitg00/agentmemory)

| | agentmemory | RTRT (`rtrt-memory`) |
|---|---|---|
| 언어 | Node.js + 자체 `iii-engine` | 러스트 |
| 저장소 | SQLite (iii-engine) | SQLite (`rusqlite::bundled`) |
| FTS | 내장 BM25 + 동의어 확장 | SQLite FTS5 BM25 ✅ (동의어 레이어는 예정) |
| 임베딩 | `all-MiniLM-L6-v2` 기본; Gemini/OpenAI/Voyage/Cohere 선택 | `all-MiniLM-L6-v2` via `fastembed` ✅ (`embeddings` 피처, 첫 다운로드 후 오프라인); `Embedder` 트레이트로 다른 백엔드 플러그블 |
| 그래프 | 지식 그래프 엔티티 매칭 | 스키마 예약(`edges`); 엔티티 매칭 예정 |
| 회수 | BM25 + 벡터 + 그래프 Reciprocal Rank Fusion | BM25 + 벡터 RRF ✅ (`recall_hybrid`), 그래프 예정 |
| LLM extract / compress | 클라우드 LLM 전용 (OpenAI / Anthropic) | 임의 `Provider`, 기존 OpenAI 호환 어댑터로 **로컬 Ollama** 서버 그대로 동작 — 새 HTTP 코드 0 (`llm` 피처, `extract_and_save` / `compress_project`). **agentmemory 대비 RTRT의 부가가치.** |
| 프로세스 모델 | `:3111` 공용 서버 | `rtrt-mcp` 안의 라이브러리/도구, 대시보드 관찰 |
| 에이전트 공유 | 모두 한 서버 공유 | 프로젝트별 SQLite, 공유는 옵트인 |

## RTRT vs rtk (rtk-ai/rtk)

| | rtk | RTRT (`rtrt-proxy`) |
|---|---|---|
| 언어 | 러스트 | 러스트 |
| 전략 | 명령별 규칙 + 자동 재작성 훅 | 명령별 규칙 + CLI 서브커맨드 |
| 범위 | 100+ 명령 | 4 명령(git status, git log, cargo build, cargo test) — 확장 중 |
| 훅 통합 | Claude Code `PreToolUse`가 `git status` → `rtk git status` 자동 변환 | 명시적 호출: `git status \| rtrt proxy "git status"`; 훅 헬퍼는 예정 |
| 토큰 절감 | 60–90% | 동등 목표; 벤치 하니스 예정 |
| 번들 | 단독 CLI | `rtrt` CLI + MCP 도구로도 노출 |

## RTRT vs codex-plugin-cc (openai/codex-plugin-cc)

| | codex-plugin-cc | RTRT (`rtrt-providers`) |
|---|---|---|
| 언어 | 타입스크립트 (Claude Code 플러그인) | 러스트 |
| 프로바이더 수 | 한 개(Codex/OpenAI 전용) | 다수(Anthropic, OpenAI, OpenAI 호환: Ollama, llama.cpp, vLLM, LM Studio 등) |
| 라우팅 | 로컬 Codex 설치에 위임 | 프로바이더 트레이트, 작업별 활성 선택 |
| 선택 방법 | Codex `config.toml` | RTRT 설정 + 요청별 오버라이드 |
| 멀티 프로바이더 목표 | 아니오 | 예 — 일급 기능 |

codex-plugin-cc는 RTRT의 멀티 프로바이더 아이디어에 영감을 주었지만, RTRT의 설계 범위는 codex-plugin-cc보다 넓고 소스를 가져오지 않습니다.

## RTRT vs 통합 가치

RTRT의 가치는 **하나의 툴킷, 하나의 바이너리, 하나의 설정**입니다.

- `rtrt` CLI 하나로 압축 / 필터 / 메모리 / 채팅 / 스캐폴드 전부 노출.
- `rtrt-mcp` MCP 서버 하나로 MCP 인지 에이전트에 동일 표면 노출.
- `rtrt-dashboard`로 토큰 절감 + 메모리 회수 + 템플릿 스캐폴드를 통합 시각화.

대신 v0.1.0 단계에서는 각 표면이 참조 프로젝트보다 좁습니다. 로드맵에서 확장합니다.
