//! Classifier for machine-generated "capture bucket" project names.
//!
//! Every memory row lives under a `project` bucket. Almost all buckets are
//! real project names (a repo basename, `default`, a person's handle). A
//! small minority are machine-generated buckets that a Task-tool subagent,
//! workflow runner, or the transcript watcher created before the
//! reattribution pass could fold them under their real project (see
//! `rtrt-dashboard/src/transcripts.rs`). Reattribution needs the source
//! transcript on disk; once that file is deleted, a stray bucket can never
//! resolve to a parent and sits in the project selector forever.
//!
//! [`is_capture_bucket_name`] recognises ONLY the handful of shapes rtrt
//! itself generates. It is deliberately conservative: a false positive would
//! hide a real project from the selector, so every pattern here matches the
//! WHOLE name against an unambiguously machine-shaped form. Anything that
//! isn't one of those exact shapes — including names that merely contain a
//! hex-looking substring, or start with a letter that happens to be `p` or
//! `a` — is left alone.

use std::sync::LazyLock;

use regex::Regex;

/// Task-tool subagent capture buckets: `agent-<anything>`.
static AGENT_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^agent-.+$").expect("static agent-prefix regex compiles"));

/// Workflow / phase capture buckets: `p<digits>-<anything>` (e.g. `p1-foo`,
/// `p42-bar`).
static PHASE_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^p\d+-.+$").expect("static phase-prefix regex compiles"));

/// A bare 32-hex-character session id with no dash pairing.
static HEX32: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{32}$").expect("static hex32 regex compiles"));

/// A `<32hex>-<40hex>` session-hash pair (parent session id + child agent id),
/// e.g. `a1f52dae0000000000000000000f66-ae675eb00000000000000000000000000003dd2`.
static HEX32_HEX40: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9a-f]{32}-[0-9a-f]{40}$").expect("static hex32-hex40 regex compiles")
});

/// True when `name` is unambiguously a machine-generated capture bucket, not
/// a real project name.
///
/// Matches ONLY:
/// - `agent-*`         — Task-tool subagent capture buckets
/// - `p<digits>-*`     — workflow / phase capture buckets
/// - a bare 32-hex string
/// - a `<32hex>-<40hex>` session-hash pair
///
/// Does NOT match ordinary project names — hyphenated names (`codex-for-oss`,
/// `wireproof`), scaffold-prefixed names (`00G_rtrt`, `00G_AI-Project-Setup`),
/// non-ASCII names, `default`, or a person's handle (`kernalix7`).
pub fn is_capture_bucket_name(name: &str) -> bool {
    AGENT_PREFIX.is_match(name)
        || PHASE_PREFIX.is_match(name)
        || HEX32.is_match(name)
        || HEX32_HEX40.is_match(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_agent_prefix() {
        assert!(is_capture_bucket_name("agent-1234"));
        assert!(is_capture_bucket_name("agent-code-reviewer"));
        // No content after the dash is not a recognised shape.
        assert!(!is_capture_bucket_name("agent-"));
        assert!(!is_capture_bucket_name("agent"));
    }

    #[test]
    fn matches_phase_prefix() {
        assert!(is_capture_bucket_name("p1-workflow"));
        assert!(is_capture_bucket_name("p42-dashboard-usage"));
        // No digits, or no dash-separated content, isn't the machine shape.
        assert!(!is_capture_bucket_name("p-workflow"));
        assert!(!is_capture_bucket_name("p4"));
        assert!(!is_capture_bucket_name("phase1-workflow"));
    }

    #[test]
    fn matches_bare_hex32() {
        let hex32 = "30877432d1026706d7e805da846a32c3";
        let hex32 = &hex32[..32];
        assert_eq!(hex32.len(), 32);
        assert!(is_capture_bucket_name(hex32));
        // Wrong length, or uppercase / non-hex chars, isn't the machine shape.
        assert!(!is_capture_bucket_name(&hex32[..31]));
        assert!(!is_capture_bucket_name(&format!("{hex32}0")));
        assert!(!is_capture_bucket_name(&hex32.to_uppercase()));
    }

    #[test]
    fn matches_hex32_hex40_session_pair() {
        let hex32 = &"30877432d1026706d7e805da846a32c3"[..32];
        let hex40 = "bb81e3c29b62179273c8eb5bb682575ec87a171a";
        assert_eq!(hex32.len(), 32);
        assert_eq!(hex40.len(), 40);
        let bucket = format!("{hex32}-{hex40}");
        assert!(is_capture_bucket_name(&bucket));
        // The confirmed failing case from the orphan bucket investigation
        // (32-hex parent session + 40-hex agent id, dash-joined).
        assert!(is_capture_bucket_name(&format!("{hex32}-{hex40}")));
    }

    #[test]
    fn does_not_match_real_project_names() {
        for name in [
            "codex-for-oss",
            "wireproof",
            "00G_rtrt",
            "00G_AI-Project-Setup",
            "default",
            "kernalix7",
            "프로젝트",
            "my-app-server",
            "agentmemory",     // no dash after "agent"
            "provider-config", // starts with "p" but not "p<digits>-"
        ] {
            assert!(
                !is_capture_bucket_name(name),
                "{name} should not classify as a capture bucket"
            );
        }
    }
}
