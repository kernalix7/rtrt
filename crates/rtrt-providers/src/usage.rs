use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

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
    pub proxy_runs: Option<ProxyUsage>,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaHeadroom {
    pub used: u64,
    pub limit: u64,
    pub remaining: u64,
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
        snapshot.load_limits();
        snapshot
    }

    pub fn headroom(&self, target: &str) -> Option<QuotaHeadroom> {
        let limit = self.limits_by_target.get(target).copied()?;
        let used = self.usage_by_target.get(target).copied().unwrap_or(0);
        Some(QuotaHeadroom {
            used,
            limit,
            remaining: limit.saturating_sub(used),
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
                .map(|(target, used)| (target.to_string(), used))
                .collect(),
            limits_by_target: limits
                .into_iter()
                .map(|(target, limit)| (target.to_string(), limit))
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
            if let Some(provider) = provider_for_model(model) {
                add_usage(&mut self.usage_by_target, provider, total);
            }
        }
        self.sources.push(format!(
            "token-log: {} ({rows} parseable rows)",
            path.display()
        ));
    }

    fn load_limits(&mut self) {
        let Some(path) = config_path() else {
            self.sources
                .push("limits: unavailable (no home directory)".to_string());
            return;
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            self.sources.push(format!(
                "limits: unavailable ({} not found)",
                path.display()
            ));
            return;
        };
        let limits = parse_limits_table(&raw);
        if limits.is_empty() {
            self.sources.push(format!(
                "limits: unavailable ({} has no [limits])",
                path.display()
            ));
            return;
        }
        self.limits_by_target.extend(limits);
        self.sources.push(format!(
            "limits: {} ({} token limits)",
            path.display(),
            self.limits_by_target.len()
        ));
    }
}

fn add_usage(usage: &mut BTreeMap<String, u64>, target: &str, tokens: u64) {
    usage
        .entry(target.to_ascii_lowercase())
        .and_modify(|used| *used = used.saturating_add(tokens))
        .or_insert(tokens);
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

fn config_path() -> Option<PathBuf> {
    std::env::var_os("RTRT_CONFIG")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".rtrt").join("config.toml")))
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

fn parse_limits_table(raw: &str) -> BTreeMap<String, u64> {
    let mut limits = BTreeMap::new();
    let mut in_limits = false;
    for line in raw.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_limits = line == "[limits]";
            continue;
        }
        if !in_limits {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(target) = key.trim().strip_suffix("_daily_tokens") else {
            continue;
        };
        let value = value.trim().replace('_', "");
        if let Ok(limit) = value.parse::<u64>() {
            limits.insert(target.to_string(), limit);
        }
    }
    limits
}
