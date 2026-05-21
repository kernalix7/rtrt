//! Built-in templates — three big-category starters, no per-language splits.
//!
//! One scaffold per category:
//!
//! - `개발` — code project skeleton (README, .gitignore, LICENSE, src/ stub).
//! - `디자인` — design kit (voice / palette / wireframe screens).
//! - `설계` — planning doc bundle (PRD + ADR template + roadmap).
//!
//! Each uses `{{project_name}}` and `{{author}}` as the shared variables.

use once_cell::sync::Lazy;

use crate::{Template, TemplateCategory, TemplateFile, TemplateSource, TemplateVariable};

fn common_vars() -> Vec<TemplateVariable> {
    vec![
        TemplateVariable {
            name: "project_name".into(),
            description: Some("프로젝트 / 이니셔티브 이름".into()),
            default: None,
            required: true,
        },
        TemplateVariable {
            name: "author".into(),
            description: Some("작성자 / 팀".into()),
            default: Some("Unknown".into()),
            required: false,
        },
    ]
}

pub static ALL: Lazy<Vec<Template>> = Lazy::new(|| vec![dev(), design(), plan()]);

fn dev() -> Template {
    Template {
        name: "dev".into(),
        description: "개발 — 코드 프로젝트 스타터 (README + LICENSE + .gitignore + src/)".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Development,
        variables: common_vars(),
        files: vec![
            TemplateFile {
                path: "README.md".into(),
                content: DEV_README.into(),
                executable: false,
            },
            TemplateFile {
                path: ".gitignore".into(),
                content: COMMON_GITIGNORE.into(),
                executable: false,
            },
            TemplateFile {
                path: "LICENSE".into(),
                content: LICENSE_PLACEHOLDER.into(),
                executable: false,
            },
            TemplateFile {
                path: "src/.gitkeep".into(),
                content: "".into(),
                executable: false,
            },
        ],
        post_hooks: vec!["git init".into()],
    }
}

fn design() -> Template {
    Template {
        name: "design".into(),
        description: "디자인 — 브랜드 보이스 + 토큰 + 와이어프레임 스크린 폴더".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Design,
        variables: common_vars(),
        files: vec![
            TemplateFile {
                path: "README.md".into(),
                content: DESIGN_README.into(),
                executable: false,
            },
            TemplateFile {
                path: "tokens.css".into(),
                content: DESIGN_TOKENS.into(),
                executable: false,
            },
            TemplateFile {
                path: "screens/01-home.md".into(),
                content: DESIGN_HOME.into(),
                executable: false,
            },
            TemplateFile {
                path: "logo/.gitkeep".into(),
                content: "".into(),
                executable: false,
            },
        ],
        post_hooks: vec![],
    }
}

fn plan() -> Template {
    Template {
        name: "plan".into(),
        description: "설계 — PRD + ADR 템플릿 + 로드맵 (필요한 만큼만 채우세요)".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Planning,
        variables: common_vars(),
        files: vec![
            TemplateFile {
                path: "PRD.md".into(),
                content: PLAN_PRD.into(),
                executable: false,
            },
            TemplateFile {
                path: "decisions/0001-template.md".into(),
                content: PLAN_ADR.into(),
                executable: false,
            },
            TemplateFile {
                path: "ROADMAP.md".into(),
                content: PLAN_ROADMAP.into(),
                executable: false,
            },
        ],
        post_hooks: vec![],
    }
}

const COMMON_GITIGNORE: &str = "# Build output\n/target\n/dist\n/build\nnode_modules/\n__pycache__/\n*.pyc\n.venv/\n\n# Local state\n.env\n*.log\n.DS_Store\n";

const LICENSE_PLACEHOLDER: &str = r#"Copyright (c) {{author}}

License: MIT (recommended) — replace this file with the full license text from
https://choosealicense.com/ if you want to be explicit.
"#;

const DEV_README: &str = r#"# {{project_name}}

작성자: {{author}}

## 시작

여기에 프로젝트 한 줄 소개.

## 구조

- `src/` — 소스 코드 (언어/스택은 자유롭게 — `cargo init` / `npm init` / `uv init` / `go mod init` 등 원하는 도구로 시작)
- `README.md` — 이 파일
- `LICENSE` — MIT 등 라이선스 텍스트

## 다음 단계

1. `cd {{project_name}} && git init` (이미 post-hook이 처리)
2. 언어별 패키지 매니저 초기화
3. 첫 번째 의존성 추가
"#;

const DESIGN_README: &str = r#"# {{project_name}} — 디자인 키트

작성자: {{author}}

## 보이스

- 톤: (예: 차분, 따뜻, 기술적)
- 금기: (예: 느낌표 남용, 마케팅 미사여구)

## 팔레트

`tokens.css` 에 HEX 값을 넣으면 웹 / Figma 양쪽에서 그대로 사용.

## 타이포

- 디스플레이:
- 본문:

## 로고

`logo/` 에 primary.svg + mono.svg 를 둔다. 마크 높이만큼 클리어스페이스 유지.

## 화면

`screens/` 폴더에 화면별 마크다운 파일. ASCII 박스로 저충실도 → Figma 링크로 고충실도 연결.
"#;

const DESIGN_TOKENS: &str = r#":root {
    /* 팔레트 */
    --color-bg: #ffffff;
    --color-fg: #0e0e0f;
    --color-accent: #2962FF;
    --color-muted: #6b6b6b;

    /* 간격 (4px 기본) */
    --space-1: 4px;
    --space-2: 8px;
    --space-3: 12px;
    --space-4: 16px;

    /* 타입 스케일 */
    --type-body: 14px;
    --type-display: 32px;
}
"#;

const DESIGN_HOME: &str = r#"# Home

```
+----------------------------------+
| logo                  [profile]  |
+----------------------------------+
|                                  |
|        Hero headline             |
|        sub-line                  |
|        [primary CTA]             |
|                                  |
+----------------------------------+
| feature 1 | feature 2 | feature 3|
+----------------------------------+
```

메모:
- 스크롤 시 nav 고정
- Hero CTA 클릭 → 온보딩 모달
"#;

const PLAN_PRD: &str = r#"# {{project_name}} — Product Requirements

작성자: {{author}}
상태: draft

## 문제

해결하려는 고통은 무엇인가? 지금 누가 겪는가? 가능하면 정량화.

## 사용자

주요 사용자:
보조 사용자:

## 목표 (우선순위 순)

1.
2.
3.

## 비-목표

-

## 솔루션 스케치

한 단락. 가치를 가장 빨리 전달하는 최소 절단 + 다음 두 차례 강화 파장.

## 성공 지표

- 북극성:
- 가드레일:

## 미해결 질문

-

## 마일스톤

| 시점 | 슬라이스 | 담당 |
|------|---------|------|
| M1   |         |      |
| M2   |         |      |
"#;

const PLAN_ADR: &str = r#"# ADR 0001 — (제목)

작성자: {{author}}
상태: proposed
일자: <오늘>

## 맥락

어떤 힘 / 제약이 작용하는가?

## 결정

한 단락 정리.

## 결과

긍정:
-

부정:
-

후속:
-

---

추가 결정은 `decisions/0002-…md`, `0003-…md` 식으로 새 파일을 만드세요.
"#;

const PLAN_ROADMAP: &str = r#"# {{project_name}} — Roadmap

작성자: {{author}}
범위: 4분기

## 이번 분기 (now)

- [ ]
- [ ]

## 다음 분기

- [ ]

## +2 분기

- [ ]

## +3 분기

- [ ]

## 리스크

-
"#;
