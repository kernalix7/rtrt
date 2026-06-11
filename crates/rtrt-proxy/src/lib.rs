//! rtrt-proxy — filter / collapse / truncate command output before it reaches the LLM.
//!
//! Strategy: per-command rule sets that turn 200-2000 token outputs into 10-400 token outputs
//! by removing noise, grouping repeated entries, and truncating safely.

mod cargo;
mod containers;
mod generic;
mod git;
mod kubernetes;
mod node;

use once_cell::sync::Lazy;
use regex::Regex;

pub use cargo::cargo_noise;
pub use generic::collapse_blanks;

pub struct CommandFilter {
    pub command: &'static str,
    apply: fn(&str) -> String,
}

impl CommandFilter {
    pub fn apply(&self, raw: &str) -> String {
        (self.apply)(raw)
    }
}

pub static FILTERS: &[CommandFilter] = &[
    CommandFilter {
        command: "git status",
        apply: git::git_status,
    },
    CommandFilter {
        command: "git diff",
        apply: git::git_passthrough,
    },
    CommandFilter {
        command: "git show",
        apply: git::git_show,
    },
    CommandFilter {
        command: "git branch",
        apply: git::git_branch,
    },
    CommandFilter {
        command: "git stash",
        apply: git::git_stash,
    },
    CommandFilter {
        command: "git log",
        apply: git::git_log,
    },
    CommandFilter {
        command: "cargo nextest",
        apply: cargo::cargo_nextest,
    },
    CommandFilter {
        command: "cargo check",
        apply: cargo::cargo_noise,
    },
    CommandFilter {
        command: "cargo clippy",
        apply: cargo::cargo_noise,
    },
    CommandFilter {
        command: "cargo build",
        apply: cargo::cargo_noise,
    },
    CommandFilter {
        command: "cargo test",
        apply: cargo::cargo_noise,
    },
    CommandFilter {
        command: "ls -la",
        apply: generic::ls_long,
    },
    CommandFilter {
        command: "ls -al",
        apply: generic::ls_long,
    },
    CommandFilter {
        command: "ls",
        apply: generic::ls_passthrough,
    },
    CommandFilter {
        command: "grep -rn",
        apply: generic::passthrough,
    },
    CommandFilter {
        command: "grep",
        apply: generic::grep_filter,
    },
    CommandFilter {
        command: "rg",
        apply: generic::grep_filter,
    },
    CommandFilter {
        command: "find",
        apply: generic::find_filter,
    },
    CommandFilter {
        command: "cat",
        apply: generic::passthrough,
    },
    CommandFilter {
        command: "read",
        apply: generic::passthrough,
    },
    CommandFilter {
        command: "curl",
        apply: generic::http_client_filter,
    },
    CommandFilter {
        command: "wget",
        apply: generic::http_client_filter,
    },
    CommandFilter {
        command: "gh",
        apply: generic::gh_filter,
    },
    CommandFilter {
        command: "docker",
        apply: containers::docker_filter,
    },
    CommandFilter {
        command: "kubectl",
        apply: kubernetes::kubectl_filter,
    },
    CommandFilter {
        command: "pytest",
        apply: generic::pytest_filter,
    },
    CommandFilter {
        command: "go test",
        apply: generic::go_test_filter,
    },
    CommandFilter {
        command: "npm",
        apply: node::node_filter,
    },
    CommandFilter {
        command: "npx",
        apply: node::node_filter,
    },
    CommandFilter {
        command: "pnpm",
        apply: node::node_filter,
    },
    CommandFilter {
        command: "pip",
        apply: generic::pip_filter,
    },
    CommandFilter {
        command: "tsc",
        apply: node::node_diagnostics,
    },
    CommandFilter {
        command: "eslint",
        apply: node::node_diagnostics,
    },
    CommandFilter {
        command: "prettier",
        apply: node::node_diagnostics,
    },
];

pub fn filter_for(command: &str) -> Option<&'static CommandFilter> {
    FILTERS.iter().find(|f| command_matches(command, f.command))
}

fn command_matches(command: &str, prefix: &str) -> bool {
    command == prefix
        || command
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace) || rest.starts_with('-'))
}

/// rtk-style "errors-only" filter — keeps lines that look like errors,
/// warnings, panics, or stack-frame markers plus a configurable number of
/// context lines around each match. Useful for `rtrt run <cmd>` where the
/// caller wants the LLM to see only the failure surface.
static ERROR_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(error(\[E\d+\])?:|warning:|panic|fatal|failed|fail\b|traceback|stacktrace|stack overflow|unhandled|undefined reference|cannot find|not found:|expected .* found|test result: FAILED|---- .* FAILED)",
    )
    .unwrap()
});

pub fn errors_only(input: &str, context_lines: usize) -> String {
    let lines: Vec<&str> = input.lines().collect();
    let mut keep = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        if ERROR_PATTERN.is_match(line) {
            let lo = i.saturating_sub(context_lines);
            let hi = (i + context_lines + 1).min(lines.len());
            for slot in keep.iter_mut().take(hi).skip(lo) {
                *slot = true;
            }
        }
    }
    let mut out = String::with_capacity(input.len() / 2);
    let mut prev_kept = false;
    for (i, line) in lines.iter().enumerate() {
        if keep[i] {
            if !prev_kept && i > 0 {
                out.push_str("...\n");
            }
            out.push_str(line);
            out.push('\n');
            prev_kept = true;
        } else {
            prev_kept = false;
        }
    }
    out
}

/// rtk-style `--ultra-compact` pass: strips ANSI escape sequences and
/// collapses runs of repeated identical lines into `<line> (×N)`.
static ANSI_ESC: Lazy<Regex> = Lazy::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap());

pub fn ultra_compact(input: &str) -> String {
    let stripped = ANSI_ESC.replace_all(input, "");
    let mut out = String::with_capacity(stripped.len());
    let mut prev: Option<String> = None;
    let mut run = 0u32;
    for line in stripped.lines() {
        let owned = line.to_string();
        match &prev {
            Some(p) if p == &owned => run += 1,
            _ => {
                if let Some(p) = prev.take() {
                    out.push_str(&p);
                    if run > 1 {
                        out.push_str(&format!(" (×{run})"));
                    }
                    out.push('\n');
                }
                prev = Some(owned);
                run = 1;
            }
        }
    }
    if let Some(p) = prev {
        out.push_str(&p);
        if run > 1 {
            out.push_str(&format!(" (×{run})"));
        }
        out.push('\n');
    }
    out
}

pub(crate) fn is_error_or_warning(line: &str) -> bool {
    ERROR_PATTERN.is_match(line)
}

pub(crate) fn collapse_repeated_adjacent(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev: Option<&str> = None;
    let mut run = 0usize;

    for line in input.lines() {
        match prev {
            Some(previous) if previous == line => run += 1,
            Some(previous) => {
                push_repeated_line(&mut out, previous, run);
                prev = Some(line);
                run = 1;
            }
            None => {
                prev = Some(line);
                run = 1;
            }
        }
    }

    if let Some(previous) = prev {
        push_repeated_line(&mut out, previous, run);
    }

    out
}

fn push_repeated_line(out: &mut String, line: &str, run: usize) {
    out.push_str(line);
    if run > 1 {
        out.push_str(&format!(" (×{run})"));
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_expanded_filters() {
        assert!(filter_for("git status").is_some());
        assert!(filter_for("git status -s").is_some());
        assert!(filter_for("git diff --stat").is_some());
        assert!(filter_for("cargo clippy --all-targets").is_some());
        assert!(filter_for("docker build .").is_some());
        assert!(filter_for("kubectl get pods").is_some());
        assert!(filter_for("pytest -q").is_some());
        assert!(filter_for("go test ./...").is_some());
        assert!(filter_for("npm test").is_some());
    }

    #[test]
    fn errors_only_keeps_failures_with_context() {
        let raw = "info line\nanother info\nerror[E0001]: type mismatch\n  --> src/foo.rs:5:1\nmore info\nyet more\nwarning: unused variable `x`\nend";
        let out = errors_only(raw, 1);
        assert!(out.contains("type mismatch"));
        assert!(out.contains("unused variable"));
        // Adjacent info lines kept via context window.
        assert!(out.contains("another info"));
        // Distant info lines dropped.
        assert!(!out.contains("end") || out.contains("..."));
    }

    #[test]
    fn ultra_compact_collapses_runs_and_strips_ansi() {
        let raw = "\x1b[31mError\x1b[0m\nsame line\nsame line\nsame line\ndifferent\n";
        let out = ultra_compact(raw);
        assert!(!out.contains("\x1b"));
        assert!(out.contains("same line (×3)"));
        assert!(out.contains("different"));
    }

    #[test]
    fn filter_count_tracks_broad_command_coverage() {
        assert_eq!(FILTERS.len(), 34);
    }
}
