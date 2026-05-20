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
    fn cargo_strips_compiling_lines() {
        let raw = "   Compiling foo v0.1.0\n   Compiling bar v0.2.0\nerror[E0001]: real error\n   Finished dev\n";
        let out = cargo_noise(raw);
        assert!(out.contains("real error"));
        assert!(!out.contains("Compiling"));
        assert!(!out.contains("Finished"));
    }
}
