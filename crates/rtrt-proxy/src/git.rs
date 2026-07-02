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

/// Condense unified-diff output while staying lossless for understanding the
/// change: every `diff --git` / `---` / `+++` / `@@` header and every `+` / `-`
/// line survives byte-identical. Only runs of unchanged context lines collapse
/// — change-adjacent boundary lines are kept and the rest becomes a
/// `… N lines unchanged` marker (see [`flush_context_run`]) — and pure
/// metadata (`index`, `similarity index`) is dropped. Mode changes, renames,
/// and binary markers are kept. Anything we cannot parse with certainty falls
/// back to the raw diff — hiding a changed line is never acceptable.
pub(crate) fn git_diff(input: &str) -> String {
    // `--stat` / `--name-only` / empty diffs carry no hunks — already compact.
    if !input.lines().any(|line| line.starts_with("@@")) {
        return input.to_string();
    }
    condense_unified_diff(input).unwrap_or_else(|| input.to_string())
}

/// Remaining line budget of the current hunk, taken from its `@@` header.
/// Counting lines off the header is the only unambiguous way to know when a
/// hunk ends: a removed line whose content starts with `--` is otherwise
/// indistinguishable from a `--- a/…` file header.
struct HunkBudget {
    old: usize,
    new: usize,
    seen_change: bool,
}

/// Where a run of unchanged context lines sits inside its hunk. Edge runs are
/// positional anchoring the `@@` header already encodes exactly, so only the
/// change-adjacent line is kept; interior runs keep both boundary lines.
enum ContextPosition {
    /// Between the `@@` header and the first changed line — keep the last line.
    Leading,
    /// Between two changed lines — keep first + last.
    Interior,
    /// After the last changed line until the hunk ends — keep the first line.
    Trailing,
}

impl HunkBudget {
    fn exhausted(&self) -> bool {
        self.old == 0 && self.new == 0
    }
}

fn condense_unified_diff(input: &str) -> Option<String> {
    let mut body = String::with_capacity(input.len());
    let mut git_headers = 0usize;
    let mut plus_headers = 0usize;
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut hunk: Option<HunkBudget> = None;
    let mut context: Vec<&str> = Vec::new();

    for line in input.lines() {
        if let Some(budget) = hunk.as_mut() {
            if !budget.exhausted() {
                if line.starts_with('+') || line.starts_with('-') || line.starts_with('\\') {
                    if line.starts_with('+') {
                        budget.new = budget.new.checked_sub(1)?;
                        added += 1;
                    } else if line.starts_with('-') {
                        budget.old = budget.old.checked_sub(1)?;
                        removed += 1;
                    }
                    // ('\\' — "No newline at end of file" — is semantically
                    // relevant and consumes no hunk budget.)
                    let position = if budget.seen_change {
                        ContextPosition::Interior
                    } else {
                        ContextPosition::Leading
                    };
                    budget.seen_change = true;
                    flush_context_run(&mut body, &mut context, position);
                    body.push_str(line);
                    body.push('\n');
                } else if line.starts_with(' ') || line.is_empty() {
                    budget.old = budget.old.checked_sub(1)?;
                    budget.new = budget.new.checked_sub(1)?;
                    context.push(line);
                } else {
                    // Unexpected line inside a hunk — bail out to the raw diff.
                    return None;
                }
                continue;
            }
            hunk = None;
        }

        // Outside any hunk: file headers and metadata. Any context still
        // buffered here trails the hunk that just ended.
        flush_context_run(&mut body, &mut context, ContextPosition::Trailing);
        if line.starts_with("@@") {
            hunk = Some(parse_hunk_budget(line)?);
            body.push_str(line);
            body.push('\n');
        } else if line.starts_with("diff --git ") {
            git_headers += 1;
            body.push_str(line);
            body.push('\n');
        } else if line.starts_with("index ")
            || line.starts_with("similarity index ")
            || line.starts_with("dissimilarity index ")
        {
            // Blob hashes / rename scores don't help in reading the change.
            // Mode changes (old mode/new mode/new file mode/deleted file mode)
            // and rename from/to fall through to the keep-branch below.
        } else {
            if line.starts_with("+++ ") {
                plus_headers += 1;
            }
            body.push_str(line);
            body.push('\n');
        }
    }
    flush_context_run(&mut body, &mut context, ContextPosition::Trailing);

    // Plain (non-git) unified diffs have no `diff --git` headers; count files
    // by their `+++` headers instead.
    let files = if git_headers > 0 {
        git_headers
    } else {
        plus_headers
    };
    let plural = if files == 1 { "" } else { "s" };
    let mut out = format!("{files} file{plural}, +{added} -{removed}\n");
    out.push_str(&body);
    Some(out)
}

/// Emit a buffered run of unchanged context lines. Interior runs keep the two
/// change-adjacent boundary lines; edge (leading/trailing) runs keep only the
/// single line touching the change, because the `@@` header already encodes
/// the exact positions the rest of the edge context would anchor. Hidden lines
/// are always accounted for by a `… N lines unchanged` marker, and the marker
/// only fires when it replaces at least two lines — shorter runs are kept
/// whole ("when in doubt, keep the line").
fn flush_context_run(out: &mut String, run: &mut Vec<&str>, position: ContextPosition) {
    let kept_edges = match position {
        ContextPosition::Interior => 2,
        ContextPosition::Leading | ContextPosition::Trailing => 1,
    };
    let hidden = run.len().saturating_sub(kept_edges);
    if hidden >= 2 {
        match position {
            ContextPosition::Leading => {
                out.push_str(&format!("… {hidden} lines unchanged\n"));
                out.push_str(run[run.len() - 1]);
                out.push('\n');
            }
            ContextPosition::Interior => {
                out.push_str(run[0]);
                out.push('\n');
                out.push_str(&format!("… {hidden} lines unchanged\n"));
                out.push_str(run[run.len() - 1]);
                out.push('\n');
            }
            ContextPosition::Trailing => {
                out.push_str(run[0]);
                out.push('\n');
                out.push_str(&format!("… {hidden} lines unchanged\n"));
            }
        }
    } else {
        for line in run.iter() {
            out.push_str(line);
            out.push('\n');
        }
    }
    run.clear();
}

/// Parse `@@ -l[,c] +l[,c] @@ …` into remaining old/new line counts. Combined
/// diffs (`@@@`, merge conflicts) intentionally fail here so the caller keeps
/// the raw output.
fn parse_hunk_budget(header: &str) -> Option<HunkBudget> {
    let rest = header.strip_prefix("@@ -")?;
    let (old_range, rest) = rest.split_once(" +")?;
    let (new_range, _) = rest.split_once(" @@")?;
    Some(HunkBudget {
        old: parse_range_count(old_range)?,
        new: parse_range_count(new_range)?,
        seen_change: false,
    })
}

fn parse_range_count(range: &str) -> Option<usize> {
    match range.split_once(',') {
        Some((start, count)) => {
            start.parse::<usize>().ok()?;
            count.parse().ok()
        }
        None => {
            range.parse::<usize>().ok()?;
            Some(1)
        }
    }
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

    const DIFF: &str = concat!(
        "diff --git a/src/lib.rs b/src/lib.rs\n",
        "index e3f074e..6982fdf 100644\n",
        "--- a/src/lib.rs\n",
        "+++ b/src/lib.rs\n",
        "@@ -1,9 +1,9 @@\n",
        " fn one() {}\n",
        " fn two() {}\n",
        " fn three() {}\n",
        " fn four() {}\n",
        "-fn five_old() {}\n",
        "+fn five_new() {}\n",
        " fn six() {}\n",
        " fn seven() {}\n",
        " fn eight() {}\n",
        " fn nine() {}\n",
    );

    #[test]
    fn git_diff_adds_summary_and_collapses_edge_context_runs() {
        let out = git_diff(DIFF);
        assert!(out.starts_with("1 file, +1 -1\n"), "{out}");
        // Leading run of 4: only the change-adjacent line survives; the `@@`
        // header still pins the exact positions of the hidden lines.
        assert!(out.contains("… 3 lines unchanged\n fn four() {}"), "{out}");
        assert!(!out.contains("fn one() {}"), "{out}");
        assert!(!out.contains("fn two() {}"), "{out}");
        // Trailing run of 4: only the change-adjacent line survives.
        assert!(out.contains(" fn six() {}\n… 3 lines unchanged"), "{out}");
        assert!(!out.contains("fn nine() {}"), "{out}");
        // Changed lines byte-identical, headers kept, index dropped.
        assert!(out.contains("-fn five_old() {}"), "{out}");
        assert!(out.contains("+fn five_new() {}"), "{out}");
        assert!(
            out.contains("diff --git a/src/lib.rs b/src/lib.rs"),
            "{out}"
        );
        assert!(out.contains("--- a/src/lib.rs"), "{out}");
        assert!(out.contains("+++ b/src/lib.rs"), "{out}");
        assert!(out.contains("@@ -1,9 +1,9 @@"), "{out}");
        assert!(!out.contains("index e3f074e"), "{out}");
    }

    #[test]
    fn git_diff_interior_runs_keep_both_boundary_lines() {
        let raw = concat!(
            "diff --git a/f b/f\n",
            "--- a/f\n",
            "+++ b/f\n",
            "@@ -1,7 +1,7 @@\n",
            "-old\n",
            "+new\n",
            " one\n",
            " two\n",
            " three\n",
            " four\n",
            "-old2\n",
            "+new2\n",
        );
        let out = git_diff(raw);
        assert!(
            out.contains(" one\n… 2 lines unchanged\n four\n"),
            "interior run must keep first+last: {out}"
        );
    }

    #[test]
    fn git_diff_short_context_runs_are_kept_whole() {
        // Runs whose marker would hide fewer than 2 lines are kept verbatim:
        // a leading run of 2 and an interior run of 3.
        let raw = concat!(
            "diff --git a/f b/f\n",
            "--- a/f\n",
            "+++ b/f\n",
            "@@ -1,8 +1,8 @@\n",
            " a\n",
            " b\n",
            "-old\n",
            "+new\n",
            " c\n",
            " d\n",
            " e\n",
            "-old2\n",
            "+new2\n",
        );
        let out = git_diff(raw);
        assert!(out.contains(" a\n b\n-old"), "{out}");
        assert!(out.contains(" c\n d\n e\n-old2"), "{out}");
        assert!(!out.contains("unchanged"), "{out}");
    }

    #[test]
    fn git_diff_keeps_mode_change_rename_and_no_newline_marker() {
        let raw = "diff --git a/tool b/tool\n\
old mode 100644\n\
new mode 100755\n\
diff --git a/old.rs b/new.rs\n\
similarity index 97%\n\
rename from old.rs\n\
rename to new.rs\n\
index abc..def 100644\n\
--- a/old.rs\n\
+++ b/new.rs\n\
@@ -1,1 +1,1 @@\n\
-x\n\
+y\n\
\\ No newline at end of file\n";
        let out = git_diff(raw);
        assert!(out.contains("old mode 100644"), "{out}");
        assert!(out.contains("new mode 100755"), "{out}");
        assert!(out.contains("rename from old.rs"), "{out}");
        assert!(out.contains("rename to new.rs"), "{out}");
        assert!(out.contains("\\ No newline at end of file"), "{out}");
        assert!(!out.contains("similarity index"), "{out}");
        assert!(!out.contains("index abc..def"), "{out}");
        assert!(out.starts_with("2 files, +1 -1\n"), "{out}");
    }

    #[test]
    fn git_diff_removed_line_starting_with_dashes_is_not_a_header() {
        // "--- foo" inside a hunk is a removed line whose content starts with
        // "-- foo"; budget counting must keep it intact and end the hunk at
        // the right line.
        let raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,1 @@\n--- not a header\n aaa\n";
        let out = git_diff(raw);
        assert!(out.contains("--- not a header"), "{out}");
        assert!(out.starts_with("1 file, +0 -1\n"), "{out}");
    }

    #[test]
    fn git_diff_stat_and_name_only_pass_through() {
        let stat = " src/lib.rs | 5 +++--\n 1 file changed, 3 insertions(+), 2 deletions(-)\n";
        assert_eq!(git_diff(stat), stat);
        let names = "src/lib.rs\nsrc/main.rs\n";
        assert_eq!(git_diff(names), names);
        assert_eq!(git_diff(""), "");
    }

    #[test]
    fn git_diff_combined_merge_diff_falls_back_to_raw() {
        let raw = "diff --cc conflicted.rs\n\
index abc,def..123\n\
--- a/conflicted.rs\n\
+++ b/conflicted.rs\n\
@@@ -1,2 -1,2 +1,2 @@@\n\
  shared\n\
++merged\n";
        assert_eq!(git_diff(raw), raw);
    }
}
