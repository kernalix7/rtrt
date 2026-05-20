# RTRT에 기여하기

[English](../CONTRIBUTING.md) | **한국어**

RTRT에 기여해 주셔서 감사합니다! 이 문서는 빠른 시작 가이드입니다.

## 사전 준비

- 러스트 stable 1.85+ (edition 2024). CI는 `stable`과 `beta`를 게이트합니다.
- `rusqlite` 번들 SQLite용 C 툴체인(`gcc` 또는 `clang`).
- 소스 관리용 `git`.

`rust-toolchain.toml`이 채널을 `stable`로 고정하고 `rustfmt`, `clippy`를 자동 설치하므로 `rustup`이 올바른 툴체인을 선택합니다.

## 빌드

```bash
git clone https://github.com/kernalix7/rtrt.git
cd rtrt
cargo build --workspace
```

## 테스트

```bash
# 전체 테스트
cargo test --workspace

# 린트
cargo clippy --workspace --all-targets -- -D warnings

# 포맷 체크
cargo fmt --all -- --check
```

## 워크플로우

1. 저장소를 **포크**합니다.
2. **기능 브랜치** 생성 (`git checkout -b feat/my-feature`).
3. **컨벤셔널 커밋** 규칙으로 변경사항을 작성합니다.
4. **풀 리퀘스트**를 보냅니다.

## PR 체크리스트

PR을 보내기 전에 확인하세요.

- [ ] `cargo test --workspace`: 전체 통과
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`: 경고 0
- [ ] `cargo fmt --all -- --check`: 포맷 일치
- [ ] 문서 업데이트(필요 시)
- [ ] 자격증명/시크릿/개인 정보가 코드에 들어가지 않음

## 커밋 컨벤션

[Conventional Commits](https://www.conventionalcommits.org/)를 따릅니다.

| 접두사 | 용도 |
|--------|------|
| `feat` | 새 기능 |
| `fix` | 버그 수정 |
| `docs` | 문서 |
| `refactor` | 리팩토링(기능 변화 없음) |
| `test` | 테스트 추가/수정 |
| `chore` | CI, 의존성 등 유지보수 |
| `perf` | 성능 개선 |

### 예시

```
feat(compress): wenyan 한문 규칙 팩 추가
fix(memory): FTS5 특수 문자 이스케이프
docs(architecture): 규칙 보호 파이프라인 설명 추가
refactor(providers): Anthropic / OpenAI 공통 헤더 정리
test(templates): python-uv 포스트-인스톨 훅 경로 커버
chore(ci): cargo-audit 0.21로 업그레이드
```

### AI 도구 공동 저자 금지

다음과 같은 `Co-authored-by:` 트레일러는 **추가하지 마세요**.

- `Co-authored-by: Claude <noreply@anthropic.com>` (Anthropic 이메일 일체)
- `Co-authored-by: Cursor <cursoragent@cursor.com>`
- `Co-authored-by: Copilot <...>` (GitHub Copilot 어떤 변형이든)
- `Co-authored-by: <기타 AI 도구/에이전트 신원>`

패치는 당신이 작성한 것이고, 인적 저작권은 당신에게 귀속됩니다. AI 도구가 얼마나 기여했든 본 저장소에서는 공동 저자 크레딧을 받지 않습니다. 트레일러가 실수로 들어가면 수정 요청을 드릴 것이며, 이미 병합된 PR의 경우 후속 PR로 히스토리 정리를 조율합니다.

함께 페어 프로그래밍한 사람과 같은 **사람 공동 저자**는 환영합니다 — 실제 사람 식별자와 이메일을 사용해 주세요.

## 릴리스 노트 작성

`CHANGELOG.md`(및 `docs/CHANGELOG.ko.md`)의 각 버전 섹션은 `### Highlights`로 시작합니다 — 한 줄 헤드라인 + 스캔하기 좋은 3~6 불릿. 이 부분이 GitHub 릴리스 페이지 최상단에 그대로 노출됩니다(릴리스 워크플로우가 섹션을 그대로 추출).

이후의 `### Added` / `### Changed` / `### Fixed`는 상세 추적용입니다.

```bash
git tag vX.Y.Z <commit>
git tag REL-vX.Y.Z vX.Y.Z^{}    # 중첩 태그 경고 회피용 디리퍼런스
git push origin vX.Y.Z REL-vX.Y.Z
```

### 외부 기여자 크레딧

Highlights 불릿이 외부 기여로 이뤄진 작업을 다룰 때는 인라인으로 크레딧을 표시합니다.

| 출처 | 접미사 |
|------|--------|
| 외부 PR | `(by @username, #PR)` |
| 외부 이슈/제안(코드는 유지보수자) | `(reported by @username, #issue)` |
| 양쪽 모두 같은 사람 | `(by @username, #PR / #issue)` |

위의 AI 트레일러 금지 규칙은 별개입니다 — 기계 생성 귀속을 금지할 뿐, 사람 기여자는 적극적이고 명시적으로 크레딧을 부여합니다.

## 보안

보안 취약점을 발견했다면 [SECURITY.md](../SECURITY.md)의 절차를 따라주세요. **공개 이슈를 열지 마세요.**
