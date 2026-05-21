//! Secret-shape redactor.
//!
//! Runs before the compression pipeline so secret-shaped substrings get
//! replaced with `<REDACTED:<kind>>` placeholders, preventing them from ever
//! reaching the downstream LLM (or showing up in cached transcripts).
//!
//! Patterns target high-confidence secret shapes — false positives are far
//! worse than false negatives here because the agent loses information. We
//! intentionally do NOT match low-entropy hex strings, generic UUIDs, or
//! standard-looking integers.

use once_cell::sync::Lazy;
use regex::Regex;

struct Pattern {
    kind: &'static str,
    regex: &'static Lazy<Regex>,
}

static AWS_ACCESS_KEY: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(AKIA|ASIA)[0-9A-Z]{16}\b").unwrap());
static AWS_SECRET: Lazy<Regex> = Lazy::new(|| {
    // Heuristic: "aws_secret_access_key" prefix followed by 40 base64-ish chars.
    Regex::new(r#"(?i)\baws[_-]?secret[_-]?access[_-]?key\s*[:=]\s*['"]?([A-Za-z0-9+/=]{40})['"]?"#)
        .unwrap()
});
static GITHUB_TOKEN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\bgh[opsur]_[A-Za-z0-9_]{30,}\b").unwrap()
});
static GITHUB_PAT_CLASSIC: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bghp_[A-Za-z0-9]{36}\b").unwrap());
static OPENAI_KEY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\bsk-(proj-)?[A-Za-z0-9_\-]{20,}\b").unwrap()
});
static ANTHROPIC_KEY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\bsk-ant-[A-Za-z0-9_\-]{20,}\b").unwrap()
});
static SLACK_TOKEN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\bxox[abprs]-[A-Za-z0-9\-]{10,}\b").unwrap()
});
static PRIVATE_KEY_BLOCK: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"-----BEGIN [A-Z ]+PRIVATE KEY-----[\s\S]*?-----END [A-Z ]+PRIVATE KEY-----")
        .unwrap()
});
static BEARER_TOKEN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\bBearer\s+([A-Za-z0-9_\-]{16,})\b").unwrap()
});
static GENERIC_API_KEY: Lazy<Regex> = Lazy::new(|| {
    // "api_key=…" / "apikey=…" / "API_KEY: …" patterns. Requires obvious context
    // so plain hex doesn't false-positive.
    Regex::new(
        r#"(?i)\bapi[_-]?key\s*[:=]\s*['"]?([A-Za-z0-9_\-]{20,})['"]?"#,
    )
    .unwrap()
});

static PATTERNS: Lazy<Vec<Pattern>> = Lazy::new(|| {
    vec![
        Pattern { kind: "aws-access-key", regex: &AWS_ACCESS_KEY },
        Pattern { kind: "aws-secret", regex: &AWS_SECRET },
        Pattern { kind: "github-pat", regex: &GITHUB_PAT_CLASSIC },
        Pattern { kind: "github-token", regex: &GITHUB_TOKEN },
        // Anthropic before OpenAI: both use the `sk-` prefix; the OpenAI regex
        // matches `sk-ant-…` too, so the more specific pattern must run first.
        Pattern { kind: "anthropic-key", regex: &ANTHROPIC_KEY },
        Pattern { kind: "openai-key", regex: &OPENAI_KEY },
        Pattern { kind: "slack-token", regex: &SLACK_TOKEN },
        Pattern { kind: "private-key", regex: &PRIVATE_KEY_BLOCK },
        Pattern { kind: "bearer-token", regex: &BEARER_TOKEN },
        Pattern { kind: "generic-api-key", regex: &GENERIC_API_KEY },
    ]
});

/// Replaces secret-shaped substrings with `<REDACTED:<kind>>`. Idempotent.
pub fn redact_secrets(input: &str) -> String {
    let mut out = input.to_string();
    for p in PATTERNS.iter() {
        let replacement = format!("<REDACTED:{}>", p.kind);
        out = p.regex.replace_all(&out, replacement.as_str()).into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_aws_access_key() {
        let s = "use AKIAIOSFODNN7EXAMPLE for prod";
        assert!(redact_secrets(s).contains("<REDACTED:aws-access-key>"));
    }

    #[test]
    fn redacts_github_pat() {
        let s = "token ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        assert!(redact_secrets(s).contains("<REDACTED:github-pat>"));
    }

    #[test]
    fn redacts_openai_key() {
        let s = "OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz";
        let out = redact_secrets(s);
        assert!(out.contains("<REDACTED:"), "{out}");
        assert!(!out.contains("sk-proj-abcdef"), "{out}");
    }

    #[test]
    fn redacts_anthropic_key() {
        let s = "ANTHROPIC=sk-ant-abcdef1234567890ghijk";
        assert!(redact_secrets(s).contains("<REDACTED:anthropic-key>"));
    }

    #[test]
    fn redacts_bearer() {
        let s = "Authorization: Bearer abc123def456ghi789jkl";
        let out = redact_secrets(s);
        assert!(out.contains("<REDACTED:"), "{out}");
        assert!(!out.contains("abc123def456ghi789jkl"), "{out}");
    }

    #[test]
    fn redacts_private_key_block() {
        let s = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow...lots of base64...\n-----END RSA PRIVATE KEY-----";
        let out = redact_secrets(s);
        assert!(out.contains("<REDACTED:private-key>"));
        assert!(!out.contains("MIIEow"), "{out}");
    }

    #[test]
    fn preserves_non_secrets() {
        let s = "this is just regular text with numbers 12345 and uuids 550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(redact_secrets(s), s);
    }
}
