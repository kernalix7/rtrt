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

/// Matches a passing libtest line (`test foo ... ok`, optionally with a
/// `--report-time` suffix). Anything else — FAILED / ignored / panics /
/// `failures:` blocks / `test result:` summaries — is kept verbatim.
static TEST_OK_LINE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^test .+ \.\.\. ok(?: <[^>]+>)?$").unwrap());

static RUNNING_N_TESTS: Lazy<Regex> = Lazy::new(|| Regex::new(r"^running \d+ tests?$").unwrap());

/// `---- <test name> stdout ----` (or stderr) — opens a captured-output block
/// for a failing test. Everything until the closing `failures:` list or the
/// `test result:` summary is that test's own output and must pass through
/// byte-identical: it can legitimately contain lines shaped exactly like
/// libtest progress or cargo noise.
static FAILURE_DUMP_HEADER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^---- .+ (stdout|stderr) ----$").unwrap());

/// `cargo test` filter: cargo build noise is dropped (same rules as
/// `cargo_noise`), consecutive passing `test … ... ok` lines collapse into a
/// single `✓ N passed` per run, and everything about failures — the FAILED
/// lines, panic/assertion output, the `failures:` blocks (including every line
/// of captured stdout/stderr), and the final `test result:` summary — survives
/// verbatim.
pub(crate) fn cargo_test(input: &str) -> String {
    let mut out = String::new();
    let mut ok_run = 0usize;
    let mut in_failure_dump = false;
    let mut last_blank = false;
    for line in input.lines() {
        if in_failure_dump {
            if line == "failures:" || line.starts_with("test result:") {
                // End of the captured-output region; both boundary lines are
                // kept by the normal path below.
                in_failure_dump = false;
            } else {
                // Inside a failing test's captured output: no collapsing, no
                // noise-dropping, no blank merging — verbatim.
                out.push_str(line);
                out.push('\n');
                last_blank = false;
                continue;
            }
        }
        if FAILURE_DUMP_HEADER.is_match(line) {
            flush_ok_run(&mut out, &mut ok_run);
            in_failure_dump = true;
            out.push_str(line);
            out.push('\n');
            last_blank = false;
            continue;
        }
        if TEST_OK_LINE.is_match(line) {
            ok_run += 1;
            continue;
        }
        let is_noise = (CARGO_NOISE.is_match(line)
            || CARGO_PROGRESS.is_match(line)
            || RUNNING_N_TESTS.is_match(line.trim()))
            && !is_error_or_warning(line);
        if is_noise {
            continue;
        }
        // Blank collapsing is done inline (not via collapse_blanks on the
        // final string) so failure dumps above stay untouched.
        let is_blank = line.trim().is_empty();
        if is_blank && last_blank {
            continue;
        }
        // Any kept line ends the run, so the ✓ summary lands where the ok
        // lines were and counts stay per test-binary section (each section
        // closes with a kept `test result:` line).
        flush_ok_run(&mut out, &mut ok_run);
        out.push_str(line);
        out.push('\n');
        last_blank = is_blank;
    }
    flush_ok_run(&mut out, &mut ok_run);

    if out.trim().is_empty() && !input.trim().is_empty() {
        "ok\n".to_string()
    } else {
        out
    }
}

fn flush_ok_run(out: &mut String, run: &mut usize) {
    if *run > 0 {
        out.push_str(&format!("✓ {run} passed\n"));
        *run = 0;
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
    fn cargo_test_collapses_ok_lines_and_keeps_failures_verbatim() {
        let raw = concat!(
            "   Compiling sample v0.1.0\n",
            "     Running unittests src/lib.rs (target/debug/deps/sample-abc)\n",
            "\n",
            "running 5 tests\n",
            "test tests::a ... ok\n",
            "test tests::b ... ok\n",
            "test tests::c ... FAILED\n",
            "test tests::d ... ignored\n",
            "test tests::e ... ok\n",
            "\n",
            "failures:\n",
            "\n",
            "---- tests::c stdout ----\n",
            "thread 'tests::c' panicked at src/lib.rs:13:46:\n",
            "assertion `left == right` failed\n",
            "  left: 2\n",
            " right: 3\n",
            "\n",
            "failures:\n",
            "    tests::c\n",
            "\n",
            "test result: FAILED. 3 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n",
        );
        let out = cargo_test(raw);
        // Passing lines collapse; runs split around the failure/ignored lines.
        assert!(out.contains("✓ 2 passed"), "{out}");
        assert!(out.contains("✓ 1 passed"), "{out}");
        assert!(!out.contains("test tests::a ... ok"), "{out}");
        // Failure surface survives byte-identical.
        assert!(out.contains("test tests::c ... FAILED"), "{out}");
        assert!(out.contains("test tests::d ... ignored"), "{out}");
        assert!(out.contains("---- tests::c stdout ----"), "{out}");
        assert!(
            out.contains("thread 'tests::c' panicked at src/lib.rs:13:46:"),
            "{out}"
        );
        assert!(out.contains("  left: 2"), "{out}");
        assert!(out.contains("failures:\n    tests::c"), "{out}");
        assert!(out.contains("test result: FAILED. 3 passed;"), "{out}");
        // Build noise and section headers are gone.
        assert!(!out.contains("Compiling"), "{out}");
        assert!(!out.contains("running 5 tests"), "{out}");
    }

    #[test]
    fn cargo_test_failure_dump_passes_through_verbatim() {
        // A failing test's captured stdout can contain lines shaped exactly
        // like libtest progress or cargo noise; inside the `---- … stdout ----`
        // block nothing may be collapsed, dropped, or blank-merged.
        let raw = concat!(
            "running 2 tests\n",
            "test tests::quick ... ok\n",
            "test tests::replay ... FAILED\n",
            "\n",
            "failures:\n",
            "\n",
            "---- tests::replay stdout ----\n",
            "running 2 tests\n",
            "test inner ... ok\n",
            "   Compiling x\n",
            "Running step 3 of 5\n",
            "\n",
            "\n",
            "thread 'tests::replay' panicked at src/lib.rs:18:9:\n",
            "replay diverged\n",
            "\n",
            "failures:\n",
            "    tests::replay\n",
            "\n",
            "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n",
        );
        let out = cargo_test(raw);
        // The whole dump — progress-shaped lines and the double blank —
        // survives byte-identical.
        assert!(
            out.contains(concat!(
                "---- tests::replay stdout ----\n",
                "running 2 tests\n",
                "test inner ... ok\n",
                "   Compiling x\n",
                "Running step 3 of 5\n",
                "\n",
                "\n",
                "thread 'tests::replay' panicked at src/lib.rs:18:9:\n",
            )),
            "{out}"
        );
        // Outside the dump, collapsing still works and no ✓ is fabricated
        // inside the dump.
        assert!(
            out.starts_with("✓ 1 passed\ntest tests::replay ... FAILED"),
            "{out}"
        );
        assert_eq!(out.matches('✓').count(), 1, "{out}");
        assert!(out.contains("test result: FAILED. 1 passed;"), "{out}");
    }

    #[test]
    fn cargo_test_all_green_keeps_summary_line() {
        let raw = "running 3 tests\n\
test a ... ok\n\
test b ... ok\n\
test c ... ok\n\
\n\
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
        let out = cargo_test(raw);
        assert!(out.contains("✓ 3 passed"), "{out}");
        assert!(out.contains("test result: ok. 3 passed;"), "{out}");
        assert!(!out.contains("... ok"), "{out}");
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
