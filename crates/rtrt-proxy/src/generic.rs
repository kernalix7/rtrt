use once_cell::sync::Lazy;
use regex::Regex;

use crate::{collapse_repeated_adjacent, is_error_or_warning};

static CURL_PROGRESS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\s*(% Total|[0-9]{1,3}\s+[0-9A-Za-z.-]+\s+[0-9A-Za-z.-]+|-=O=-)").unwrap()
});

static PYTEST_NOISE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^=+ (test session starts|warnings summary|slowest durations) =+$").unwrap()
});

pub fn collapse_blanks(input: &str) -> String {
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

pub(crate) fn passthrough(input: &str) -> String {
    input.to_string()
}

pub(crate) fn ls_passthrough(input: &str) -> String {
    collapse_blanks(input)
}

pub(crate) fn ls_long(input: &str) -> String {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut parsed_any = false;

    for line in input.lines() {
        if line.trim().is_empty() || line.starts_with("total ") {
            continue;
        }

        match parse_ls_long_line(line) {
            Some(entry) => {
                parsed_any = true;
                if entry.is_dir {
                    dirs.push(entry.rendered);
                } else {
                    files.push(entry.rendered);
                }
            }
            None => files.push(line.to_string()),
        }
    }

    if !parsed_any {
        return input.to_string();
    }

    let mut out = String::new();
    for entry in dirs.into_iter().chain(files) {
        out.push_str(&entry);
        out.push('\n');
    }
    out
}

struct LsEntry {
    is_dir: bool,
    rendered: String,
}

fn parse_ls_long_line(line: &str) -> Option<LsEntry> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 9 {
        return None;
    }

    let perms = parts[0];
    if perms.len() < 10 {
        return None;
    }

    let size = parts[4];
    let name = parts[8..].join(" ");
    let prefix = match perms.as_bytes().first() {
        Some(b'd') => "dir ",
        Some(b'l') => "link ",
        Some(_) => "",
        None => return None,
    };

    Some(LsEntry {
        is_dir: perms.starts_with('d'),
        rendered: format!("{prefix}{size} {name}"),
    })
}

/// `grep` / `rg` filter: group consecutive matches per file and collapse
/// groups that exceed a data-derived per-file cap (`max(3, √total-matches)`)
/// to the first `cap` matches plus a `… +N more in <file>` marker. Filenames
/// and line numbers of every kept match survive byte-identical. Formats we
/// cannot parse with certainty (context output with `--` separators, plain
/// stdin matches without `file:line:` prefixes) fall back to the previous
/// behavior so no match is ever silently mangled.
pub(crate) fn grep_filter(input: &str) -> String {
    if let Some(out) = collapse_match_groups(input) {
        return out;
    }
    if input.lines().count() > derived_huge_output_threshold(input) {
        collapse_repeated_adjacent(input)
    } else {
        input.to_string()
    }
}

/// `path:line:` prefix of grep/rg `-n` output (`--no-heading` / piped form).
static MATCH_PREFIX: Lazy<Regex> = Lazy::new(|| Regex::new(r"^([^:\n]+):\d+:").unwrap());

/// `line:` prefix of a match line under `rg --heading`.
static HEADING_MATCH: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\d+:").unwrap());

/// One file's worth of consecutive output lines: an optional heading line
/// (rg `--heading` format) plus its match lines.
struct MatchGroup<'a> {
    file: &'a str,
    heading: Option<&'a str>,
    matches: Vec<&'a str>,
}

fn collapse_match_groups(input: &str) -> Option<String> {
    // `-C`/`-A`/`-B` context output interleaves `--` separators and
    // `path-line-` context lines with the matches; collapsing there could
    // orphan context from its match, so leave it alone.
    if input.lines().any(|line| line == "--") {
        return None;
    }

    let groups = parse_no_heading_groups(input).or_else(|| parse_heading_groups(input))?;
    let total: usize = groups.iter().map(|group| group.matches.len()).sum();
    let cap = derived_per_file_cap(total);
    if groups.iter().all(|group| group.matches.len() <= cap) {
        return Some(input.to_string());
    }

    let mut out = String::with_capacity(input.len());
    for group in &groups {
        if let Some(heading) = group.heading {
            out.push_str(heading);
            out.push('\n');
        }
        let shown = group.matches.len().min(cap);
        for line in &group.matches[..shown] {
            out.push_str(line);
            out.push('\n');
        }
        if shown < group.matches.len() {
            let hidden = group.matches.len() - shown;
            out.push_str(&format!("… +{hidden} more in {}\n", group.file));
        }
        if group.heading.is_some() {
            out.push('\n');
        }
    }
    Some(out)
}

/// Per-file display cap derived from the total number of matches in the
/// output (~√-scaled with a floor of 3), so small outputs stay untouched and
/// pathological single-file repetition collapses hard.
fn derived_per_file_cap(total_matches: usize) -> usize {
    3.max((total_matches as f64).sqrt() as usize)
}

/// Strict parse of `path:line:content` output. Every non-empty line must
/// match (binary-match notices pass through as their own group); otherwise
/// the format is not certain enough to collapse.
fn parse_no_heading_groups(input: &str) -> Option<Vec<MatchGroup<'_>>> {
    let mut groups: Vec<MatchGroup<'_>> = Vec::new();
    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with("Binary file ") && line.ends_with(" matches") {
            groups.push(MatchGroup {
                file: line,
                heading: None,
                matches: vec![line],
            });
            continue;
        }
        let captures = MATCH_PREFIX.captures(line)?;
        let file = captures.get(1)?.as_str();
        match groups.last_mut() {
            Some(group) if group.file == file && group.heading.is_none() => {
                group.matches.push(line);
            }
            _ => groups.push(MatchGroup {
                file,
                heading: None,
                matches: vec![line],
            }),
        }
    }
    if groups.is_empty() {
        None
    } else {
        Some(groups)
    }
}

/// Strict parse of `rg --heading -n` output: a path line followed by `line:`
/// match lines, groups separated by blank lines.
fn parse_heading_groups(input: &str) -> Option<Vec<MatchGroup<'_>>> {
    let mut groups: Vec<MatchGroup<'_>> = Vec::new();
    let mut expecting_heading = true;
    for line in input.lines() {
        if line.trim().is_empty() {
            expecting_heading = true;
            continue;
        }
        if HEADING_MATCH.is_match(line) {
            // A match line with no heading above it — not heading format.
            groups.last_mut()?.matches.push(line);
        } else {
            if !expecting_heading {
                return None;
            }
            groups.push(MatchGroup {
                file: line,
                heading: Some(line),
                matches: Vec::new(),
            });
            expecting_heading = false;
        }
    }
    let valid = !groups.is_empty() && groups.iter().all(|group| !group.matches.is_empty());
    if valid { Some(groups) } else { None }
}

fn derived_huge_output_threshold(input: &str) -> usize {
    let byte_width = input.len().max(1);
    let line_width = input.lines().map(str::len).max().unwrap_or_default().max(1);
    byte_width / line_width
}

pub(crate) fn find_filter(input: &str) -> String {
    collapse_blanks(input)
}

pub(crate) fn http_client_filter(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        if CURL_PROGRESS.is_match(line) && !is_error_or_warning(line) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if out.trim().is_empty() {
        input.to_string()
    } else {
        collapse_blanks(&out)
    }
}

pub(crate) fn gh_filter(input: &str) -> String {
    // In `gh pr checks` / `gh run view`, "X " marks a FAILING check and "✓"
    // a passing one. Failures are the whole point of reading the output, so
    // they are always kept verbatim; the passing lines are pure noise and
    // collapse into a single "✓ N passed" summary.
    let mut out = String::new();
    let mut passed = 0usize;
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('✓') {
            passed += 1;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if passed > 0 {
        out.push_str(&format!("✓ {passed} passed\n"));
    }
    collapse_blanks(&out)
}

pub(crate) fn pytest_filter(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let trimmed = line.trim();
        if (PYTEST_NOISE.is_match(trimmed)
            || trimmed.starts_with("collecting ")
            || trimmed.starts_with("collected "))
            && !is_error_or_warning(trimmed)
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    collapse_blanks(&out)
}

pub(crate) fn go_test_filter(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let trimmed = line.trim();
        if (trimmed == "PASS" || trimmed.starts_with("ok  \t") || trimmed.starts_with("?   \t"))
            && !is_error_or_warning(trimmed)
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

pub(crate) fn pip_filter(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let trimmed = line.trim();
        let is_noise = trimmed.starts_with("Collecting ")
            || trimmed.starts_with("Downloading ")
            || trimmed.starts_with("Installing collected packages")
            || trimmed.starts_with("Successfully installed")
            || trimmed.starts_with("Requirement already satisfied");
        if is_noise && !is_error_or_warning(trimmed) {
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
    fn ls_long_drops_permissions_owner_group_date_and_dirs_first() {
        let raw = "total 8\ndrwxr-xr-x  2 me staff 4.0K Jun  1 10:00 src\n-rw-r--r--  1 me staff  123 Jun  1 10:00 Cargo.toml\nlrwxrwxrwx  1 me staff    3 Jun  1 10:00 link -> src\n";
        let out = ls_long(raw);
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some("dir 4.0K src"));
        assert!(out.contains("123 Cargo.toml"));
        assert!(out.contains("link 3 link -> src"));
        assert!(!out.contains("drwxr-xr-x"));
        assert!(!out.contains("me staff"));
        assert!(!out.contains("Jun"));
    }

    #[test]
    fn grep_small_output_passes_through_unchanged() {
        let raw = "src/lib.rs:1:error: real\nsrc/main.rs:2:warning: also real\n";
        assert_eq!(grep_filter(raw), raw);
    }

    #[test]
    fn grep_collapses_heavy_files_and_keeps_filenames_and_linenos() {
        // 25 matches in one file + 2 in another → cap = max(3, √27) = 5.
        let mut raw = String::new();
        for i in 1..=25 {
            raw.push_str(&format!("src/big.rs:{i}:let x = {i};\n"));
        }
        raw.push_str("src/small.rs:7:let y = 7;\n");
        raw.push_str("src/small.rs:9:let z = 9;\n");
        let out = grep_filter(&raw);
        // First cap matches survive byte-identical.
        assert!(out.contains("src/big.rs:1:let x = 1;"), "{out}");
        assert!(out.contains("src/big.rs:5:let x = 5;"), "{out}");
        assert!(!out.contains("src/big.rs:6:"), "{out}");
        assert!(out.contains("… +20 more in src/big.rs"), "{out}");
        // Under-cap files are untouched.
        assert!(out.contains("src/small.rs:7:let y = 7;"), "{out}");
        assert!(out.contains("src/small.rs:9:let z = 9;"), "{out}");
        assert!(!out.contains("more in src/small.rs"), "{out}");
    }

    #[test]
    fn grep_heading_format_collapses_per_file() {
        let mut raw = String::from("src/big.rs\n");
        for i in 1..=30 {
            raw.push_str(&format!("{i}:let x = {i};\n"));
        }
        raw.push_str("\nsrc/small.rs\n3:let y = 3;\n");
        let out = grep_filter(&raw);
        // cap = max(3, √31) = 5; heading and line numbers survive.
        assert!(out.contains("src/big.rs\n1:let x = 1;"), "{out}");
        assert!(out.contains("5:let x = 5;"), "{out}");
        assert!(!out.contains("\n6:let x = 6;"), "{out}");
        assert!(out.contains("… +25 more in src/big.rs"), "{out}");
        assert!(out.contains("src/small.rs\n3:let y = 3;"), "{out}");
    }

    #[test]
    fn grep_context_output_is_never_collapsed() {
        // `-C` output carries `--` separators; collapsing could orphan
        // context from its match, so it must pass through.
        let mut raw = String::new();
        for i in 1..=30 {
            raw.push_str(&format!("src/a.rs:{i}:match {i}\n"));
            raw.push_str("--\n");
        }
        let out = grep_filter(&raw);
        assert!(out.contains("src/a.rs:30:match 30"), "{out}");
    }

    #[test]
    fn grep_unparseable_output_falls_back_without_losing_lines() {
        let raw = "plain match one\nplain match two\nplain match three\n";
        assert_eq!(grep_filter(raw), raw);
    }

    #[test]
    fn http_clients_drop_progress_and_keep_errors() {
        let raw = "% Total    % Received % Xferd\n100  1024  100  1024\ncurl: (22) error: failed request\n{\"ok\":false}\n";
        let out = http_client_filter(raw);
        assert!(!out.contains("% Total"));
        assert!(out.contains("failed request"));
        assert!(out.contains("{\"ok\":false}"));
    }

    #[test]
    fn pytest_keeps_failure_sections() {
        let raw = "================ test session starts ================\ncollecting ...\ncollected 2 items\nFAILED tests/test_a.py::test_a - AssertionError\nerror: boom\n";
        let out = pytest_filter(raw);
        assert!(!out.contains("test session starts"));
        assert!(!out.contains("collected 2"));
        assert!(out.contains("FAILED tests/test_a.py"));
        assert!(out.contains("error: boom"));
    }

    #[test]
    fn go_test_returns_ok_for_success_noise_only() {
        let raw = "ok  \texample.com/project\t0.012s\nPASS\n";
        assert_eq!(go_test_filter(raw), "ok\n");
    }

    #[test]
    fn gh_filter_keeps_failing_checks_and_summarizes_passes() {
        // "X" marks a FAILING check — dropping those lines hid the only
        // information the caller cares about.
        let raw = "✓ build (1m2s)\n✓ lint (12s)\nX test (45s)\nX docs (3s)\n";
        let out = gh_filter(raw);
        assert!(out.contains("X test (45s)"), "{out}");
        assert!(out.contains("X docs (3s)"), "{out}");
        assert!(!out.contains("✓ build"), "{out}");
        assert!(!out.contains("✓ lint"), "{out}");
        assert!(out.contains("✓ 2 passed"), "{out}");
    }

    #[test]
    fn gh_filter_all_passing_collapses_to_summary() {
        let raw = "✓ build (1m2s)\n✓ lint (12s)\n✓ test (45s)\n";
        assert_eq!(gh_filter(raw).trim(), "✓ 3 passed");
    }

    #[test]
    fn gh_filter_passes_through_non_check_lines() {
        let raw = "Some checks were not successful\nX test (45s)\n";
        let out = gh_filter(raw);
        assert!(out.contains("Some checks were not successful"), "{out}");
        assert!(out.contains("X test (45s)"), "{out}");
    }
}
