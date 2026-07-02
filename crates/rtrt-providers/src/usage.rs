use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use rtrt_core::Config;
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub usage_by_target: BTreeMap<String, u64>,
    pub limits_by_target: BTreeMap<String, u64>,
    pub requests_by_target: BTreeMap<String, u64>,
    pub request_limits_by_target: BTreeMap<String, u64>,
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
    pub fn with_ledger_window(mut self) -> Self {
        let windows = crate::usage_ledger::provider_usage_windows();
        for (target, window) in windows {
            let recent = window.last_24h;
            self.usage_by_target.insert(target.clone(), recent.tokens);
            self.requests_by_target.insert(target, recent.requests);
        }
        self.sources.push(format!(
            "provider-usage-ledger: {} target(s) in the rolling 24h window",
            self.usage_by_target.len()
        ));
        self
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
            requests_by_target: BTreeMap::new(),
            request_limits_by_target: BTreeMap::new(),
            proxy_runs: None,
            sources: Vec::new(),
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
            proxy_runs: None,
            sources: Vec::new(),
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
        }
        self.sources.push(format!(
            "limits: {} ({} token limits, {} request limits)",
            path.display(),
            self.limits_by_target.len(),
            self.request_limits_by_target.len()
        ));
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
