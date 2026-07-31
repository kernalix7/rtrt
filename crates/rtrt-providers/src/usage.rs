use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use rtrt_core::{Config, PoolKey};
use serde::{Deserialize, Serialize};

use crate::usage_ledger::PoolCap;

const PROXY_STATS_DB_FILE_NAME: &str = "proxy-stats.sqlite";
const ESTIMATED_CHARS_PER_TOKEN: u64 = 4;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn merge(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
    }
}

/// Usage and limits, keyed by target *and* by pool.
///
/// The `*_by_target` maps are the original surface and keep their exact
/// meaning. The `*_by_pool` maps are additive and keyed by
/// [`PoolKey::canonical`] (`opencode#opencode-go`): the finer identity that
/// separates two upstream quotas reached through one target. A pool only
/// appears in `limits_by_pool` when a cap was configured *for that pool* — a
/// target-wide cap is never split across its pools.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub usage_by_target: BTreeMap<String, u64>,
    pub limits_by_target: BTreeMap<String, u64>,
    pub requests_by_target: BTreeMap<String, u64>,
    pub request_limits_by_target: BTreeMap<String, u64>,
    #[serde(default)]
    pub usage_by_pool: BTreeMap<String, u64>,
    #[serde(default)]
    pub limits_by_pool: BTreeMap<String, u64>,
    #[serde(default)]
    pub requests_by_pool: BTreeMap<String, u64>,
    #[serde(default)]
    pub request_limits_by_pool: BTreeMap<String, u64>,
    pub proxy_runs: Option<ProxyUsage>,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaHeadroom {
    pub used: u64,
    pub limit: u64,
    pub remaining: u64,
    pub token_limit_configured: bool,
    pub request_used: Option<u64>,
    pub request_limit: Option<u64>,
    pub request_remaining: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyUsage {
    pub runs: u64,
    pub input_chars: u64,
    pub output_chars: u64,
}

/// Headroom for one pool inside a target.
///
/// `used` / `request_used` are this pool's own numbers. Each cap carries its
/// own [`crate::usage_ledger::CapScope`], so a ceiling inherited from the target
/// is reported as shared with the sibling pools instead of being presented as
/// this pool's private allowance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolQuota {
    /// [`PoolKey::canonical`] of the bucket.
    pub key: String,
    pub used: u64,
    pub request_used: u64,
    pub tokens: PoolCap,
    pub requests: PoolCap,
}

impl UsageSnapshot {
    pub fn load_best_effort() -> Self {
        let mut snapshot = Self::default();
        snapshot.load_token_log(
            &PathBuf::from(".priv-storage")
                .join("sessions")
                .join("token-log.tsv"),
        );
        snapshot.load_proxy_stats();
        snapshot.load_limits();
        snapshot
    }

    /// Overlay the per-provider usage ledger's rolling 24h window onto this
    /// snapshot, so `select_route` can rank candidates by recent ledger usage
    /// against the configured `[limits]` daily caps.
    ///
    /// Additive and opt-in: `load_best_effort` does NOT call this, so default
    /// routing behavior is unchanged in P1. P2 wires it in to make routing
    /// usage-aware. Ledger windows replace the per-target counters they cover
    /// (the ledger is the authoritative recent-usage source for routing).
    ///
    /// The per-pool window is filled in at the same time from the same read.
    /// Target-keyed consumers see byte-identical numbers, because the
    /// target-level map is the fold of the pooled one.
    pub fn with_ledger_window(mut self) -> Self {
        let pooled = crate::usage_ledger::pool_usage_windows();
        let windows = crate::usage_ledger::fold_pools_to_targets(&pooled);
        for (target, window) in windows {
            let recent = window.last_24h;
            self.usage_by_target.insert(target.clone(), recent.tokens);
            self.requests_by_target.insert(target, recent.requests);
        }
        self.sources.push(format!(
            "provider-usage-ledger: {} target(s) in the rolling 24h window",
            self.usage_by_target.len()
        ));
        let mut pools = 0_usize;
        for (key, window) in &pooled {
            let recent = window.last_24h;
            self.usage_by_pool.insert(key.clone(), recent.tokens);
            self.requests_by_pool.insert(key.clone(), recent.requests);
            if PoolKey::parse(key).is_pooled() {
                pools = pools.saturating_add(1);
            }
        }
        // Only reported when the ledger actually contains pooled rows, so a
        // ledger written before pools existed produces the same source list it
        // always did.
        if pools > 0 {
            self.sources.push(format!(
                "provider-usage-ledger: {pools} pool(s) inside those targets"
            ));
        }
        self
    }

    /// Headroom for one pool, or `None` when no cap is configured at either the
    /// pool or the target level — the same "never fabricate a ceiling" contract
    /// as [`UsageSnapshot::headroom`].
    pub fn pool_headroom(&self, key: &PoolKey) -> Option<PoolQuota> {
        let canonical = key.canonical();
        let target = key.target_key();
        let pool_token_limit = self.limits_by_pool.get(&canonical).copied();
        let pool_request_limit = self.request_limits_by_pool.get(&canonical).copied();
        let target_token_limit = self.limits_by_target.get(target).copied();
        let target_request_limit = self.request_limits_by_target.get(target).copied();
        if pool_token_limit.is_none()
            && pool_request_limit.is_none()
            && target_token_limit.is_none()
            && target_request_limit.is_none()
        {
            return None;
        }
        let used = self.usage_by_pool.get(&canonical).copied().unwrap_or(0);
        let request_used = self.requests_by_pool.get(&canonical).copied().unwrap_or(0);
        let target_used = self.usage_by_target.get(target).copied().unwrap_or(0);
        let target_requests = self.requests_by_target.get(target).copied().unwrap_or(0);
        Some(PoolQuota {
            key: canonical,
            used,
            request_used,
            // Same Pool / Shared / Unknown resolution the ledger uses, so the
            // two surfaces cannot disagree about what a cap means.
            tokens: crate::usage_ledger::axis_cap(
                pool_token_limit,
                target_token_limit,
                used,
                target_used,
            ),
            requests: crate::usage_ledger::axis_cap(
                pool_request_limit,
                target_request_limit,
                request_used,
                target_requests,
            ),
        })
    }

    /// The snapshot every routing surface (CLI, MCP `agent_route`, dashboard
    /// route preview) must use: best-effort sources overlaid with the
    /// provider-usage ledger's rolling 24h window, so `select_route` ranks
    /// candidates headroom-aware everywhere — never on stale token-log data
    /// alone.
    pub fn load_for_routing() -> Self {
        Self::load_best_effort().with_ledger_window()
    }

    pub fn headroom(&self, target: &str) -> Option<QuotaHeadroom> {
        let target = normalize_target(target);
        let limit = self.limits_by_target.get(&target).copied();
        let request_limit = self.request_limits_by_target.get(&target).copied();
        if limit.is_none() && request_limit.is_none() {
            return None;
        }
        let used = self.usage_by_target.get(&target).copied().unwrap_or(0);
        let request_used = request_limit.map(|_| {
            self.requests_by_target
                .get(&target)
                .copied()
                .unwrap_or_default()
        });
        Some(QuotaHeadroom {
            used,
            limit: limit.unwrap_or_default(),
            remaining: limit.map_or(u64::MAX, |limit| limit.saturating_sub(used)),
            token_limit_configured: limit.is_some(),
            request_used,
            request_limit,
            request_remaining: request_limit.map(|limit| {
                limit.saturating_sub(
                    self.requests_by_target
                        .get(&target)
                        .copied()
                        .unwrap_or_default(),
                )
            }),
        })
    }

    #[cfg(test)]
    pub fn from_usage_and_limits_for_tests(
        usage: impl IntoIterator<Item = (&'static str, u64)>,
        limits: impl IntoIterator<Item = (&'static str, u64)>,
    ) -> Self {
        Self {
            usage_by_target: usage
                .into_iter()
                .map(|(target, used)| (normalize_target(target), used))
                .collect(),
            limits_by_target: limits
                .into_iter()
                .map(|(target, limit)| (normalize_target(target), limit))
                .collect(),
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub fn from_usage_limits_and_requests_for_tests(
        usage: impl IntoIterator<Item = (&'static str, u64)>,
        limits: impl IntoIterator<Item = (&'static str, u64)>,
        requests: impl IntoIterator<Item = (&'static str, u64)>,
        request_limits: impl IntoIterator<Item = (&'static str, u64)>,
    ) -> Self {
        Self {
            usage_by_target: usage
                .into_iter()
                .map(|(target, used)| (normalize_target(target), used))
                .collect(),
            limits_by_target: limits
                .into_iter()
                .map(|(target, limit)| (normalize_target(target), limit))
                .collect(),
            requests_by_target: requests
                .into_iter()
                .map(|(target, used)| (normalize_target(target), used))
                .collect(),
            request_limits_by_target: request_limits
                .into_iter()
                .map(|(target, limit)| (normalize_target(target), limit))
                .collect(),
            ..Self::default()
        }
    }

    fn load_token_log(&mut self, path: &Path) {
        let Ok(raw) = std::fs::read_to_string(path) else {
            self.sources
                .push(format!("token-log: unavailable ({})", path.display()));
            return;
        };
        let mut rows = 0usize;
        for line in raw.lines().filter(|line| !line.trim().is_empty()) {
            let fields = line.split('\t').collect::<Vec<_>>();
            if fields.len() < token_log_min_fields() {
                continue;
            }
            let metric_end = fields
                .len()
                .saturating_sub(token_log_trailing_text_fields());
            let Some(model) = fields.get(
                fields
                    .len()
                    .saturating_sub(token_log_trailing_text_fields()),
            ) else {
                continue;
            };
            let total = fields[token_log_timestamp_fields()..metric_end]
                .iter()
                .filter_map(|value| value.parse::<u64>().ok())
                .fold(0_u64, u64::saturating_add);
            if total == 0 {
                continue;
            }
            rows = rows.saturating_add(1);
            add_usage(&mut self.usage_by_target, model, total);
            add_usage(&mut self.requests_by_target, model, 1);
            if let Some(provider) = provider_for_model(model) {
                add_usage(&mut self.usage_by_target, provider, total);
                add_usage(&mut self.requests_by_target, provider, 1);
            }
        }
        self.sources.push(format!(
            "token-log: {} ({rows} parseable rows)",
            path.display()
        ));
    }

    fn load_proxy_stats(&mut self) {
        let path = proxy_stats_path();
        if !path.exists() {
            self.sources.push(format!(
                "proxy-stats: unavailable ({} not found)",
                path.display()
            ));
            return;
        }
        match load_proxy_usage(&path) {
            Ok(proxy) => {
                let estimated_tokens =
                    chars_to_estimated_tokens(proxy.input_chars.saturating_add(proxy.output_chars));
                if estimated_tokens > 0 {
                    add_usage(&mut self.usage_by_target, "proxy", estimated_tokens);
                }
                if proxy.runs > 0 {
                    add_usage(&mut self.requests_by_target, "proxy", proxy.runs);
                }
                self.proxy_runs = Some(proxy);
                self.sources.push(format!(
                    "proxy-stats: {} ({} runs, chars/{} token estimate)",
                    path.display(),
                    proxy.runs,
                    ESTIMATED_CHARS_PER_TOKEN
                ));
            }
            Err(err) => {
                self.sources.push(format!(
                    "proxy-stats: unavailable ({}: {err})",
                    path.display()
                ));
            }
        }
    }

    fn load_limits(&mut self) {
        let Some(path) = Config::default_path() else {
            self.sources
                .push("limits: unavailable (no config path)".to_string());
            return;
        };
        let config = match Config::load() {
            Ok(config) => config,
            Err(err) => {
                self.sources
                    .push(format!("limits: unavailable ({}: {err})", path.display()));
                return;
            }
        };
        if config.limits.is_empty() {
            self.sources.push(format!(
                "limits: unavailable ({} has no [limits.<target>])",
                path.display()
            ));
            return;
        }
        for (target, limit) in &config.limits.targets {
            add_limit(&mut self.limits_by_target, target, limit.daily_tokens);
            add_limit(
                &mut self.request_limits_by_target,
                target,
                limit.daily_requests,
            );
            for (pool, cap) in &limit.pools {
                let key = PoolKey::new(target, Some(pool)).canonical();
                if let Some(tokens) = cap.daily_tokens {
                    self.limits_by_pool.insert(key.clone(), tokens);
                }
                if let Some(requests) = cap.daily_requests {
                    self.request_limits_by_pool.insert(key, requests);
                }
            }
        }
        self.sources.push(format!(
            "limits: {} ({} token limits, {} request limits)",
            path.display(),
            self.limits_by_target.len(),
            self.request_limits_by_target.len()
        ));
        // Additive line, emitted only when per-pool caps exist, so a config
        // without them reports exactly what it always has.
        let pool_caps = self.limits_by_pool.len() + self.request_limits_by_pool.len();
        if pool_caps > 0 {
            self.sources.push(format!(
                "limits: {} pool cap(s) nested under [limits.<target>.pools]",
                pool_caps
            ));
        }
    }
}

fn add_usage(usage: &mut BTreeMap<String, u64>, target: &str, tokens: u64) {
    usage
        .entry(target.to_ascii_lowercase())
        .and_modify(|used| *used = used.saturating_add(tokens))
        .or_insert(tokens);
}

fn add_limit(limits: &mut BTreeMap<String, u64>, target: &str, limit: Option<u64>) {
    if let Some(limit) = limit {
        limits.insert(normalize_target(target), limit);
    }
}

fn normalize_target(target: &str) -> String {
    target.to_ascii_lowercase()
}

fn load_proxy_usage(path: &Path) -> Result<ProxyUsage, String> {
    let output = Command::new("sqlite3")
        .arg("-readonly")
        .arg(path)
        .arg(
            "SELECT COUNT(*), COALESCE(SUM(input_chars), 0), COALESCE(SUM(output_chars), 0) FROM proxy_runs;",
        )
        .output()
        .map_err(|err| format!("spawn sqlite3: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("sqlite3 exited with {}", output.status)
        } else {
            stderr
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let row = stdout
        .lines()
        .next()
        .ok_or_else(|| "sqlite3 returned no rows".to_string())?;
    parse_proxy_usage_row(row)
}

fn parse_proxy_usage_row(row: &str) -> Result<ProxyUsage, String> {
    let mut fields = row.trim().split('|');
    let runs = parse_nonnegative_u64(fields.next(), "runs")?;
    let input_chars = parse_nonnegative_u64(fields.next(), "input_chars")?;
    let output_chars = parse_nonnegative_u64(fields.next(), "output_chars")?;
    if fields.next().is_some() {
        return Err("sqlite3 returned too many columns".to_string());
    }
    Ok(ProxyUsage {
        runs,
        input_chars,
        output_chars,
    })
}

fn proxy_stats_path() -> PathBuf {
    std::env::var_os("RTRT_PROXY_STATS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".rtrt")
                .join(PROXY_STATS_DB_FILE_NAME)
        })
}

fn chars_to_estimated_tokens(chars: u64) -> u64 {
    chars.div_ceil(ESTIMATED_CHARS_PER_TOKEN)
}

fn parse_nonnegative_u64(value: Option<&str>, field: &str) -> Result<u64, String> {
    let value = value.ok_or_else(|| format!("sqlite3 missing {field}"))?;
    value
        .parse::<u64>()
        .map_err(|err| format!("sqlite3 invalid {field}: {err}"))
}

fn provider_for_model(model: &str) -> Option<&'static str> {
    let normalized = model.to_ascii_lowercase();
    if normalized.starts_with("claude") {
        Some("anthropic")
    } else if normalized.starts_with("gpt")
        || normalized.starts_with("o1")
        || normalized.starts_with("o3")
    {
        Some("openai")
    } else {
        None
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn token_log_trailing_text_fields() -> usize {
    2
}

fn token_log_timestamp_fields() -> usize {
    1
}

fn token_log_min_fields() -> usize {
    token_log_timestamp_fields() + token_log_trailing_text_fields() + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage_ledger::CapScope;

    /// Two pools under one target: 150 tokens / 1 request on `opencode-go`,
    /// 600 tokens / 1 request on `ollama`, folded to 750 / 2 at target level.
    fn pooled_snapshot() -> UsageSnapshot {
        let mut snapshot = UsageSnapshot::default();
        snapshot.usage_by_target.insert("opencode".into(), 750);
        snapshot.requests_by_target.insert("opencode".into(), 2);
        snapshot
            .usage_by_pool
            .insert("opencode#opencode-go".into(), 150);
        snapshot.usage_by_pool.insert("opencode#ollama".into(), 600);
        snapshot
            .requests_by_pool
            .insert("opencode#opencode-go".into(), 1);
        snapshot
            .requests_by_pool
            .insert("opencode#ollama".into(), 1);
        snapshot
    }

    fn go() -> PoolKey {
        PoolKey::from_target_model("opencode", Some("opencode-go/glm-5.2"))
    }

    #[test]
    fn pool_headroom_without_any_cap_is_none() {
        let snapshot = pooled_snapshot();
        assert!(snapshot.pool_headroom(&go()).is_none());
        // The observed usage is still there — measured, not inferred.
        assert_eq!(snapshot.usage_by_pool["opencode#opencode-go"], 150);
    }

    #[test]
    fn target_cap_is_shared_across_pools_never_split() {
        let mut snapshot = pooled_snapshot();
        snapshot.limits_by_target.insert("opencode".into(), 1_000);
        snapshot
            .request_limits_by_target
            .insert("opencode".into(), 10);

        let quota = snapshot.pool_headroom(&go()).expect("target cap applies");
        assert_eq!(quota.key, "opencode#opencode-go");
        assert_eq!(quota.used, 150, "this pool's own usage");
        assert_eq!(quota.tokens.scope, CapScope::Shared);
        // The whole cap, drawn down by every sibling together.
        assert_eq!(quota.tokens.limit, Some(1_000));
        assert_eq!(quota.tokens.used, 750);
        assert_eq!(quota.tokens.remaining, Some(250));
        assert_eq!(quota.requests.scope, CapScope::Shared);
        assert_eq!(quota.requests.remaining, Some(8));
        // The target-keyed view is untouched by any of this.
        let target = snapshot.headroom("opencode").expect("target headroom");
        assert_eq!(target.used, 750);
        assert_eq!(target.remaining, 250);
    }

    #[test]
    fn configured_pool_cap_wins_and_charges_only_that_pool() {
        let mut snapshot = pooled_snapshot();
        snapshot.limits_by_target.insert("opencode".into(), 1_000);
        snapshot
            .limits_by_pool
            .insert("opencode#opencode-go".into(), 400);

        let quota = snapshot.pool_headroom(&go()).expect("pool cap applies");
        assert_eq!(quota.tokens.scope, CapScope::Pool);
        assert_eq!(quota.tokens.limit, Some(400));
        assert_eq!(quota.tokens.used, 150);
        assert_eq!(quota.tokens.remaining, Some(250));
        // The uncapped axis stays honest about being shared.
        assert_eq!(quota.requests.scope, CapScope::Unknown);

        // The sibling keeps sharing the target cap.
        let cloud = PoolKey::from_target_model("opencode", Some("ollama/glm-5.2:cloud"));
        let sibling = snapshot.pool_headroom(&cloud).expect("shared cap applies");
        assert_eq!(sibling.tokens.scope, CapScope::Shared);
        assert_eq!(sibling.tokens.limit, Some(1_000));
    }

    #[test]
    fn unpooled_key_reads_the_target_bucket() {
        let mut snapshot = UsageSnapshot::default();
        snapshot.usage_by_target.insert("ollama".into(), 150);
        snapshot.usage_by_pool.insert("ollama".into(), 150);
        snapshot.limits_by_target.insert("ollama".into(), 1_000);
        let key = PoolKey::from_target_model("ollama", Some("granite4:350m"));
        assert!(!key.is_pooled());
        let quota = snapshot.pool_headroom(&key).expect("target cap applies");
        assert_eq!(quota.key, "ollama");
        assert_eq!(quota.used, 150);
        assert_eq!(quota.tokens.remaining, Some(850));
    }

    #[test]
    fn snapshots_serialized_before_pools_still_deserialize() {
        let legacy = r#"{
            "usage_by_target": {"openai": 150},
            "limits_by_target": {"openai": 1000},
            "requests_by_target": {"openai": 1},
            "request_limits_by_target": {"openai": 10},
            "proxy_runs": null,
            "sources": ["token-log: unavailable"]
        }"#;
        let snapshot: UsageSnapshot = serde_json::from_str(legacy).expect("legacy snapshot parses");
        assert_eq!(snapshot.usage_by_target["openai"], 150);
        assert!(snapshot.usage_by_pool.is_empty());
        assert!(snapshot.limits_by_pool.is_empty());
        let headroom = snapshot.headroom("openai").expect("headroom");
        assert_eq!(headroom.remaining, 850);
    }
}
