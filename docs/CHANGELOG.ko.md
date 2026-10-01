# 변경 이력

[English](../CHANGELOG.md) | **한국어**

이 문서는 프로젝트의 주요 변경 사항을 모두 기록합니다.

형식은 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) 1.1.0을 따르고, 버전 규칙은 [Semantic Versioning](https://semver.org/spec/v2.0.0.html)을 지향합니다.

## [Unreleased]

## [0.2.0] - 2026-10-01

### Highlights

**RTRT 0.2.0은 기존 v1 통합과 함께 native OpenCode 2.0.20 통합을 추가합니다.**

- npm agent에 native `./server` plugin definition과 `./tui` prompt-footer statusline을 포함합니다. v1 이름 있는 `RtrtProvenance` root export와 legacy setup-managed statusline은 별도 경로로 유지하며 호환성을 보존합니다.
- Native hook은 RTRT tool provenance, dashboard lifecycle, 명시적 dashboard 열기, permission deny를 보존합니다. 외부 Claude CLI permission brokerage, per-call shell identity, provider-limit recovery는 2.0.20 public API에 해당 표면이 없어 지원되지 않습니다.

### 수정

- Dashboard `/api/*`의 Origin 검증은 기존처럼 없는 헤더로 처리하지 않고, 존재하지만 잘못되었거나 반복된 헤더를 거부합니다. 의도적으로 Origin이 없는 bearer client는 유지하며 bootstrap은 계속 허용된 Origin을 요구합니다.
- npm 게시 staging에서 저장소 전용 개발 scripts를 제외하고 runtime 파일, 실행 권한, exports, exact-version 플랫폼 의존성을 보존합니다.

### 배포

- 완전하고 변경되지 않은 `option-ext@0.2.0` MPL covered source archive를 고정 SHA-256 및 로컬 추출 안내와 함께 포함합니다. 기존 targeted license notice는 유지하며 법적 인증이나 전체 의존성 inventory를 의미하지 않습니다.
- 기존 `0.1.7` 태그와 게시 패키지는 그대로입니다. 이번 릴리즈는 Docker 이미지를 게시하거나 운영 collector, sandbox, 전역 설정을 활성화하지 않습니다.

## [0.1.7] - 2026-09-28

### Highlights

**RTRT 0.1.7은 부분 게시된 v0.1.6과 별도 릴리스를 준비하며 npm 레지스트리 반영 대기 시간을 제한된 범위에서 늘립니다.**

- `npm publish` 뒤 플랫폼 검증은 이제 10초 간격으로 최대 60회 레지스트리를 조회하고 패키지 이름·버전·SHA-512 SRI가 일치할 때만 성공합니다. 불일치는 계속 거부하며, 게시 후 대기 시간이 초과됐다고 이미 접수된 버전을 무작정 재빌드하지 않습니다.
- `rtrt-agent`의 가시성 검사에도 같은 제한 시간을 적용합니다. 보호된 `npm-publish` 환경은 태그 참조만 허용하므로 `main`에서 dispatch한 `REL-` 게시 복구는 빌드 전에 거부합니다.

## [0.1.6] - 2026-09-28

### Highlights

**RTRT 0.1.6은 v0.1.5 감사에서 확인된 문제를 pair별 collector 인증, 서드파티 고지 포함, 모바일 프로젝트 선택, CI 검증 확대로 보완합니다.**

- `rtrt collector serve`는 이제 per-map `--credentials` 파일을 통해 각 `(guest_id, project)` 매핑을 자체 bearer credential에 묶고, `forward flush` 경로에 대응되는 `--guest-id` / `--project` 필터를 추가해 한 token이 무관한 host store에 더는 쓸 수 없게 합니다. 단일 `--token` 형식은 매핑이 정확히 하나일 때만 유지됩니다.
- 대시보드 모바일 레이아웃은 `≤720px`에서 전체 sidebar를 숨기는 대신 project picker를 계속 보여주며, 폭이 넓은 overview 콘텐츠는 페이지 대신 카드 내부에서 스크롤됩니다.
- 새 CI 잡은 옵트인 feature 표면(`embeddings`, `onnx`, `bertscore`, `chains`)을 `cargo check --locked` 깊이로 점검해 모델 다운로드가 허용되지 않는 상황에서도 미래의 빌드 회귀를 잡습니다. 테스트 매트릭스에 `macos-15-intel` 러너가 추가됩니다.
- 새 `THIRD_PARTY_NOTICES/` 디렉터리에 대시보드 번들에서 누락된 MIT 고지와 일부 Rust 의존성의 버전 고정 고지를 포함합니다. `THIRD_PARTY_LICENSES.md`도 해석된 버전에 맞게 정정했지만 법적 의무의 완전한 충족을 인증하지는 않습니다.
- Live-key, WSL, macOS x64 커버리지를 정직하게 명시합니다. live 프로바이더 키는 여전히 사전 태그 전용 `scripts/smoke.sh` 단계이며, WSL은 전용 CI lane이 없고, `macos-15-intel`은 `macos-latest` arm64 전용 실행에 의존하지 않고 이 릴리스에서 매트릭스에 합류합니다.

### 추가

- `rtrt collector serve`는 한 bearer token을 한 `(guest_id, project)` 매핑과 짝지우는 `--credentials <TOML>` 파일을 받으며, 여러 매핑에서 공유 bearer를 거부합니다. 파일은 운영자 소유의 mode `0600`이어야 하고 경로의 심볼릭 링크를 검사하며, 열린 파일의 신원·소유자·mode를 다시 확인합니다. 비-Unix 타깃에서는 이러한 검사의 의미가 이식되지 않아 credential 파일을 거부합니다. `--token`은 단일 매핑에만 허용하고 두 인증 출처를 결합할 수 없습니다.
- 인증 미들웨어는 모든 binding에 대해 constant-time bearer를 검증하고 token 없이 `AuthorizedBinding { pair, identity }`를 요청 extension에 넣습니다. Ingest handler는 `guest_id` 또는 `project`가 다르면 저장 전에 거부하므로 한 pair의 token으로 다른 매핑을 승인할 수 없습니다. 바이너리 통합 테스트는 A의 token이 B에 대해 `403`을 받고 B의 SQLite store를 만들지 않는지 확인합니다.
- `rtrt forward flush --guest-id <id> --project <name>`은 delivery 전에 spool을 한 쌍으로 필터링해 guest가 자신의 queue만 비울 수 있게 합니다. 새 `Selection` enum이 기존 `pending(limit, force)` helper를 대체합니다. 필터링 selection은 전체 rowid 스트림을 스캔하지만 due row의 전달 가능 배치를 100개로 제한해 foreign row가 매칭 row를 starvation하지 못하게 하고, 잘못되었거나 key와 불일치한 payload에 대해서는 spool 검증이 queue를 변경하지 않고 fail closed를 유지합니다. 필터 없는 flush는 여전히 mixed-pair spool을 거부하는데, 어떤 단일 token도 양쪽 host store에 대한 delivery를 인증할 수 없기 때문입니다.
- `THIRD_PARTY_NOTICES/INDEX.md`에는 임베드된 JavaScript와 `ring`, `subtle`, `webpki-roots`, `option-ext`를 포함한 일부 Rust 의존성의 버전별 소스 아카이브와 SHA-256이 기록됩니다. 다섯 바이너리 아카이브, 다섯 플랫폼 npm 패키지, `rtrt-agent`에 같은 고지 트리를 넣습니다. 이는 표적 인벤토리이며 법적 인증이나 MPL-2.0 소스 제공 의무 검토의 대체물이 아닙니다.

### 변경

- `MemoryStore::ingest_forwarded`와 wire shape(`WireEvent` / `ForwardedEvent`)는 그대로입니다. Collector의 authorization은 이제 ingest handler가 body를 읽기 전에 token을 특정 `(guest_id, project)` pair에 묶어, 기존 `(source_guest, source_project, event_id)` idempotency 보장은 대체가 아닌 강화로 유지됩니다.
- `THIRD_PARTY_LICENSES.md`는 이제 resolved 버전을 정확히 기록합니다. `ring@0.17.14`는 번들된 BoringSSL 분할을 가진 ISC/Apache-2.0으로, `subtle@2.6.1`은 BSD-3-Clause로, `webpki-roots@0.26.11`과 `webpki-roots@1.0.7`은 CDLA-Permissive-2.0으로, `option-ext@0.2.0`은 MPL-2.0으로 표기됩니다. v0.1.5 기록의 webpki-roots 오기 MPL-2.0 항목과 ring license 혼합은 제거됩니다.
- 대시보드 모바일 레이아웃은 project picker를 계속 노출합니다. sidebar의 `mode-nav` 그룹은 `≤720px`에서 숨겨지지만 `project-picker` 행은 그대로 보이고 main column의 min-width가 `minmax(0, 1fr)`로 내려가 긴 slug가 가로 스크롤바를 강제하지 않으며, overview 카드와 savings hero는 `overflow-wrap: anywhere`를 받아 긴 셀이 잘리지 않고 줄바꿈됩니다.
- 한·영 설치 문서에 v0.1.6 고정 예시와 릴리스 아카이브 이름을 명시합니다. 소스 트리의 Homebrew formula는 실제 checksum을 채우고 별도 tap 변경을 게시하기 전까지 템플릿입니다.

### 수정

- v0.1.5 감사에서 시연한 collector 공유 token 위조 경로를 빌드된 CLI 통합 경로에서 차단합니다. `crates/rtrt-cli/tests/collector_auth.rs`는 두 독립 project와 고유 token으로 실제 collector 바이너리를 실행해 A의 token으로 B에 쓰면 B store 생성 없이 `403`이 반환되고, A의 flush가 B의 queue를 변경하지 않는지 검증합니다.
- 대시보드는 이제 `720px` 이하에서 유일한 project selector를 더 이상 숨기지 않습니다. `.project-picker` 규칙은 작은 viewport에서 하단 경계를 제거하고 `.savings-hero`와 overview 카드는 긴 토큰을 제한해, 모바일 세션은 창 크기를 조정하지 않고도 선택된 프로젝트를 바꿀 수 있습니다.
- OpenCode CI 플러그인의 `npm test` 잡은 이제 `notices-package.test.mjs`를 포함합니다. 이 테스트는 `THIRD_PARTY_NOTICES/INDEX.md`의 모든 항목이 packed `rtrt-agent` tarball에 존재하고 각 `LICENSE`가 INDEX에 기록된 SHA-256과 일치하는지 단언해, notice를 드롭하거나 수정하는 미래의 bump가 게시 전에 CI에서 실패하게 만듭니다.

### CI

- 새 `feature-lanes` 잡은 `ORT_SKIP_DOWNLOAD=1`로 `cargo check --locked -p rtrt-memory --features embeddings`, `cargo check --locked -p rtrt-compress --features onnx`, `cargo check --locked -p rtrt-eval --features bertscore`, `cargo check --locked -p rtrt-templates --features chains`를 실행합니다. 각 lane은 compile-only이므로 ONNX 모델 다운로드나 live 프로바이더 호출이 필요 없습니다. 이 잡은 모든 옵트인 feature가 모든 기본 바이너리로 컴파일된다고 주장하지 않습니다 — v0.1.5 audit record가 이미 그 광범위한 주장을 정정한 바 있습니다 — 회귀가 잡힐 수 있도록 feature 당 하나의 한정된 compile lane을 추가합니다.
- 테스트 매트릭스는 기존 `ubuntu-latest` x64/arm64, `macos-latest` arm64, `windows-latest`, `beta`-toolchain Linux x64 lane과 함께 `macos-15-intel` 러너 항목을 새로 받습니다. macOS x64는 이전에 빌드만 되고 CI에서 테스트되지는 않았는데, 이번 릴리스는 arm64 실행에 의존하지 않고 매트릭스에 추가합니다.
- Live Anthropic / OpenAI / OpenAI 호환 검사는 CI lane이 아닌 `scripts/smoke.sh` 사전 태그 게이트로 남아 있습니다 — CI 환경에 프로바이더 키가 없기 때문입니다. WSL 런타임 또한 전용 CI lane이 없습니다. 기존 러너 레이블에 깔끔하게 맞지 않기 때문입니다. 두 한계는 묵인하지 않고 audit follow-up에 문서화됩니다.

### Notes

- 실제 프로바이더 키가 필요한 smoke는 CI에 포함되지 않고, WSL 런타임도 전용 CI 잡이 없습니다. 추가된 옵트인 feature 잡은 모델 다운로드 없이 컴파일하며 macOS Intel 테스트가 매트릭스에 포함됩니다.
- Homebrew formula의 SHA-256은 여전히 0으로 된 템플릿이므로 설치 가능한 tap 릴리스가 아닙니다. 고지 트리는 법률 준수를 인증하지 않으며 고지 배치와 MPL-2.0 소스 제공 의무는 적격 법률 자문의 검토 대상입니다.

### 게시 결과 (사실 부속기록)

- `v0.1.6`와 `REL-v0.1.6` 쌍 태그는 둘 다 커밋 `31bdd0f`를 가리키며 `origin`에 그대로 남아 있습니다. 태그가 옮겨지지 않았고 SHA도 변경되지 않았습니다.
- GitHub Actions run `36374328637`의 두 번째 시도는 불변 Windows SRI 계산에서 실패했고, `main` 브랜치 `workflow_dispatch` 복구 run `36383130874`는 `npm-publish` 환경의 tag-only 정책에 의해 거부되었습니다.
- `rtrt-dashboard-<platform>` npm 패키지 5개 모두 `0.1.6` 버전으로 게시가 성공했습니다 (각각 npm 레지스트리 `200`). `rtrt-agent@0.1.6`와 `v0.1.6`의 GitHub Release는 게시되지 않았습니다.
- 불변 태그나 이미 게시된 `0.1.6` 플랫폼 패키지 버전을 옮기는 방식으로 부분 상태를 복구할 수 없습니다. `rtrt-dashboard-*@0.1.6` 패키지 5개는 완성된 릴리스가 아니며 `rtrt-agent@0.1.6`은 npm 레지스트리에 존재하지 않습니다.

## [0.1.5] - 2026-09-26

### Highlights

**RTRT 0.1.5는 컨테이너와 원격 guest가 host 소유 project에 명시적인 memory event를 단방향 · bearer-authenticated ingest loop으로 쓸 수 있게 합니다. Host는 `rtrt collector serve`를 명시적인 `(guest_id, project)` map과 함께 실행하고, guest는 `rtrt forward enqueue`로 event를 private durable spool에 적재한 뒤 `rtrt forward flush`가 host collector가 정확히 한 번 저장할 때까지 bounded exponential backoff로 재시도합니다.**

- `rtrt collector serve`는 container 또는 원격 guest의 memory event를 host project로 통합하는 authenticated `POST /v1/events` endpoint입니다. Collector는 모든 browser `Origin` 요청을 request body를 읽지 않고 거부하고, 없는 bearer와 잘못된 bearer도 request body를 읽지 않고 거부하며, 각 request body를 1 MiB로 제한하고, host가 제어하는 project path에서만 destination을 파생하므로 guest는 자기 target을 선택할 수 없습니다.
- `rtrt forward enqueue|flush`는 모든 event를 delivery 시도 전에 private durable SQLite spool에 기록합니다. Spool은 crash에서도 살아남고 bounded exponential backoff로 재시도하며 host collector가 acknowledge한 경우에만 event를 비우므로 retry가 host memory row를 중복 생성하지 않습니다.
- Host collector는 새 `MemoryStore::ingest_forwarded` idempotent forward-ingest 경로로 각 `(guest, project, event id)` delivery를 정확히 한 번만 저장하며 이 경로는 schema v9 위에 올라갑니다. 이미 받은 tuple의 redelivery는 쓰지 않고 acknowledge합니다. 같은 tuple이 다른 host project에 오는 것은 별개의 delivery이며 거기서 row를 삽입합니다.
- Guest spool의 경로 검증과 Unix 소유권 검사는 지원되는 Unix 환경에 적용되며, Windows에서도 forward spool은 동작하지만 Unix 전용 소유권·mode 검사는 적용되지 않습니다. Unix에서는 직속 spool directory가 operator-owned + mode `0700`, spool file이 operator-owned + mode `0600`이어야 합니다. Symlink된 경로 구성 요소는 해당 경로를 통해 쓰기 전에 거부됩니다.
- CI는 이제 OpenCode 테스트 의존성을 `npm ci` 실행 전에 `.rtrt/tmp/npm-test-dependencies` 아래에 stage하므로 optional platform package나 다른 통제되지 않은 transitive dependency가 OpenCode plugin의 lockfile-gated test job을 망가뜨리지 않습니다.

### 추가

- `rtrt collector serve`는 container 또는 원격 guest의 memory event를 명시적으로 매핑한 host project로 통합하는 authenticated `POST /v1/events` endpoint입니다. Authorization middleware는 모든 `Origin` 요청, 없는 bearer, 잘못된 bearer를 `FORBIDDEN` / `UNAUTHORIZED`로 거부하며 그렇게 할 때 body를 절대 buffer하지 않고, 받아들인 각 request는 1 MiB로 cap된 뒤 JSON으로 파싱됩니다. 모든 destination은 host가 제어하는 project path에서 파생되므로 guest가 보낸 path는 받지 않으며 retry는 stable event id로 dedup됩니다. Server는 `127.0.0.1:7313`이 기본이고 모든 request에 `RTRT_COLLECTOR_TOKEN`이 필요합니다. host-local network 밖으로 나가는 traffic은 reverse proxy에서 TLS를 종료해야 하며(collector는 TLS를 직접 구현하지 않음) 가능하면 private bridge address에 bind합니다.
- `rtrt forward enqueue`는 명시적인 memory event를 `~/.rtrt/forward-spool.sqlite`에 적재하고 `rtrt forward flush`는 due event를 bounded exponential backoff로 재시도합니다. 각 event는 stable id를 가지며 bearer-authenticated POST로 delivery되고 host collector의 유효한 acknowledgement가 확인된 경우에만 queue에서 제거됩니다. 실패한 delivery는 queue에 남고 host memory row를 중복 생성하지 않고 다시 시도할 수 있습니다.

### 변경

- `MemoryStore::ingest_forwarded`가 이제 host의 유일한 forward-ingest 경로입니다. `(source_guest, source_project, event_id)`로 idempotency를 키잉하고 memory row, FTS5 mirror, merged metadata(발신자 키에 더해 `source_guest` / `source_project` provenance), `session_id` / `body_sha`, `forwarded_event_receipts` row를 단일 SQLite transaction에서 commit하므로 crash가 receipt 없는 memory 또는 memory 없는 receipt를 남길 수 없습니다. Redelivery는 원본 `memory_id`와 `inserted = false`를 반환합니다. 같은 `event_id`를 가진 fresh tuple이라도 다른 guest 또는 remote project에서 오면 별개의 delivery로 보고 그 row에 삽입합니다. Wire shape(`WireEvent` / `ForwardedEvent`)은 그대로입니다.

### 수정

- Guest forward spool이 지원되는 모든 Unix platform에서 경로 검증과 소유권 검사를 균일하게 적용합니다. Linux와 Android에서는 `/proc/self/status`를, 다른 Unix platform에서는 `id -u`로 effective UID를 읽습니다(Windows에서도 spool은 동작하지만 Unix 소유권·mode 검사는 적용되지 않습니다). 직속 spool directory는 operator-owned + mode `0700`, spool file은 operator-owned + mode `0600`이어야 하며 symlink된 경로 구성 요소를 거부합니다. 직속 spool directory가 없으면 `open_spool()`의 `private_parent()`가 먼저 mode `0700`으로 생성할 수 있고, 그 후에 파일 경로의 symlink 여부를 검사합니다. 이 코드는 symlink 대상에 쓰지 않습니다.

### CI

- OpenCode plugin CI는 `.rtrt/tmp/npm-test-dependencies` 아래에 locked test dependency를 stage한 뒤 `npm ci --ignore-scripts`를 실행하므로, optional platform package나 다른 통제되지 않은 transitive dependency가 OpenCode plugin의 lockfile-gated test job을 망가뜨리지 않습니다. Stage된 `node_modules`는 test working directory로 옮겨지며 기존 fixture 동작은 그대로입니다.

## [0.1.4] - 2026-09-21

### Highlights

**RTRT 0.1.4는 OpenCode session migration의 loop를 닫습니다. `rtrt opencode sessions backup`이 이 사용자가 소유한 모든 session store를 secret 없이 snapshot하고, `--source`로 어떤 snapshot이든 기존 migration 경로로 복원합니다.**

- `rtrt opencode sessions backup`은 `global.sqlite`, `projects/<slug>.sqlite`, session content가 없는 `manifest.json`을 하나의 private `0700` root에 쓰며 기존 경로, traversal, symlink output을 거부합니다.
- Snapshot은 read-only handle에서 SQLite online backup API로 뜨므로 실행 중인 store를 수정하지 않고 진행 중인 WAL 내용도 하나의 commit된 snapshot으로 담깁니다.
- Credential table은 schema를 유지한 채 row를 모두 잃습니다. 복사된 trigger와 view는 scrub 전에 fail closed하며, 각 snapshot은 WAL에서 분리 후 vacuum하므로 해제된 secret page가 남지 않습니다.
- `status`, `dry-run`, `apply`가 `--source <db>`를 받아 session migration이 더 이상 단방향이 아닙니다.

### 추가

- `rtrt opencode sessions backup`은 global OpenCode store와 RTRT-private project store를 모두 하나의 private backup root(`global.sqlite`, `projects/<slug>.sqlite`, `manifest.json`)로 snapshot합니다. Snapshot은 read-only handle에서 SQLite online backup API로 뜨므로 WAL 내용이 하나의 commit된 snapshot으로 담기고 실행 중인 store를 수정하지 않습니다. 민감 table은 schema를 유지한 채 row를 모두 잃으며 이는 migration이 이미 적용하는 제외 집합과 같고, 각 snapshot은 WAL에서 분리 후 vacuum하므로 해제된 credential page가 남지 않습니다. Root는 mode `0700`, file은 `0600`이며 기존 경로는 병합하지 않고 거부하고, manifest는 완결성 표식으로 마지막에 씁니다.
- `rtrt opencode sessions status|dry-run|apply`가 `--source <db>`를 받습니다. 기존 migration 경로로 backup을 복원할 수 있어 session migration이 더 이상 단방향이 아닙니다.

### 수정

- Session migration 문서를 정확히 고쳤습니다. 원본을 "backup으로 유지"하는 것이 아니라 원본을 수정하지 않습니다.

## [0.1.3] - 2026-09-16

### Highlights

**RTRT 0.1.3은 `rtrt-agent`에서 native dashboard를 자동으로 사용할 수 있게 하면서 브라우저 실행은 명시적이고 로컬이며 credential-safe하게 유지합니다.**

- OpenCode 플러그인을 로드하면 `PATH`, 미리 설치된 RTRT binary 또는 install script 없이 버전이 일치하는 loopback-only dashboard backend 하나를 시작합니다.
- Native dashboard 실행 파일은 정확한 버전의 npm 플랫폼 패키지 5개로 제공하며 릴리스 워크플로가 `rtrt-agent`보다 먼저 게시하고 검증합니다.
- 기존 `~/.rtrt` 데이터와 유효한 private credential은 보존하며 안전하지 않은 소유권, mode, symlink, 외부 listener는 거부합니다.

### 추가

- OpenCode가 플러그인을 로드하면 `rtrt-agent`가 버전이 일치하는 `rtrt-dashboard` backend를 loopback-only 사용자별 detached process로 예약합니다. install script, `PATH` 또는 미리 설치된 RTRT binary 없이 정확한 버전의 optional npm 플랫폼 패키지 5개가 native 실행 파일을 제공합니다.
- 브라우저는 `rtrt-dashboard-open` 또는 인자가 없는 `rtrt_dashboard_open` 도구로 명시적으로만 엽니다. 두 경로 모두 기존 60초 HMAC bootstrap fragment를 사용하며 credential을 prompt나 command template에 넣지 않습니다.

### 변경

- 대시보드 시작은 fail-soft이며 정상 singleton을 재사용하고 유효한 private credential과 기존 `~/.rtrt` 데이터를 모두 보존합니다. 안전하지 않은 소유권, mode, symlink 또는 외부 listener는 거부합니다.
- 쌍 태그 릴리스 워크플로는 대시보드 플랫폼 패키지 5개를 모두 게시·검증한 뒤 `rtrt-agent`를 게시하며 GitHub Release는 마지막 단계로 유지합니다.

### 수정

- `rustls`를 `0.23.45`로 갱신해 `RUSTSEC-2026-0285`를 해결하고 yanked `chacha20 0.10.0` lock entry를 `0.10.2`로 교체했습니다.

## [0.1.2] - 2026-09-14

### Highlights

**RTRT 0.1.2는 강화 릴리스입니다. OpenCode 스테이터스라인이 키보드 입력을 막지 않고, Unix에서 프록시 통계가 비공개로 유지되며, 릴리스 계약은 trusted publishing 아래 `rtrt-agent@0.1.2`를 고정합니다.**

- OpenCode 스테이터스라인은 범위가 지정된 수명주기·상태 이벤트 때만 세션 사용량을 계산하므로, 오래 실행한 로컬 세션에서 스테이터스라인 렌더 루프가 키보드 입력을 빼앗지 않습니다.
- Unix에서 프록시 통계 저장소를 비공개로 생성합니다(`~/.rtrt`는 `0700`, `proxy-stats.sqlite`와 사이드카는 `0600`). 이전 권한은 복구하고 안전하지 않은 경로는 거부합니다.
- OpenCode setup은 정확한 `rtrt-agent@0.1.2` 패키지를 등록하며, 쌍 태그 릴리스 계약과 사전 점검을 npm trusted publishing 기준으로 강화했습니다.

### 변경

- OpenCode setup은 이제 단수 루트 `plugin` 키로 정확한 `rtrt-agent@0.1.2` npm 패키지를 등록합니다. 기존 `0.1.1` 등록은 다음 `rtrt setup --agent opencode --apply` 실행 시 다시 씁니다.
- 쌍 태그 릴리스 계약을 강화했습니다. `vX.Y.Z`와 `REL-vX.Y.Z` run은 빌드 전에 태그, 워크스페이스 버전, `rtrt-agent` 패키지 버전, 플러그인 메타데이터, 변경 이력 섹션이 모두 일치하는지 검증하며, 이 사전 점검이 실패하면 npm trusted publishing 단계는 실행을 거부합니다.

### 수정

- OpenCode 스테이터스라인이 빈도가 높은 파일·메시지 조각 이벤트를 구독하거나, Solid 렌더 계산에서 스트리밍 메시지 상태를 추적하거나, 세션 범위가 없는 이벤트를 마운트된 모든 세션에 브로드캐스트하거나, prompt-right 입력 경로 안에서 렌더링하지 않도록 수정했습니다. 세션 사용량은 application-bottom surface에서 범위가 지정된 수명주기·상태 이벤트 때만 스냅샷으로 계산하므로, 오래 실행한 로컬 세션에서 스테이터스라인 렌더 루프가 키보드 입력 처리를 막지 않습니다.
- Unix에서 프록시 통계 저장소를 비공개로 유지하도록 수정했습니다. 기본 `~/.rtrt` 디렉터리는 `0700`으로 생성하고, `proxy-stats.sqlite`와 기존 `-wal`, `-shm`, `-journal` 사이드카 파일은 `0600`으로 유지합니다. 소유자가 본인이지만 느슨한 이전 권한이 남아 있는 파일은 다음 쓰기 가능한 통계 접근 시 복구하며, 심볼릭 링크, 일반 파일이 아닌 경로, 다른 사용자가 소유한 경로는 거부합니다. `RTRT_PROXY_STATS_PATH`를 명시적으로 지정한 경우 해당 상위 디렉터리의 권한은 그대로 둡니다.

## [0.1.1] - 2026-09-10

### Highlights

**RTRT 0.1.1은 로컬 우선 툴킷 릴리스를 완성하며, 멀티 에이전트 오케스트레이션은 호스트 런타임이 담당합니다.**

- 제품 바이너리 3개, 옵트인 `rtrt-eval`, MCP 도구 23개, 빌트인 템플릿 `dev`, `design`, `plan`, `standardization` 4종을 갖춘 11개 크레이트 워크스페이스입니다.
- OpenCode 통합은 정확한 `rtrt-agent@0.1.1` npm 패키지를 등록하며 OMO와 호환됩니다. `rtrt-agent`는 trusted publishing으로 npm에 게시되며 워크스페이스 크레이트는 소스 전용입니다.
- 쌍 태그 릴리스 자동화: `vX.Y.Z` 태그 run은 Rust 바이너리를 검증·빌드해 Actions artifact로만 게시하고, `REL-vX.Y.Z` 태그 run은 다시 빌드한 뒤 trusted publishing으로 `rtrt-agent`를 npm에 게시하고 `vX.Y.Z` 아래에 GitHub Release를 생성/갱신해 플랫폼별 바이너리 아카이브 5개와 체크섬을 첨부합니다. source archive는 GitHub이 `vX.Y.Z` 태그에서 자동 생성합니다.
- Setup 강화로 관리 OpenCode 상태에 엄격한 사전 점검, 심볼릭 링크 보호, 원자적 쓰기를 적용합니다.

### 추가

- 단수 루트 `plugin` 키와 정확한 `rtrt-agent@0.1.1` 등록을 사용하는 OpenCode npm 플러그인 통합을 추가했습니다. Setup 관리 스테이터스라인은 npm 패키지에 포함되지 않습니다.
- 프로젝트 전용 OpenCode 세션 마이그레이션과 launcher, setup 소유 Linux shell sandbox를 추가했습니다.

### 변경

- 프로바이더 라우팅과 페일오버는 툴킷 기능으로 유지합니다. 네이티브 오케스트레이션, team, scheduler, roster, `team_dispatch`는 제거하고 호스트 런타임에 위임합니다.
- 대시보드 Orchestration 페이지를 Failover로 변경했습니다. 경로는 `/failover`, 백엔드는 `/api/failover/config`이며, `/orchestration`은 overview로 이동하고 `assets/js/orchestration.js`는 더 이상 존재하지 않습니다.
- 프로젝트를 선택하지 않아도 페일오버 정책을 편집할 수 있습니다. 셀렉터가 없으면 전역 `[failover]` 정책을 읽고 쓰며, 선택한 프로젝트는 이를 읽기 전용으로 상속합니다. **Custom**은 `<repo>/.rtrt/config.toml`에 기록하고, **Follow global**은 그 override만 제거합니다.
- OpenCode setup은 Rules, TUI, MCP를 쓰기 전에 엄격한 legacy 정리를 검증하고, 마지막에 인식 가능한 항목을 정리합니다. 그 이전 단계가 실패하면 legacy runtime을 보존합니다.
- 통합 릴리스 워크플로는 쌍 태그 기반으로 하나의 versioned release에서 릴리스를 자동화합니다. `vX.Y.Z` 태그 run은 `release.yml`의 validate-and-build 작업을 트리거해 플랫폼별 Rust 바이너리를 만들어 Actions artifact로만 게시합니다. `REL-vX.Y.Z` 태그 run은 빌드를 다시 돌리고 npm publish 작업(trusted publishing으로 `rtrt-agent`를 npm에 게시)을 실행한 뒤 `vX.Y.Z` 아래에 GitHub Release를 생성/갱신하고 플랫폼별 바이너리 아카이브 5개와 체크섬을 첨부합니다. source archive는 GitHub이 `vX.Y.Z` 태그에서 자동 생성합니다. 워크스페이스 크레이트는 crates.io에 게시하지 않습니다.

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
