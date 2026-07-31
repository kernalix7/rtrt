//! Pool identity — the quota bucket an invocation actually draws from.
//!
//! A *target* is the tool rtrt invokes (`opencode`, `claude`, `ollama`); a
//! *pool* is the upstream backend behind that target, carried in the model
//! string's prefix. One target routinely fans out to several unrelated upstream
//! quotas — the default team roster reaches two through `opencode` alone:
//!
//! ```text
//! target=opencode  model=opencode-go/glm-5.2    -> pool "opencode-go"
//! target=opencode  model=ollama/glm-5.2:cloud   -> pool "ollama"
//! ```
//!
//! Keying usage by target alone merges those into a single bucket, so exhausting
//! one upstream looks like exhausting both. [`PoolKey`] is the finer identity:
//! `target` plus the optional pool derived from the model prefix.
//!
//! ```
//! use rtrt_core::pool::PoolKey;
//!
//! let go = PoolKey::from_target_model("opencode", Some("opencode-go/glm-5.2"));
//! let cloud = PoolKey::from_target_model("opencode", Some("ollama/glm-5.2:cloud"));
//! assert_eq!(go.canonical(), "opencode#opencode-go");
//! assert_eq!(cloud.canonical(), "opencode#ollama");
//! // A model with no prefix carries no pool identity — never invent one.
//! assert_eq!(
//!     PoolKey::from_target_model("ollama", Some("granite4:350m")).canonical(),
//!     "ollama"
//! );
//! ```

use std::fmt;

use serde::{Deserialize, Serialize};

/// Separates target from pool in a [`PoolKey::canonical`] string.
///
/// Chosen because it cannot appear in a target name (targets are tool binaries)
/// and [`PoolKey::parse`] splits on the *first* occurrence, so a pool name that
/// happens to contain it still round-trips.
pub const POOL_SEPARATOR: char = '#';

/// Separates the pool prefix from the rest of a model string.
pub const MODEL_POOL_SEPARATOR: char = '/';

/// The identity of one quota bucket: a target, plus the upstream pool inside it
/// when the model string names one.
///
/// `pool: None` means "unpooled" — the model carried no prefix, so the target
/// itself is the finest identity we honestly know.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PoolKey {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
}

impl PoolKey {
    /// Build a key from an explicit target and pool. Both are trimmed and
    /// lowercased; a blank pool collapses to `None` (unpooled) rather than to an
    /// empty pool name.
    pub fn new(target: &str, pool: Option<&str>) -> Self {
        Self {
            target: normalize_target(target),
            pool: pool.and_then(normalize_pool),
        }
    }

    /// A target with no pool identity.
    pub fn unpooled(target: &str) -> Self {
        Self {
            target: normalize_target(target),
            pool: None,
        }
    }

    /// Derive the key from the pair recorded per invocation.
    ///
    /// The pool is the model string's prefix before the first `/`
    /// (`opencode-go/glm-5.2` → `opencode-go`). A model with no `/`
    /// (`granite4:350m`), a leading `/`, a blank model, or `None` yields an
    /// unpooled key — we never synthesise a pool name.
    pub fn from_target_model(target: &str, model: Option<&str>) -> Self {
        Self {
            target: normalize_target(target),
            pool: model.and_then(pool_from_model),
        }
    }

    /// Stable string form: `target` when unpooled, else `target#pool`.
    pub fn canonical(&self) -> String {
        match &self.pool {
            Some(pool) => format!("{}{POOL_SEPARATOR}{pool}", self.target),
            None => self.target.clone(),
        }
    }

    /// The target-level key this pool folds into (the canonical form of the
    /// same target with its pool dropped).
    pub fn target_key(&self) -> &str {
        &self.target
    }

    /// Inverse of [`PoolKey::canonical`]: split on the first [`POOL_SEPARATOR`].
    ///
    /// A trailing separator with nothing after it (`"opencode#"`) parses as
    /// unpooled — an empty pool name is not an identity.
    pub fn parse(key: &str) -> Self {
        match key.split_once(POOL_SEPARATOR) {
            Some((target, pool)) => Self::new(target, Some(pool)),
            None => Self::unpooled(key),
        }
    }

    /// True when this key names a pool inside its target.
    pub fn is_pooled(&self) -> bool {
        self.pool.is_some()
    }

    pub fn pool(&self) -> Option<&str> {
        self.pool.as_deref()
    }
}

impl fmt::Display for PoolKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.pool {
            Some(pool) => write!(f, "{}{POOL_SEPARATOR}{pool}", self.target),
            None => f.write_str(&self.target),
        }
    }
}

/// The pool a model string names, or `None` when it names none.
///
/// The prefix before the first `/`, trimmed and lowercased. Blank prefixes and
/// prefix-less models are `None`.
pub fn pool_from_model(model: &str) -> Option<String> {
    let (prefix, _rest) = model.trim().split_once(MODEL_POOL_SEPARATOR)?;
    normalize_pool(prefix)
}

/// Targets are compared case-insensitively and without surrounding whitespace,
/// matching how the usage ledger has always normalized them.
fn normalize_target(target: &str) -> String {
    target.trim().to_ascii_lowercase()
}

fn normalize_pool(pool: &str) -> Option<String> {
    let pool = pool.trim();
    if pool.is_empty() {
        return None;
    }
    Some(pool.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_prefix_becomes_the_pool() {
        let key = PoolKey::from_target_model("opencode", Some("opencode-go/glm-5.2"));
        assert_eq!(key.target, "opencode");
        assert_eq!(key.pool(), Some("opencode-go"));
        assert_eq!(key.canonical(), "opencode#opencode-go");
        assert!(key.is_pooled());
    }

    #[test]
    fn sibling_pools_under_one_target_are_distinct() {
        let go = PoolKey::from_target_model("opencode", Some("opencode-go/glm-5.2"));
        let cloud = PoolKey::from_target_model("opencode", Some("ollama/glm-5.2:cloud"));
        assert_ne!(go, cloud);
        assert_ne!(go.canonical(), cloud.canonical());
        // ...but they fold back to the same target.
        assert_eq!(go.target_key(), cloud.target_key());
    }

    #[test]
    fn model_without_slash_is_unpooled() {
        let key = PoolKey::from_target_model("ollama", Some("granite4:350m"));
        assert_eq!(key.pool(), None);
        assert!(!key.is_pooled());
        assert_eq!(key.canonical(), "ollama");
    }

    #[test]
    fn missing_or_blank_model_is_unpooled() {
        assert_eq!(PoolKey::from_target_model("claude", None).pool(), None);
        assert_eq!(PoolKey::from_target_model("claude", Some("")).pool(), None);
        assert_eq!(
            PoolKey::from_target_model("claude", Some("   ")).pool(),
            None
        );
    }

    #[test]
    fn leading_slash_has_no_prefix_so_no_pool() {
        let key = PoolKey::from_target_model("opencode", Some("/glm-5.2"));
        assert_eq!(key.pool(), None);
        assert_eq!(key.canonical(), "opencode");
    }

    #[test]
    fn only_the_first_segment_is_the_pool() {
        let key = PoolKey::from_target_model("opencode", Some("vendor/family/model:tag"));
        assert_eq!(key.pool(), Some("vendor"));
    }

    #[test]
    fn target_and_pool_are_normalized() {
        let key = PoolKey::from_target_model("  OpenCode ", Some(" OpenCode-GO/GLM-5.2"));
        assert_eq!(key.canonical(), "opencode#opencode-go");
    }

    #[test]
    fn canonical_parse_round_trips() {
        for key in [
            PoolKey::unpooled("opencode"),
            PoolKey::new("opencode", Some("opencode-go")),
            PoolKey::from_target_model("opencode", Some("ollama/glm-5.2:cloud")),
            // A pool name containing the separator still round-trips, because
            // parse splits on the FIRST separator only.
            PoolKey::new("opencode", Some("od#d")),
        ] {
            assert_eq!(PoolKey::parse(&key.canonical()), key, "round-trip {key}");
        }
    }

    #[test]
    fn parse_treats_empty_pool_as_unpooled() {
        assert_eq!(PoolKey::parse("opencode#"), PoolKey::unpooled("opencode"));
        assert_eq!(PoolKey::parse("opencode# "), PoolKey::unpooled("opencode"));
    }

    #[test]
    fn parse_lowercases_like_the_ledger() {
        assert_eq!(
            PoolKey::parse("OpenCode#OpenCode-Go"),
            PoolKey::new("opencode", Some("opencode-go"))
        );
    }

    #[test]
    fn display_matches_canonical() {
        let key = PoolKey::new("opencode", Some("ollama"));
        assert_eq!(key.to_string(), key.canonical());
        let plain = PoolKey::unpooled("claude");
        assert_eq!(plain.to_string(), plain.canonical());
    }

    #[test]
    fn pool_from_model_matches_key_derivation() {
        assert_eq!(
            pool_from_model("opencode-go/glm-5.2"),
            Some("opencode-go".to_string())
        );
        assert_eq!(pool_from_model("granite4:350m"), None);
        assert_eq!(pool_from_model("/x"), None);
        assert_eq!(pool_from_model(""), None);
    }
}
