//! qdrant-style payload filter DSL.
//!
//! Memory rows can carry a free-form `BTreeMap<String, String>` metadata
//! payload (serialised as JSON in the `metadata` column). Callers express
//! recall-time predicates over the payload with a tiny DSL:
//!
//! ```text
//! source=claude              -- exact match
//! agent!=cursor              -- negated exact match
//! topic~^auth                -- regex match (`~` is Rust `Regex::is_match`)
//! source=claude,topic~auth   -- conjunction (AND)
//! ```
//!
//! Whitespace and commas separate predicates. Keys and values are not
//! quoted; the DSL is intentionally compact so it fits inside an HTTP query
//! parameter or an MCP tool argument.

use std::collections::BTreeMap;

use regex::Regex;
use rtrt_core::{Error, Result};

#[derive(Debug, Clone)]
pub enum PayloadPredicate {
    Eq { key: String, value: String },
    Neq { key: String, value: String },
    Regex { key: String, regex: Regex },
}

impl PayloadPredicate {
    pub fn matches(&self, payload: &BTreeMap<String, String>) -> bool {
        match self {
            PayloadPredicate::Eq { key, value } => payload.get(key).is_some_and(|v| v == value),
            PayloadPredicate::Neq { key, value } => payload.get(key).is_none_or(|v| v != value),
            PayloadPredicate::Regex { key, regex } => {
                payload.get(key).is_some_and(|v| regex.is_match(v))
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PayloadFilter {
    predicates: Vec<PayloadPredicate>,
}

impl PayloadFilter {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn parse(input: &str) -> Result<Self> {
        let mut predicates = Vec::new();
        for raw in input
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            predicates.push(parse_predicate(raw)?);
        }
        Ok(Self { predicates })
    }

    pub fn is_empty(&self) -> bool {
        self.predicates.is_empty()
    }

    pub fn matches(&self, payload: &BTreeMap<String, String>) -> bool {
        self.predicates.iter().all(|p| p.matches(payload))
    }

    pub fn predicates(&self) -> &[PayloadPredicate] {
        &self.predicates
    }
}

fn parse_predicate(raw: &str) -> Result<PayloadPredicate> {
    if let Some((key, value)) = raw.split_once("!=") {
        return Ok(PayloadPredicate::Neq {
            key: validate_key(key)?,
            value: value.to_string(),
        });
    }
    if let Some((key, pattern)) = raw.split_once('~') {
        let regex = Regex::new(pattern)
            .map_err(|e| Error::Memory(format!("invalid regex `{pattern}`: {e}")))?;
        return Ok(PayloadPredicate::Regex {
            key: validate_key(key)?,
            regex,
        });
    }
    if let Some((key, value)) = raw.split_once('=') {
        return Ok(PayloadPredicate::Eq {
            key: validate_key(key)?,
            value: value.to_string(),
        });
    }
    Err(Error::Memory(format!(
        "unrecognised payload predicate: `{raw}` (expected key=val, key!=val, key~regex)"
    )))
}

fn validate_key(key: &str) -> Result<String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err(Error::Memory("payload predicate has empty key".into()));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(Error::Memory(format!(
            "payload key `{trimmed}` may only contain [A-Za-z0-9_.-]"
        )));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn eq_predicate() {
        let f = PayloadFilter::parse("source=claude").unwrap();
        assert!(f.matches(&meta(&[("source", "claude")])));
        assert!(!f.matches(&meta(&[("source", "cursor")])));
        assert!(!f.matches(&meta(&[])));
    }

    #[test]
    fn neq_predicate_passes_when_key_missing() {
        let f = PayloadFilter::parse("source!=cursor").unwrap();
        assert!(f.matches(&meta(&[("source", "claude")])));
        assert!(!f.matches(&meta(&[("source", "cursor")])));
        assert!(f.matches(&meta(&[])));
    }

    #[test]
    fn regex_predicate() {
        let f = PayloadFilter::parse(r"topic~^auth").unwrap();
        assert!(f.matches(&meta(&[("topic", "auth_token_rotation")])));
        assert!(!f.matches(&meta(&[("topic", "user_profile")])));
    }

    #[test]
    fn conjunction_via_comma() {
        let f = PayloadFilter::parse("source=claude,topic~auth").unwrap();
        assert!(f.matches(&meta(&[("source", "claude"), ("topic", "auth_flow")])));
        assert!(!f.matches(&meta(&[("source", "claude"), ("topic", "billing")])));
    }

    #[test]
    fn rejects_bad_key() {
        let err = PayloadFilter::parse("bad key=value").unwrap_err();
        assert!(err.to_string().contains("key"));
    }

    #[test]
    fn empty_filter_matches_everything() {
        let f = PayloadFilter::empty();
        assert!(f.matches(&meta(&[])));
        assert!(f.matches(&meta(&[("a", "b")])));
    }
}
