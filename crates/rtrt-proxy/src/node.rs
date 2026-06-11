use once_cell::sync::Lazy;
use regex::Regex;

use crate::{collapse_blanks, is_error_or_warning};

static NODE_PROGRESS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^\s*(added|removed|changed|audited|found 0 vulnerabilities|up to date|Progress:|Packages:|Done in|Lockfile is up to date)",
    )
    .unwrap()
});

pub(crate) fn node_filter(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let trimmed = line.trim();
        if (NODE_PROGRESS.is_match(trimmed)
            || trimmed.starts_with("npm notice")
            || trimmed.starts_with("pnpm: ")
            || trimmed.contains("WARN deprecated"))
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

pub(crate) fn node_diagnostics(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let trimmed = line.trim();
        let is_noise = trimmed == "Done"
            || trimmed == "All matched files use Prettier code style!"
            || trimmed.starts_with("Checked ")
            || trimmed.starts_with("Found 0 errors");
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
    fn npm_drops_install_progress_and_keeps_errors() {
        let raw = "added 128 packages\nfound 0 vulnerabilities\nnpm ERR! code ERESOLVE\nerror: dependency conflict\n";
        let out = node_filter(raw);
        assert!(!out.contains("added 128"));
        assert!(!out.contains("found 0"));
        assert!(out.contains("npm ERR!"));
        assert!(out.contains("dependency conflict"));
    }

    #[test]
    fn tsc_diagnostics_keep_errors() {
        let raw =
            "Found 0 errors\nsrc/index.ts:1:1 - error TS2322: Type 'string' is not assignable\n";
        let out = node_diagnostics(raw);
        assert!(!out.contains("Found 0 errors"));
        assert!(out.contains("error TS2322"));
    }
}
