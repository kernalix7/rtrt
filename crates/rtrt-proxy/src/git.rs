use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::BTreeSet;

use crate::collapse_blanks;

static GIT_STATUS_NOISE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?m)^(\s*\(use .*\)|nothing to commit.*|no changes added to commit.*|Untracked files:|Changes to be committed:|Changes not staged for commit:)$",
    )
    .unwrap()
});

static GIT_LOG_FMT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^(Author: .*|Date: .*)\n").unwrap());

pub(crate) fn git_status(input: &str) -> String {
    if input.trim().is_empty() {
        return "ok\n".to_string();
    }

    if input.lines().any(|line| line.starts_with("##")) || looks_like_short_status(input) {
        return short_status(input);
    }

    let mut branch = None;
    let mut tracking = None;
    let mut changes = Vec::new();

    for line in input.lines() {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix("On branch ") {
            branch = Some(name.to_string());
            continue;
        }
        if trimmed.starts_with("Your branch is") {
            tracking = parse_tracking(trimmed);
            continue;
        }
        if GIT_STATUS_NOISE.is_match(trimmed)
            || trimmed.is_empty()
            || trimmed.starts_with("use ")
            || trimmed.starts_with("(use ")
        {
            continue;
        }
        if is_status_change(trimmed) {
            changes.push(trimmed.to_string());
        }
    }

    let mut out = String::new();
    if let Some(branch) = branch {
        out.push('*');
        out.push(' ');
        out.push_str(&branch);
        if let Some(tracking) = tracking {
            out.push_str("...");
            out.push_str(&tracking);
        }
        out.push('\n');
    }

    if changes.is_empty() {
        out.push_str("clean\n");
    } else {
        out.push_str(&format!("dirty: {} files\n", changes.len()));
        for change in changes {
            out.push_str(&change);
            out.push('\n');
        }
    }

    out
}

fn parse_tracking(line: &str) -> Option<String> {
    line.split('\'').nth(1).map(ToOwned::to_owned)
}

fn looks_like_short_status(input: &str) -> bool {
    input.lines().all(|line| {
        let trimmed = line.trim_end();
        trimmed.is_empty()
            || trimmed.starts_with("##")
            || trimmed.len() > 3
                && matches!(
                    &trimmed[..2],
                    " M" | "M "
                        | "MM"
                        | " A"
                        | "A "
                        | " D"
                        | "D "
                        | "R "
                        | " R"
                        | "C "
                        | " C"
                        | "??"
                        | "!!"
                        | "UU"
                        | "UD"
                        | "DU"
                        | "AA"
                        | "DD"
                )
                && trimmed.as_bytes()[2].is_ascii_whitespace()
    })
}

fn short_status(input: &str) -> String {
    let mut out = String::new();
    let mut changes = Vec::new();

    for line in input.lines() {
        if let Some(branch) = line.strip_prefix("## ") {
            out.push('*');
            out.push(' ');
            out.push_str(branch);
            out.push('\n');
        } else if !line.trim().is_empty() {
            changes.push(line);
        }
    }

    if changes.is_empty() {
        if out.is_empty() {
            out.push_str("ok\n");
        } else {
            out.push_str("clean\n");
        }
        return out;
    }

    out.push_str(&format!("dirty: {} files\n", changes.len()));
    for change in changes {
        out.push_str(change);
        out.push('\n');
    }
    out
}

fn is_status_change(line: &str) -> bool {
    line.starts_with("modified:")
        || line.starts_with("new file:")
        || line.starts_with("deleted:")
        || line.starts_with("renamed:")
        || line.starts_with("copied:")
        || line.starts_with("both modified:")
        || line.starts_with("both added:")
        || line.starts_with("both deleted:")
        || line.starts_with("added by us:")
        || line.starts_with("deleted by them:")
        || line.starts_with("deleted by us:")
        || line.starts_with("added by them:")
        || line.starts_with("typechange:")
        || looks_like_untracked_file(line)
}

fn looks_like_untracked_file(line: &str) -> bool {
    !line.contains(':')
        && !line.starts_with("On branch ")
        && !line.starts_with("Your branch ")
        && !line.starts_with("Changes ")
        && !line.starts_with("Untracked ")
        && !line.starts_with("no changes ")
}

pub(crate) fn git_passthrough(input: &str) -> String {
    input.to_string()
}

pub(crate) fn git_show(input: &str) -> String {
    collapse_blanks(input)
}

pub(crate) fn git_stash(input: &str) -> String {
    collapse_blanks(input)
}

pub(crate) fn git_log(input: &str) -> String {
    if !input.lines().any(|line| {
        line.starts_with("Author: ") || line.starts_with("Date: ") || line.trim().is_empty()
    }) {
        return input.to_string();
    }

    let stripped = GIT_LOG_FMT.replace_all(input, "");
    collapse_blanks(&stripped)
}

pub(crate) fn git_branch(input: &str) -> String {
    let mut current = None;
    let mut locals = BTreeSet::new();
    let mut remotes = BTreeSet::new();

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.contains("HEAD ->") {
            continue;
        }

        let is_current = trimmed.starts_with('*');
        let name = trimmed.trim_start_matches('*').trim();
        if let Some(remote) = normalize_remote_branch(name) {
            remotes.insert(remote);
        } else if is_current {
            current = Some(name.to_string());
        } else {
            locals.insert(name.to_string());
        }
    }

    if let Some(current) = &current {
        locals.remove(current);
    }

    let local_names: BTreeSet<String> = current
        .iter()
        .cloned()
        .chain(locals.iter().cloned())
        .collect();
    let remote_only: Vec<String> = remotes
        .into_iter()
        .filter(|remote| !local_names.contains(remote))
        .collect();
    let total = local_names.len() + remote_only.len();
    let cap = derived_display_cap(total);

    let mut out = String::new();
    if let Some(current) = current {
        out.push_str("* ");
        out.push_str(&current);
        out.push('\n');
    }
    for local in locals {
        out.push_str("  ");
        out.push_str(&local);
        out.push('\n');
    }

    if !remote_only.is_empty() {
        out.push_str("remote-only:\n");
        let show_count = if total <= cap {
            remote_only.len()
        } else {
            cap.saturating_sub(local_names.len()).min(remote_only.len())
        };
        for remote in remote_only.iter().take(show_count) {
            out.push_str("  ");
            out.push_str(remote);
            out.push('\n');
        }
        if show_count < remote_only.len() {
            out.push_str(&format!("... +{} more\n", remote_only.len() - show_count));
        }
    }

    if out.is_empty() {
        input.to_string()
    } else {
        out
    }
}

fn normalize_remote_branch(name: &str) -> Option<String> {
    name.strip_prefix("remotes/origin/")
        .or_else(|| name.strip_prefix("origin/"))
        .map(ToOwned::to_owned)
}

fn derived_display_cap(total: usize) -> usize {
    // Show all branches when total <= max(3, sqrt(total) * 2); otherwise show
    // that data-derived cap and append "... +N more" for the hidden entries.
    3.max((total as f64).sqrt() as usize * 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_status_condenses_branch_tracking_and_dirty_summary() {
        let raw = "On branch main\nYour branch is up to date with 'origin/main'.\n\nChanges not staged for commit:\n  (use \"git add <file>...\" to update what will be committed)\n\tmodified:   src/lib.rs\n\nnothing to commit\n";
        let out = git_status(raw);
        assert!(out.contains("* main...origin/main"));
        assert!(out.contains("dirty: 1 files"));
        assert!(out.contains("modified:   src/lib.rs"));
        assert!(!out.contains("On branch"));
        assert!(!out.contains("use \"git add"));
    }

    #[test]
    fn git_status_short_clean_emits_ok() {
        assert_eq!(git_status(""), "ok\n");
    }

    #[test]
    fn git_branch_strips_origin_and_summarizes_remote_only() {
        let raw = "* main\n  dev\n  remotes/origin/HEAD -> origin/main\n  remotes/origin/main\n  remotes/origin/dev\n  remotes/origin/feature/a\n  remotes/origin/feature/b\n  remotes/origin/feature/c\n  remotes/origin/feature/d\n  remotes/origin/feature/e\n";
        let out = git_branch(raw);
        assert!(out.contains("* main"));
        assert!(out.contains("  dev"));
        assert!(out.contains("remote-only:"));
        assert!(out.contains("feature/a"));
        assert!(out.contains("... +"));
        assert!(!out.contains("remotes/origin/"));
    }

    #[test]
    fn git_log_oneline_passes_through() {
        let raw = "abc1234 first commit\ndef5678 second commit\n";
        assert_eq!(git_log(raw), raw);
    }
}
