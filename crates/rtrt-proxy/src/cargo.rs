use once_cell::sync::Lazy;
use regex::Regex;

use crate::{collapse_blanks, is_error_or_warning};

static CARGO_NOISE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?m)^\s*(Compiling|Checking|Finished|Downloading|Downloaded|Fresh|Blocking|Waiting for file lock|Updating|Adding) .*$",
    )
    .unwrap()
});

static CARGO_PROGRESS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\s*(Running|Doc-tests|Building|Packaging|Verifying) .*$").unwrap()
});

pub fn cargo_noise(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        if (CARGO_NOISE.is_match(line) || CARGO_PROGRESS.is_match(line))
            && !is_error_or_warning(line)
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }

    let collapsed = collapse_blanks(&out);
    if collapsed.trim().is_empty() && !input.trim().is_empty() {
        "ok\n".to_string()
    } else {
        collapsed
    }
}

pub(crate) fn cargo_nextest(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let trimmed = line.trim_start();
        let is_progress = trimmed.starts_with("Starting ")
            || trimmed.starts_with("PASS ")
            || trimmed.starts_with("SKIP ")
            || trimmed.starts_with("SLOW ")
            || trimmed.starts_with("RETRY ")
            || trimmed.starts_with("Cancelling due to test failure");

        if is_progress && !is_error_or_warning(line) {
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }

    let collapsed = collapse_blanks(&out);
    if collapsed.trim().is_empty() && !input.trim().is_empty() {
        "ok\n".to_string()
    } else {
        collapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_strips_compiling_lines_and_keeps_errors() {
        let raw = "   Compiling foo v0.1.0\n   Checking bar v0.2.0\nerror[E0001]: real error\n   Finished dev\n";
        let out = cargo_noise(raw);
        assert!(out.contains("real error"));
        assert!(!out.contains("Compiling"));
        assert!(!out.contains("Checking"));
        assert!(!out.contains("Finished"));
    }

    #[test]
    fn cargo_clippy_keeps_warnings() {
        let raw = "    Checking foo v0.1.0\nwarning: needless borrow\n  --> src/lib.rs:1:1\n    Finished dev\n";
        let out = cargo_noise(raw);
        assert!(out.contains("warning: needless borrow"));
        assert!(out.contains("--> src/lib.rs"));
        assert!(!out.contains("Checking foo"));
    }

    #[test]
    fn nextest_drops_pass_progress_but_keeps_failures() {
        let raw = "Starting 3 tests across 1 binary\nPASS test_a\nFAIL test_b\nerror: test failed\nSummary [0.123s] 2 tests run: 1 passed, 1 failed\n";
        let out = cargo_nextest(raw);
        assert!(!out.contains("PASS test_a"));
        assert!(out.contains("FAIL test_b"));
        assert!(out.contains("error: test failed"));
        assert!(out.contains("Summary"));
    }
}
