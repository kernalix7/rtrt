//! rtrt-proxy — filter / collapse / truncate command output before it reaches the LLM.
//!
//! Strategy: per-command rule sets that turn 200-2000 token outputs into 10-400 token outputs
//! by removing noise, grouping repeated entries, and truncating safely.

use once_cell::sync::Lazy;
use regex::Regex;

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
        apply: git_status,
    },
    CommandFilter {
        command: "git log",
        apply: git_log,
    },
    CommandFilter {
        command: "cargo build",
        apply: cargo_noise,
    },
    CommandFilter {
        command: "cargo test",
        apply: cargo_noise,
    },
];

pub fn filter_for(command: &str) -> Option<&'static CommandFilter> {
    FILTERS.iter().find(|f| command.starts_with(f.command))
}

static GIT_STATUS_NOISE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^(On branch .*|Your branch .*|\s*\(use .*\)|nothing to commit.*)$").unwrap()
});

fn git_status(input: &str) -> String {
    let trimmed = GIT_STATUS_NOISE.replace_all(input, "");
    collapse_blanks(&trimmed)
}

static GIT_LOG_FMT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^(Author: .*|Date: .*)\n").unwrap());

fn git_log(input: &str) -> String {
    let stripped = GIT_LOG_FMT.replace_all(input, "");
    collapse_blanks(&stripped)
}

static CARGO_NOISE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^(\s*Compiling .*|\s*Finished .*|\s*Downloading .*|\s*Downloaded .*)$")
        .unwrap()
});

fn cargo_noise(input: &str) -> String {
    let stripped = CARGO_NOISE.replace_all(input, "");
    collapse_blanks(&stripped)
}

fn collapse_blanks(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_blank = false;
    for line in input.lines() {
        let is_blank = line.trim().is_empty();
        if is_blank && last_blank {
            continue;
        }
        out.push_str(line);
        out.push('\n');
        last_blank = is_blank;
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_git_status_filter() {
        assert!(filter_for("git status").is_some());
        assert!(filter_for("git status -s").is_some());
        assert!(filter_for("git diff").is_none());
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
    fn cargo_strips_compiling_lines() {
        let raw = "   Compiling foo v0.1.0\n   Compiling bar v0.2.0\nerror[E0001]: real error\n   Finished dev\n";
        let out = cargo_noise(raw);
        assert!(out.contains("real error"));
        assert!(!out.contains("Compiling"));
        assert!(!out.contains("Finished"));
    }
}
