//! Built-in templates — programmatic definitions, no external files at compile time.
//!
//! Each template uses `{{project_name}}`, `{{author}}`, `{{license}}` (default MIT) as
//! shared variables; specific templates may add more.

use once_cell::sync::Lazy;

use crate::{Template, TemplateFile, TemplateSource, TemplateVariable};

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
        rust_cli(),
        rust_lib(),
        rust_axum(),
        node_typescript(),
        python_uv(),
        go_cli(),
    ]
});

fn rust_cli() -> Template {
    Template {
        name: "rust-cli".into(),
        description: "Rust binary crate with clap + anyhow + tracing".into(),
        source: TemplateSource::BuiltIn,
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

const COMMON_README: &str = "# {{project_name}}\n\nAuthor: {{author}}\nLicense: {{license}}\n";

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
