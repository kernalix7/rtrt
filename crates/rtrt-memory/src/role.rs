//! Coarse INPUT / OUTPUT split over a memory row's `kind`.
//!
//! Input rows are the user's own typed prompts — a tiny fraction of total
//! volume next to everything an agent produces (assistant turns, teammate
//! messages, stop/subagent-stop captures, tool-batch summaries, …), so
//! row-level views and the memory map read as "only output" by default. This
//! module is the single source of truth for the INPUT/OUTPUT kind grouping
//! so the dashboard's All/Input/Output role filter (memory timeline + search)
//! means the same thing everywhere it's applied — SQL `WHERE` clauses,
//! in-process post-filtering, and the `role` query/body param itself.

/// `kind` values that represent the user's own typed prompts.
pub const INPUT_KINDS: &[&str] = &["user-prompt-submit", "user-prompt-expansion"];

/// True when `kind` is one of [`INPUT_KINDS`] — the user's own prompt, not
/// agent-produced output.
pub fn is_input_kind(kind: &str) -> bool {
    INPUT_KINDS.contains(&kind)
}

/// Normalises a `role` query/body param to `"input"` / `"output"`. Anything
/// else — absent, empty, `"all"`, or an unrecognised value — means "no
/// restriction" and is returned as `None` so callers treat garbage input the
/// same as "all" rather than erroring.
pub fn normalize_role(role: Option<&str>) -> Option<&'static str> {
    match role.map(str::trim) {
        Some("input") => Some("input"),
        Some("output") => Some("output"),
        _ => None,
    }
}

/// True when `kind` belongs to `role` (`"input"` / `"output"`; `None` or an
/// unrecognised value matches everything).
pub fn role_matches(role: Option<&str>, kind: &str) -> bool {
    match normalize_role(role) {
        Some("input") => is_input_kind(kind),
        Some("output") => !is_input_kind(kind),
        _ => true,
    }
}

/// SQL predicate fragment (anonymous `?` placeholders) + the values to bind,
/// for a `WHERE ... AND ({clause})` query assembled with a manual
/// `Vec<&dyn rusqlite::ToSql>` (positions must match the fragment's `?`
/// order). Returns `("1=1", [])` for "all" — always parameterized, never
/// string-interpolates the `kind` values into the query text.
pub fn role_sql_clause(role: Option<&str>) -> (String, Vec<&'static str>) {
    let placeholders = std::iter::repeat_n("?", INPUT_KINDS.len())
        .collect::<Vec<_>>()
        .join(",");
    match normalize_role(role) {
        Some("input") => (format!("kind IN ({placeholders})"), INPUT_KINDS.to_vec()),
        Some("output") => (
            format!("kind NOT IN ({placeholders})"),
            INPUT_KINDS.to_vec(),
        ),
        _ => ("1=1".to_string(), Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_kinds_are_recognised() {
        assert!(is_input_kind("user-prompt-submit"));
        assert!(is_input_kind("user-prompt-expansion"));
        assert!(!is_input_kind("assistant-turn"));
        assert!(!is_input_kind("teammate-message"));
        assert!(!is_input_kind("stop"));
        assert!(!is_input_kind("subagent-stop"));
        assert!(!is_input_kind("post-tool-batch"));
    }

    #[test]
    fn normalize_role_accepts_only_known_values() {
        assert_eq!(normalize_role(Some("input")), Some("input"));
        assert_eq!(normalize_role(Some("output")), Some("output"));
        assert_eq!(normalize_role(Some("all")), None);
        assert_eq!(normalize_role(Some("")), None);
        assert_eq!(normalize_role(None), None);
        assert_eq!(normalize_role(Some("bogus")), None);
    }

    #[test]
    fn role_matches_input_and_output() {
        assert!(role_matches(Some("input"), "user-prompt-submit"));
        assert!(!role_matches(Some("input"), "assistant-turn"));
        assert!(role_matches(Some("output"), "assistant-turn"));
        assert!(!role_matches(Some("output"), "user-prompt-submit"));
        assert!(role_matches(None, "assistant-turn"));
        assert!(role_matches(None, "user-prompt-submit"));
        // Unrecognised value behaves like "all", never like an empty filter.
        assert!(role_matches(Some("bogus"), "assistant-turn"));
    }

    #[test]
    fn sql_clause_shapes() {
        let (clause, vals) = role_sql_clause(None);
        assert_eq!(clause, "1=1");
        assert!(vals.is_empty());
        let (clause, vals) = role_sql_clause(Some("input"));
        assert_eq!(clause, "kind IN (?,?)");
        assert_eq!(vals, INPUT_KINDS);
        let (clause, vals) = role_sql_clause(Some("output"));
        assert_eq!(clause, "kind NOT IN (?,?)");
        assert_eq!(vals, INPUT_KINDS);
    }
}
