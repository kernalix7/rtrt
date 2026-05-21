# 성능

[English](PERF.md) | **한국어**

> "성능은 측정값으로만." — [`DESIGN.ko.md`](DESIGN.ko.md#6-성능은-측정값으로만)

이 문서는 약속한 SLO와 최근 측정값을 공개합니다. 10% 이상 회귀는 릴리스 차단.

## SLO (Service Level Objectives)

### CLI / 라이브러리 동작

| 동작 | 입력 | p50 목표 | p99 목표 | 비고 |
|------|------|---------|---------|------|
| `rtrt --help` | — | < 10 ms | < 20 ms | 콜드 스타트 |
| `rtrt compress -l ultra` | 4 KB | < 0.5 ms | < 1 ms | 룰 엔진 |
| `rtrt compress --ml --ratio 0.5` | 4 KB | < 1 ms | < 3 ms | 휴리스틱 (ONNX 미포함) |
| `rtrt memory save` | 1 KB | < 2 ms | < 5 ms | SQLite WAL |
| `rtrt memory recall` (BM25) | 1 K rows | < 5 ms | < 15 ms | FTS5 |
| `rtrt memory recall` (BM25) | 100 K rows | < 50 ms | < 150 ms | |
| `rtrt memory recall` (hybrid + HNSW) | 100 K rows | < 100 ms | < 250 ms | embeddings + hnsw |
| `rtrt signatures --lang rust` | 8 KB | < 5 ms | < 15 ms | tree-sitter |
| `rtrt repo-map` | 1 K Rust 파일 | < 3 s | < 8 s | I/O 바운드 |
| `rtrt-mcp` stdio 핸드셰이크 | — | < 30 ms | < 80 ms | |
| `rtrt-dashboard` 첫 페인트 | localhost | < 50 ms | < 120 ms | 인라인 HTML |

### 자동 캡처 파이프라인

쓰기 path (dedup + privacy + save + tag)는 가벼워야 함. 에이전트가 못 느낄 정도.

| 단계 | p99 목표 |
|------|---------|
| Dedup 윈도우 조회 | < 0.1 ms |
| Privacy 필터 (4 KB) | < 0.5 ms |
| SHA-256 (4 KB) | < 0.1 ms |
| SQLite save | < 5 ms |
| **End-to-end 자동 캡처** | **< 6 ms** |

옵션 LLM 압축은 백그라운드 tokio task. 응답 path는 항상 3단계 이후 반환.

### 리소스 캡

| 바이너리 | Idle RSS | Peak RSS |
|---------|----------|----------|
| `rtrt` (대부분 명령) | < 10 MB | < 50 MB |
| `rtrt-mcp` (idle) | < 15 MB | < 80 MB |
| `rtrt-dashboard` (idle) | < 20 MB | < 100 MB |

### 정확도 (장기 목표)

라벨링된 데이터셋 필요. 옵트인 `rtrt-eval` 크레이트가 손-튜닝 smoke fixture (`crates/rtrt-eval/fixtures/recall_smoke.json`) 동봉. 동일 스키마 외부 fixture 받음 — LongMemEval-S / Memorybench / 인하우스 코퍼스 드랍하면 실수치 측정.

| 작업 | 메트릭 | 목표 |
|------|--------|------|
| `compress` 의미 보존 | BERTScore F1 vs 원본 | > 0.85 (full) |
| `compress` 토큰 절감 | 평균 char 감소 | > 35% (full), > 55% (ultra) |
| `memory recall` (BM25) | R@5 (LongMemEval-S) | > 0.80 |
| `memory recall` (hybrid) | R@5 | > 0.92 |
| `memory recall` (hybrid) | MRR | > 0.78 |

## 최근 측정값

### `rtrt-memory` recall — 2026-05-21

환경: 노트북, Rust 1.85 stable, release 프로필, 인메모리 SQLite.

| 벤치 | 크기 | p50 | SLO 내? |
|------|------|-----|--------|
| `recall_bm25` | 1 K | **32 µs** | ✅ (목표 5 ms) |
| `recall_bm25` | 10 K | **69 µs** | ✅ (목표 50 ms) |
| `recall_bm25` | 100 K | **443 µs** | ✅ (목표 50 ms) |
| `recent_paged` (limit=50) | 1 K | **29 µs** | ✅ (v5 인덱스 적용 후) |
| `recent_paged` (limit=50) | 10 K | **30 µs** | ✅ (v5 인덱스 적용 후) |
| `recent_paged` (limit=50) | 100 K | **32 µs** | ✅ (v5 인덱스 적용 후, 이전 71 ms) |
| `save_one` | 1 K | **25 µs** | ✅ (목표 2 ms) |
| `save_one` | 10 K | **26 µs** | ✅ |
| `projects_listing` | 8 프로젝트 × 1 K | **629 µs** | ✅ |

**메모**

- `recall_bm25` 모든 크기에서 SLO 내 — FTS5 효율 확인.
- `recent_paged` 100 K가 명백한 미스였음 (71 ms). 스키마 v5가 `(project, created_at DESC, id DESC)` 커버링 인덱스를 추가, 쿼리는 이제 단일 seek + 순차 walk로 응답. p50 모든 크기에서 ~32 µs (100 K에서 2200× 가속).
- `save_one` 상수 시간 — WAL이 쓰기 흡수.

### `rtrt-compress` 벤치

`crates/rtrt-compress/benches/compress_bench.rs`. README의 "60%+ 절감" 주장은 여기서 픽스처 × 레벨로 측정. `rtrt benchmark`로 갱신.

### `rtrt-eval` 스모크 픽스처 — 2026-05-22

환경: 노트북, Rust 1.85 stable, debug 프로필, 인메모리 SQLite. `cargo run -p rtrt-eval -- recall` / `compress`로 갱신.

| 표면 | 메트릭 | 값 |
|------|--------|-----|
| `recall_bm25` (내장 `recall_smoke`, 12 docs, 7 queries) | R@5 | **0.857** |
| `recall_bm25` (동일 fixture) | MRR | **0.857** |
| `compress lite` (내장 `compress_smoke`) | 평균 ratio | **0.962** |
| `compress full` | 평균 ratio | **0.932** |
| `compress ultra` | 평균 ratio | **0.879** |

R@5 0.80 floor는 `rtrt_eval::tests::recall_at_5_on_smoke_fixture_clears_floor`로 강제. 스모크 fixture는 의도적으로 작음 — 실수치 게시는 진짜 라벨링 코퍼스로 교체 후.

## 재현

```bash
# 전체 criterion 스위트
cargo bench --workspace

# 메모리 recall 벤치만
cargo bench -p rtrt-memory --bench recall_bench

# 빠른 실행 (통계 분석 건너뛰기)
cargo bench -p rtrt-memory --bench recall_bench -- --quick

# rtrt CLI 단축
rtrt benchmark
rtrt benchmark --bench recall_bench --package rtrt-memory
```

Criterion이 `target/criterion/report/index.html`에 HTML 리포트, stdout에 텍스트 요약 출력. PR 설명에 둘 다 첨부.

## 회귀 정책

- `crates/rtrt-{compress,memory,proxy}/` 건드리는 PR은 관련 벤치 재실행 + 델타 PR 설명에 보고.
- 어느 p50이든 **10% 이상 회귀**는 머지 차단. 명시적 "성능 트레이드 문서화" 줄이 `CHANGELOG.md`에 있으면 예외.
- 릴리스 워크플로는 `cargo bench --workspace` 실행, 회귀 시 게시 거부.
