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

pub(crate) fn grep_filter(input: &str) -> String {
    if input.lines().count() > derived_huge_output_threshold(input) {
        collapse_repeated_adjacent(input)
    } else {
        input.to_string()
    }
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
    let mut out = String::new();
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("X ") && !is_error_or_warning(trimmed) && !trimmed.contains("failed")
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
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
    fn grep_rn_style_output_can_pass_through_unchanged() {
        let raw = "src/lib.rs:1:error: real\nsrc/main.rs:2:warning: also real\n";
        assert_eq!(passthrough(raw), raw);
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
}
