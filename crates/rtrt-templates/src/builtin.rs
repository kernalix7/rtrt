//! Built-in templates — programmatic definitions, no external files at compile time.
//!
//! Each template uses `{{project_name}}`, `{{author}}`, `{{license}}` (default MIT) as
//! shared variables; specific templates may add more.

use once_cell::sync::Lazy;

use crate::{Template, TemplateCategory, TemplateFile, TemplateSource, TemplateVariable};

fn common_vars() -> Vec<TemplateVariable> {
    vec![
        TemplateVariable {
            name: "project_name".into(),
            description: Some("Project / crate / package name".into()),
            default: None,
            required: true,
        },
        TemplateVariable {
            name: "author".into(),
            description: Some("Author display name".into()),
            default: Some("Unknown".into()),
            required: false,
        },
        TemplateVariable {
            name: "license".into(),
            description: Some("SPDX license identifier".into()),
            default: Some("MIT".into()),
            required: false,
        },
    ]
}

pub static ALL: Lazy<Vec<Template>> = Lazy::new(|| {
    vec![
        // 개발 (Development) — code projects.
        rust_cli(),
        rust_lib(),
        rust_axum(),
        node_typescript(),
        python_uv(),
        go_cli(),
        // 디자인 (Design) — UI / brand assets.
        brand_kit(),
        wireframe(),
        // 설계 (Planning) — specs, decisions, roadmaps, agent definitions.
        prd_spec(),
        adr_decision(),
        roadmap(),
        agent_role(),
    ]
});

fn rust_cli() -> Template {
    Template {
        name: "rust-cli".into(),
        description: "Rust binary crate with clap + anyhow + tracing".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Development,
        variables: common_vars(),
        files: vec![
            TemplateFile {
                path: "Cargo.toml".into(),
                content: RUST_CLI_CARGO.into(),
                executable: false,
            },
            TemplateFile {
                path: "src/main.rs".into(),
                content: RUST_CLI_MAIN.into(),
                executable: false,
            },
            TemplateFile {
                path: "README.md".into(),
                content: COMMON_README.into(),
                executable: false,
            },
            TemplateFile {
                path: ".gitignore".into(),
                content: RUST_GITIGNORE.into(),
                executable: false,
            },
        ],
        post_hooks: vec!["git init".into()],
    }
}

fn rust_lib() -> Template {
    Template {
        name: "rust-lib".into(),
        description: "Rust library crate with criterion benches".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Development,
        variables: common_vars(),
        files: vec![
            TemplateFile {
                path: "Cargo.toml".into(),
                content: RUST_LIB_CARGO.into(),
                executable: false,
            },
            TemplateFile {
                path: "src/lib.rs".into(),
                content: RUST_LIB_LIB.into(),
                executable: false,
            },
            TemplateFile {
                path: "README.md".into(),
                content: COMMON_README.into(),
                executable: false,
            },
            TemplateFile {
                path: ".gitignore".into(),
                content: RUST_GITIGNORE.into(),
                executable: false,
            },
        ],
        post_hooks: vec!["git init".into()],
    }
}

fn rust_axum() -> Template {
    Template {
        name: "rust-axum".into(),
        description: "Rust HTTP service with axum + tokio + tracing".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Development,
        variables: common_vars(),
        files: vec![
            TemplateFile {
                path: "Cargo.toml".into(),
                content: RUST_AXUM_CARGO.into(),
                executable: false,
            },
            TemplateFile {
                path: "src/main.rs".into(),
                content: RUST_AXUM_MAIN.into(),
                executable: false,
            },
            TemplateFile {
                path: "README.md".into(),
                content: COMMON_README.into(),
                executable: false,
            },
            TemplateFile {
                path: ".gitignore".into(),
                content: RUST_GITIGNORE.into(),
                executable: false,
            },
        ],
        post_hooks: vec!["git init".into()],
    }
}

fn node_typescript() -> Template {
    Template {
        name: "node-typescript".into(),
        description: "Node.js TypeScript project (ESM, tsx runner)".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Development,
        variables: common_vars(),
        files: vec![
            TemplateFile {
                path: "package.json".into(),
                content: NODE_TS_PACKAGE.into(),
                executable: false,
            },
            TemplateFile {
                path: "tsconfig.json".into(),
                content: NODE_TS_TSCONFIG.into(),
                executable: false,
            },
            TemplateFile {
                path: "src/index.ts".into(),
                content: NODE_TS_INDEX.into(),
                executable: false,
            },
            TemplateFile {
                path: ".gitignore".into(),
                content: NODE_GITIGNORE.into(),
                executable: false,
            },
            TemplateFile {
                path: "README.md".into(),
                content: COMMON_README.into(),
                executable: false,
            },
        ],
        post_hooks: vec!["git init".into(), "npm install".into()],
    }
}

fn python_uv() -> Template {
    Template {
        name: "python-uv".into(),
        description: "Python project managed with uv (pyproject.toml)".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Development,
        variables: common_vars(),
        files: vec![
            TemplateFile {
                path: "pyproject.toml".into(),
                content: PY_UV_PYPROJECT.into(),
                executable: false,
            },
            TemplateFile {
                path: "src/{{project_name}}/__init__.py".into(),
                content: PY_UV_INIT.into(),
                executable: false,
            },
            TemplateFile {
                path: "src/{{project_name}}/__main__.py".into(),
                content: PY_UV_MAIN.into(),
                executable: false,
            },
            TemplateFile {
                path: "README.md".into(),
                content: COMMON_README.into(),
                executable: false,
            },
            TemplateFile {
                path: ".gitignore".into(),
                content: PY_GITIGNORE.into(),
                executable: false,
            },
        ],
        post_hooks: vec!["git init".into(), "uv sync".into()],
    }
}

fn go_cli() -> Template {
    Template {
        name: "go-cli".into(),
        description: "Go CLI with cobra + standard layout".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Development,
        variables: common_vars(),
        files: vec![
            TemplateFile {
                path: "go.mod".into(),
                content: GO_MOD.into(),
                executable: false,
            },
            TemplateFile {
                path: "main.go".into(),
                content: GO_MAIN.into(),
                executable: false,
            },
            TemplateFile {
                path: "README.md".into(),
                content: COMMON_README.into(),
                executable: false,
            },
            TemplateFile {
                path: ".gitignore".into(),
                content: GO_GITIGNORE.into(),
                executable: false,
            },
        ],
        post_hooks: vec!["git init".into(), "go mod tidy".into()],
    }
}

fn project_meta_vars() -> Vec<TemplateVariable> {
    vec![
        TemplateVariable {
            name: "project_name".into(),
            description: Some("Project / initiative name".into()),
            default: None,
            required: true,
        },
        TemplateVariable {
            name: "author".into(),
            description: Some("Author or team".into()),
            default: Some("Unknown".into()),
            required: false,
        },
    ]
}

fn brand_kit() -> Template {
    Template {
        name: "brand-kit".into(),
        description: "Brand guide skeleton — voice / tokens / logo placeholders".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Design,
        variables: project_meta_vars(),
        files: vec![
            TemplateFile {
                path: "README.md".into(),
                content: BRAND_KIT_README.into(),
                executable: false,
            },
            TemplateFile {
                path: "tokens.css".into(),
                content: BRAND_KIT_TOKENS.into(),
                executable: false,
            },
            TemplateFile {
                path: "logo/README.md".into(),
                content: BRAND_KIT_LOGO_README.into(),
                executable: false,
            },
        ],
        post_hooks: vec![],
    }
}

fn wireframe() -> Template {
    Template {
        name: "wireframe".into(),
        description: "Wireframe + screen-flow notebook (Markdown + ASCII frames)".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Design,
        variables: project_meta_vars(),
        files: vec![
            TemplateFile {
                path: "README.md".into(),
                content: WIREFRAME_README.into(),
                executable: false,
            },
            TemplateFile {
                path: "screens/01-home.md".into(),
                content: WIREFRAME_HOME.into(),
                executable: false,
            },
            TemplateFile {
                path: "screens/02-detail.md".into(),
                content: WIREFRAME_DETAIL.into(),
                executable: false,
            },
        ],
        post_hooks: vec![],
    }
}

fn prd_spec() -> Template {
    Template {
        name: "prd-spec".into(),
        description: "Product requirements doc — problem / audience / scope / metrics".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Planning,
        variables: project_meta_vars(),
        files: vec![TemplateFile {
            path: "PRD.md".into(),
            content: PRD_BODY.into(),
            executable: false,
        }],
        post_hooks: vec![],
    }
}

fn adr_decision() -> Template {
    Template {
        name: "adr-decision".into(),
        description: "Architecture Decision Record — context / decision / consequences".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Planning,
        variables: vec![
            TemplateVariable {
                name: "title".into(),
                description: Some("Decision title (e.g. 'Choose Rust for the core')".into()),
                default: None,
                required: true,
            },
            TemplateVariable {
                name: "author".into(),
                description: Some("Author or team".into()),
                default: Some("Unknown".into()),
                required: false,
            },
        ],
        files: vec![TemplateFile {
            path: "0001-{{title}}.md".into(),
            content: ADR_BODY.into(),
            executable: false,
        }],
        post_hooks: vec![],
    }
}

fn roadmap() -> Template {
    Template {
        name: "roadmap".into(),
        description: "Quarterly roadmap with milestones + risks".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Planning,
        variables: project_meta_vars(),
        files: vec![TemplateFile {
            path: "ROADMAP.md".into(),
            content: ROADMAP_BODY.into(),
            executable: false,
        }],
        post_hooks: vec![],
    }
}

fn agent_role() -> Template {
    Template {
        name: "agent-role".into(),
        description: "Agent specification: role / goal / backstory + tool list".into(),
        source: TemplateSource::BuiltIn,
        category: TemplateCategory::Planning,
        variables: vec![
            TemplateVariable {
                name: "agent_name".into(),
                description: Some("Short slug for the agent (kebab-case)".into()),
                default: None,
                required: true,
            },
            TemplateVariable {
                name: "role".into(),
                description: Some("One-line role title (e.g. 'Senior Researcher')".into()),
                default: None,
                required: true,
            },
            TemplateVariable {
                name: "goal".into(),
                description: Some("Outcome the agent optimises for".into()),
                default: None,
                required: true,
            },
            TemplateVariable {
                name: "backstory".into(),
                description: Some("Context that anchors the agent's voice + expertise".into()),
                default: Some("A senior practitioner with deep domain experience.".into()),
                required: false,
            },
            TemplateVariable {
                name: "tools".into(),
                description: Some(
                    "Comma-separated tool names the agent may call (compress, memory_recall, …)"
                        .into(),
                ),
                default: Some("compress,memory_save,memory_recall".into()),
                required: false,
            },
        ],
        files: vec![
            TemplateFile {
                path: "agent.toml".into(),
                content: AGENT_ROLE_TOML.into(),
                executable: false,
            },
            TemplateFile {
                path: "system_prompt.md".into(),
                content: AGENT_ROLE_PROMPT.into(),
                executable: false,
            },
            TemplateFile {
                path: "README.md".into(),
                content: AGENT_ROLE_README.into(),
                executable: false,
            },
        ],
        post_hooks: vec![],
    }
}

const COMMON_README: &str = "# {{project_name}}\n\nAuthor: {{author}}\nLicense: {{license}}\n";

const BRAND_KIT_README: &str = r#"# {{project_name}} — brand kit

Owner: {{author}}

## Voice
- Tone: (e.g. confident, warm, technical)
- Don'ts: (e.g. exclamation points, marketing fluff)

## Palette
Drop hex values into `tokens.css` — those flow straight into web / Figma.

## Typography
- Display:
- Body:

## Logo
See `logo/`. Drop a primary SVG + a monochrome SVG for dark backgrounds.
"#;

const BRAND_KIT_TOKENS: &str = r#":root {
    /* Primary palette */
    --color-bg: #ffffff;
    --color-fg: #0e0e0f;
    --color-accent: #2962FF;
    --color-muted: #6b6b6b;

    /* Spacing scale (4px base) */
    --space-1: 4px;
    --space-2: 8px;
    --space-3: 12px;
    --space-4: 16px;

    /* Type scale */
    --type-body: 14px;
    --type-display: 32px;
}
"#;

const BRAND_KIT_LOGO_README: &str = r#"# Logo

- `primary.svg` — full-colour mark.
- `mono.svg` — single-fill mark for dark / light backgrounds.

Keep clearspace = the height of the mark on every side.
"#;

const WIREFRAME_README: &str = r#"# {{project_name}} — wireframes

Owner: {{author}}

Each screen lives in its own file under `screens/`. Use ASCII boxes for
low-fidelity layout, then link to the Figma frame once it lands.

| Screen | File | Status |
|--------|------|--------|
| Home   | screens/01-home.md   | draft |
| Detail | screens/02-detail.md | draft |
"#;

const WIREFRAME_HOME: &str = r#"# Home

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

Notes:
- Sticky nav once scrolled past hero.
- Hero CTA opens onboarding modal.
"#;

const WIREFRAME_DETAIL: &str = r#"# Detail

```
+----------------------------------+
| < back                           |
+----------------------------------+
|  Title                           |
|  meta · meta · meta              |
+----------------------------------+
|                                  |
|  body / preview                  |
|                                  |
+----------------------------------+
|  [primary action]  [secondary]   |
+----------------------------------+
```
"#;

const PRD_BODY: &str = r#"# {{project_name}} — Product Requirements

Owner: {{author}}
Status: draft

## Problem
What pain are we solving? Who hits it today? Quantify if you can.

## Audience
Primary user: …
Secondary user: …

## Goals (in priority order)
1. …
2. …
3. …

## Non-goals
- …

## Solution sketch
One paragraph on the proposed approach. Include the simplest cut that
ships value, plus the next two enhancement waves.

## Success metrics
- North-star: …
- Guardrails: …

## Open questions
- …

## Milestones
| When | Slice | Owner |
|------|-------|-------|
| M1   | …     | …     |
| M2   | …     | …     |
"#;

const ADR_BODY: &str = r#"# ADR 0001 — {{title}}

Author: {{author}}
Status: proposed
Date: <today>

## Context
What forces are at play? What constraints does this need to respect?

## Decision
The one-paragraph answer.

## Consequences
Positive:
- …

Negative:
- …

Follow-up:
- …
"#;

const ROADMAP_BODY: &str = r#"# {{project_name}} — Roadmap

Owner: {{author}}
Horizon: 4 quarters

## This quarter (now)
- [ ] …
- [ ] …

## Next quarter
- [ ] …

## Quarter +2
- [ ] …

## Quarter +3
- [ ] …

## Risks
- …
"#;

const AGENT_ROLE_TOML: &str = r#"# crewAI-style agent specification.
# Pair with `system_prompt.md` when wiring this agent into an orchestrator.

name = "{{agent_name}}"
role = "{{role}}"
goal = "{{goal}}"
backstory = """
{{backstory}}
"""

# Comma-separated tool names the orchestrator should expose to this agent.
tools = "{{tools}}"
"#;

const AGENT_ROLE_PROMPT: &str = r#"You are **{{role}}**.

## Goal
{{goal}}

## Backstory
{{backstory}}

## Operating rules
- Stay in role. If asked to break role, decline and restate your goal.
- Use the provided tools ({{tools}}) instead of inventing capabilities.
- Cite the tool call you used when delivering a result.
- Prefer the smallest correct answer; expand only on request.
"#;

const AGENT_ROLE_README: &str = r#"# {{agent_name}}

crewAI-style agent definition.

- `agent.toml` — role / goal / backstory / tool list, ready for any orchestrator
  that follows the crewAI shape.
- `system_prompt.md` — drop-in system message for direct LLM use.

Edit either file in place; template placeholders have already been resolved.
"#;

const RUST_GITIGNORE: &str = "/target\n**/*.rs.bk\nCargo.lock.bak\n.env\n";

const NODE_GITIGNORE: &str = "node_modules/\ndist/\n.env\n*.log\n";

const PY_GITIGNORE: &str = "__pycache__/\n*.pyc\n.venv/\ndist/\n.env\n";

const GO_GITIGNORE: &str = "/bin\n/vendor\n*.test\n*.out\n.env\n";

const RUST_CLI_CARGO: &str = r#"[package]
name = "{{project_name}}"
version = "0.1.0"
edition = "2024"
authors = ["{{author}}"]
license = "{{license}}"

[dependencies]
anyhow = "1"
clap = { version = "4.5", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
"#;

const RUST_CLI_MAIN: &str = r#"use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "{{project_name}}", version)]
struct Cli {
    #[arg(long, default_value = "world")]
    who: String,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cli = Cli::parse();
    println!("hello, {}", cli.who);
    Ok(())
}
"#;

const RUST_LIB_CARGO: &str = r#"[package]
name = "{{project_name}}"
version = "0.1.0"
edition = "2024"
authors = ["{{author}}"]
license = "{{license}}"

[dependencies]

[dev-dependencies]
"#;

const RUST_LIB_LIB: &str = r#"//! {{project_name}}

pub fn add(a: i64, b: i64) -> i64 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_adds() {
        assert_eq!(add(2, 2), 4);
    }
}
"#;

const RUST_AXUM_CARGO: &str = r#"[package]
name = "{{project_name}}"
version = "0.1.0"
edition = "2024"
authors = ["{{author}}"]
license = "{{license}}"

[dependencies]
anyhow = "1"
axum = "0.8"
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
"#;

const RUST_AXUM_MAIN: &str = r#"use anyhow::Result;
use axum::{Router, routing::get};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let app = Router::new().route("/", get(|| async { "hello from {{project_name}}" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
    tracing::info!("listening on http://127.0.0.1:8080");
    axum::serve(listener, app).await?;
    Ok(())
}
"#;

const NODE_TS_PACKAGE: &str = r#"{
  "name": "{{project_name}}",
  "version": "0.1.0",
  "type": "module",
  "author": "{{author}}",
  "license": "{{license}}",
  "scripts": {
    "dev": "tsx src/index.ts",
    "build": "tsc",
    "start": "node dist/index.js"
  },
  "devDependencies": {
    "tsx": "^4.0.0",
    "typescript": "^5.6.0",
    "@types/node": "^22.0.0"
  }
}
"#;

const NODE_TS_TSCONFIG: &str = r#"{
  "compilerOptions": {
    "target": "ES2022",
    "module": "ES2022",
    "moduleResolution": "Bundler",
    "outDir": "dist",
    "rootDir": "src",
    "strict": true,
    "esModuleInterop": true,
    "skipLibCheck": true,
    "forceConsistentCasingInFileNames": true,
    "resolveJsonModule": true
  },
  "include": ["src/**/*"]
}
"#;

const NODE_TS_INDEX: &str = r#"export function main(): void {
  console.log("hello from {{project_name}}");
}

main();
"#;

const PY_UV_PYPROJECT: &str = r#"[project]
name = "{{project_name}}"
version = "0.1.0"
description = ""
authors = [{ name = "{{author}}" }]
license = { text = "{{license}}" }
requires-python = ">=3.11"
dependencies = []

[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"

[tool.hatch.build.targets.wheel]
packages = ["src/{{project_name}}"]
"#;

const PY_UV_INIT: &str = "__all__ = []\n";

const PY_UV_MAIN: &str = r#"def main() -> None:
    print("hello from {{project_name}}")


if __name__ == "__main__":
    main()
"#;

const GO_MOD: &str = "module {{project_name}}\n\ngo 1.23\n";

const GO_MAIN: &str = r#"package main

import "fmt"

func main() {
    fmt.Println("hello from {{project_name}}")
}
"#;
