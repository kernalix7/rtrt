//! Fixture-driven correctness tests for the heavy-output filters.
//!
//! Every fixture is a real captured output (`git diff` from this repo's
//! history, `cargo test` runs, `rg` over the workspace). The assertions pin
//! the lossless guarantees: changed lines, failures, filenames, and line
//! numbers must survive filtering byte-identical.

use std::collections::HashSet;

const GIT_DIFF: &str = include_str!("fixtures/git_diff.txt");
const GIT_DIFF_U10: &str = include_str!("fixtures/git_diff_u10.txt");
const CARGO_TEST_PASS: &str = include_str!("fixtures/cargo_test_pass.txt");
const CARGO_TEST_FAIL: &str = include_str!("fixtures/cargo_test_fail.txt");
const RG_NO_HEADING: &str = include_str!("fixtures/rg_no_heading.txt");
const RG_HEADING: &str = include_str!("fixtures/rg_heading.txt");

fn apply(command: &str, input: &str) -> String {
    rtrt_proxy::filter_for(command)
        .unwrap_or_else(|| panic!("no filter for {command}"))
        .apply(input)
}

#[test]
fn git_diff_keeps_every_changed_line_byte_identical() {
    let out = apply("git diff", GIT_DIFF);
    let kept: HashSet<&str> = out.lines().collect();
    for line in GIT_DIFF.lines() {
        let is_change = (line.starts_with('+') || line.starts_with('-'))
            && !line.starts_with("+++")
            && !line.starts_with("---");
        if is_change || line.starts_with("@@") || line.starts_with("diff --git") {
            assert!(kept.contains(line), "lost line: {line:?}");
        }
    }
}

#[test]
fn git_diff_keeps_all_file_and_hunk_headers() {
    let out = apply("git diff", GIT_DIFF);
    for header in ["--- a/", "+++ b/"] {
        let expected = GIT_DIFF.lines().filter(|l| l.starts_with(header)).count();
        let got = out.lines().filter(|l| l.starts_with(header)).count();
        assert_eq!(expected, got, "{header} header count changed");
    }
    assert!(
        !out.contains("\nindex "),
        "index metadata should be dropped"
    );
}

#[test]
fn git_diff_summary_line_matches_real_counts() {
    let out = apply("git diff", GIT_DIFF);
    let files = GIT_DIFF
        .lines()
        .filter(|l| l.starts_with("diff --git"))
        .count();
    let added = GIT_DIFF
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    let removed = GIT_DIFF
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    let first = out.lines().next().unwrap();
    assert_eq!(first, format!("{files} files, +{added} -{removed}"));
}

#[test]
fn git_diff_wide_context_keeps_changes_and_collapses_hard() {
    let out = apply("git diff", GIT_DIFF_U10);
    let kept: HashSet<&str> = out.lines().collect();
    for line in GIT_DIFF_U10.lines() {
        let is_change = (line.starts_with('+') || line.starts_with('-'))
            && !line.starts_with("+++")
            && !line.starts_with("---");
        if is_change || line.starts_with("@@") {
            assert!(kept.contains(line), "lost line: {line:?}");
        }
    }
    // Regression guard: the -U10 fixture measures ~38% byte savings; hold
    // the line at >= 25% so future edits cannot silently regress it.
    assert!(
        out.len() * 4 <= GIT_DIFF_U10.len() * 3,
        "wide-context diff should shrink by at least 25%: {} -> {}",
        GIT_DIFF_U10.len(),
        out.len()
    );
}

#[test]
fn git_diff_actually_shrinks_the_fixture() {
    let out = apply("git diff", GIT_DIFF);
    assert!(
        out.len() < GIT_DIFF.len(),
        "condenser produced no savings: {} -> {}",
        GIT_DIFF.len(),
        out.len()
    );
    assert!(out.contains("lines unchanged"), "no context run collapsed");
}

#[test]
fn cargo_test_pass_fixture_collapses_ok_lines() {
    let out = apply("cargo test", CARGO_TEST_PASS);
    assert!(!out.contains("... ok"), "per-test ok lines survived: {out}");
    assert!(out.contains("✓ 23 passed"), "{out}");
    assert!(out.contains("test result: ok. 23 passed;"), "{out}");
    assert!(!out.contains("Compiling"), "{out}");
    assert!(!out.contains("running 23 tests"), "{out}");
}

#[test]
fn cargo_test_fail_fixture_keeps_the_whole_failure_surface() {
    let out = apply("cargo test", CARGO_TEST_FAIL);
    let kept: HashSet<&str> = out.lines().collect();
    // Every non-passing, non-noise line of the real failing run survives:
    // FAILED lines, panic locations, assertion left/right, failures: block,
    // summary, and cargo's final error line.
    for line in CARGO_TEST_FAIL.lines() {
        let is_ok_line = line.starts_with("test ") && line.ends_with("... ok");
        let is_noise = line.starts_with("   Compiling")
            || line.starts_with("    Finished")
            || line.starts_with("     Running")
            || line.starts_with("running ");
        if !is_ok_line && !is_noise && !line.trim().is_empty() {
            assert!(kept.contains(line), "lost failure line: {line:?}");
        }
    }
    assert!(out.contains("✓ 4 passed"), "{out}");
    assert!(
        out.contains("test result: FAILED. 7 passed; 2 failed; 1 ignored;"),
        "{out}"
    );
}

#[test]
fn rg_no_heading_fixture_keeps_filenames_and_linenos() {
    let out = apply("rg", RG_NO_HEADING);
    // Every kept match line must exist verbatim in the input.
    for line in out.lines() {
        if !line.starts_with('…') {
            assert!(
                RG_NO_HEADING.lines().any(|orig| orig == line),
                "fabricated line: {line:?}"
            );
        }
    }
    // Every file in the input still appears in the output.
    for line in RG_NO_HEADING.lines() {
        let file = line.split(':').next().unwrap();
        assert!(out.contains(file), "file vanished: {file}");
    }
    // Marker counts + kept lines add up to the original match count.
    let kept_matches = out.lines().filter(|l| !l.starts_with('…')).count();
    let hidden: usize = out
        .lines()
        .filter_map(|l| {
            l.strip_prefix("… +")
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|n| n.parse::<usize>().ok())
        })
        .sum();
    assert_eq!(kept_matches + hidden, RG_NO_HEADING.lines().count());
    assert!(hidden > 0, "fixture should trigger collapsing");
}

#[test]
fn rg_heading_fixture_keeps_every_file_heading() {
    let out = apply("rg", RG_HEADING);
    for line in RG_HEADING.lines() {
        let is_heading = !line.trim().is_empty() && !line.chars().next().unwrap().is_ascii_digit();
        if is_heading {
            assert!(out.contains(line), "heading vanished: {line:?}");
        }
    }
}
