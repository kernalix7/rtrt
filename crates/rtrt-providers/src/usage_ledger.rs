//! Per-provider usage ledger — the P1 foundation for usage-aware routing.
//!
//! Every rtrt provider invocation appends one tab-separated row to
//! `~/.rtrt/provider-usage.tsv`:
//!
//! ```text
//! epoch_ts \t target \t model \t input_tokens \t output_tokens \t est \t ok
//! ```
//!
//! `est` is `1` when the token counts are an ESTIMATE (CLI shell-outs do not
//! report real usage, so we use `chars / 4` of the prompt and captured output)
//! and `0` when they are real API [`crate::Usage`] counts. `ok` is `1` on a
//! successful invocation and `0` on failure.
//!
//! Writes are strictly best-effort: a ledger failure never propagates to the
//! caller, because recording usage must not break an otherwise-fine invocation.

use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rtrt_core::Config;
use serde::{Deserialize, Serialize};

const LEDGER_FILE_NAME: &str = "provider-usage.tsv";
/// Keep the ledger bounded; on append we truncate to the most-recent rows.
const MAX_LEDGER_ROWS: usize = 5000;
/// CLI shell-outs return only text, so tokens are estimated at ~4 chars/token.
const ESTIMATED_CHARS_PER_TOKEN: u64 = 4;

/// Rolling windows surfaced by [`provider_usage_windows`], in seconds.
const WINDOW_5H_SECS: u64 = 5 * 60 * 60;
const WINDOW_24H_SECS: u64 = 24 * 60 * 60;
const WINDOW_7D_SECS: u64 = 7 * 24 * 60 * 60;

/// One parsed ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    pub epoch_ts: u64,
    pub target: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated: bool,
    pub ok: bool,
}

impl LedgerRow {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    fn to_tsv_line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.epoch_ts,
            self.target,
            self.model,
            self.input_tokens,
            self.output_tokens,
            u8::from(self.estimated),
            u8::from(self.ok),
        )
    }

    fn parse(line: &str) -> Option<Self> {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() < 7 {
            return None;
        }
        Some(Self {
            epoch_ts: fields[0].trim().parse().ok()?,
            target: normalize_target(fields[1]),
            model: fields[2].to_string(),
            input_tokens: fields[3].trim().parse().ok()?,
            output_tokens: fields[4].trim().parse().ok()?,
            estimated: fields[5].trim() != "0",
            ok: fields[6].trim() != "0",
        })
    }
}

/// Token + request totals inside one rolling window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowUsage {
    pub requests: u64,
    pub tokens: u64,
    /// Requests whose token counts were estimated (CLI shell-outs).
    pub estimated_requests: u64,
}

impl WindowUsage {
    fn add(&mut self, row: &LedgerRow) {
        self.requests = self.requests.saturating_add(1);
        self.tokens = self.tokens.saturating_add(row.total_tokens());
        if row.estimated {
            self.estimated_requests = self.estimated_requests.saturating_add(1);
        }
    }

    /// True when any contributing request carried an estimated token count.
    pub fn has_estimates(&self) -> bool {
        self.estimated_requests > 0
    }
}

/// Per-target usage across the 5h / 24h / 7d rolling windows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetWindows {
    pub last_5h: WindowUsage,
    pub last_24h: WindowUsage,
    pub last_7d: WindowUsage,
}

/// Windowed headroom for one configured `[limits]` target.
///
/// The cap is the `[limits]` daily one applied against the 24h window. Targets
/// with no `[limits]` entry are reported with `limit_tokens`/`request_limit` as
/// `None` — we never fabricate a ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetHeadroom {
    pub used_tokens: u64,
    pub limit_tokens: Option<u64>,
    pub remaining_tokens: Option<u64>,
    pub used_requests: u64,
    pub request_limit: Option<u64>,
    pub remaining_requests: Option<u64>,
    /// Any of the contributing 24h rows used an estimated token count.
    pub tokens_estimated: bool,
}

impl TargetHeadroom {
    /// True when neither a token nor a request limit is configured.
    pub fn limits_unknown(&self) -> bool {
        self.limit_tokens.is_none() && self.request_limit.is_none()
    }
}

/// Append one invocation to the ledger. Best-effort: returns the row that was
/// written on success, or `None` if the write was skipped or failed (the caller
/// must never treat this as a hard error).
pub fn record_invocation(
    target: &str,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    est: bool,
    ok: bool,
) -> Option<LedgerRow> {
    let row = LedgerRow {
        epoch_ts: now_epoch_secs(),
        target: normalize_target(target),
        model: model.to_string(),
        input_tokens,
        output_tokens,
        estimated: est,
        ok,
    };
    let path = ledger_path();
    if append_row(&path, &row).is_err() {
        return None;
    }
    // Cap the file on a best-effort basis; ignore any trim failure.
    let _ = trim_to_cap(&path, MAX_LEDGER_ROWS);
    Some(row)
}

/// Estimate token count for a CLI text body (`chars / 4`, rounded up).
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(ESTIMATED_CHARS_PER_TOKEN)
}

/// Read the ledger and bucket every target into the 5h / 24h / 7d windows.
pub fn provider_usage_windows() -> BTreeMap<String, TargetWindows> {
    windows_from_rows(&read_rows(&ledger_path()), now_epoch_secs())
}

/// Per-target windowed headroom for the relevant (24h) window, using the
/// `[limits]` daily caps from `config`. Every target seen in the ledger is
/// included; targets configured in `[limits]` but unseen are also included so a
/// configured cap is always visible. Targets with no `[limits]` entry report
/// `None` limits rather than a fabricated cap.
pub fn target_headroom(config: &Config) -> BTreeMap<String, TargetHeadroom> {
    let windows = provider_usage_windows();
    let mut out = BTreeMap::new();
    let mut targets = windows.keys().cloned().collect::<Vec<_>>();
    for name in config.limits.targets.keys() {
        targets.push(normalize_target(name));
    }
    targets.sort();
    targets.dedup();
    for target in targets {
        out.insert(target.clone(), headroom_for(&target, &windows, config));
    }
    out
}

/// Headroom for a single target. Public so the router can query one candidate
/// without materializing the whole map.
pub fn headroom_for_target(target: &str, config: &Config) -> TargetHeadroom {
    let windows = provider_usage_windows();
    headroom_for(&normalize_target(target), &windows, config)
}

fn headroom_for(
    target: &str,
    windows: &BTreeMap<String, TargetWindows>,
    config: &Config,
) -> TargetHeadroom {
    let window = windows.get(target).copied().unwrap_or_default().last_24h;
    let limit = config.limits.target(target);
    let limit_tokens = limit.and_then(|limit| limit.daily_tokens);
    let request_limit = limit.and_then(|limit| limit.daily_requests);
    TargetHeadroom {
        used_tokens: window.tokens,
        limit_tokens,
        remaining_tokens: limit_tokens.map(|limit| limit.saturating_sub(window.tokens)),
        used_requests: window.requests,
        request_limit,
        remaining_requests: request_limit.map(|limit| limit.saturating_sub(window.requests)),
        tokens_estimated: window.has_estimates(),
    }
}

fn windows_from_rows(rows: &[LedgerRow], now: u64) -> BTreeMap<String, TargetWindows> {
    let mut out: BTreeMap<String, TargetWindows> = BTreeMap::new();
    for row in rows {
        // Future-dated rows (clock skew) are treated as "just now" via
        // saturating_sub, so they still count toward the recent windows.
        let age = now.saturating_sub(row.epoch_ts);
        let entry = out.entry(row.target.clone()).or_default();
        if age <= WINDOW_5H_SECS {
            entry.last_5h.add(row);
        }
        if age <= WINDOW_24H_SECS {
            entry.last_24h.add(row);
        }
        if age <= WINDOW_7D_SECS {
            entry.last_7d.add(row);
        }
    }
    out
}

fn append_row(path: &Path, row: &LedgerRow) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", row.to_tsv_line())
}

fn trim_to_cap(path: &Path, cap: usize) -> std::io::Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let lines = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if lines.len() <= cap {
        return Ok(());
    }
    let start = lines.len() - cap;
    let mut kept = lines[start..].join("\n");
    kept.push('\n');
    std::fs::write(path, kept)
}

fn read_rows(path: &Path) -> Vec<LedgerRow> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(LedgerRow::parse)
        .collect()
}

fn ledger_path() -> PathBuf {
    if let Some(custom) = std::env::var_os("RTRT_PROVIDER_USAGE_PATH") {
        return PathBuf::from(custom);
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rtrt")
        .join(LEDGER_FILE_NAME)
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn normalize_target(target: &str) -> String {
    target.trim().to_ascii_lowercase()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(target: &str, age_secs: u64, tokens_in: u64, tokens_out: u64, est: bool) -> LedgerRow {
        LedgerRow {
            epoch_ts: 1_000_000 - age_secs,
            target: normalize_target(target),
            model: "m".to_string(),
            input_tokens: tokens_in,
            output_tokens: tokens_out,
            estimated: est,
            ok: true,
        }
    }

    #[test]
    fn estimate_tokens_rounds_up_quarter_chars() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn tsv_round_trips() {
        let r = row("Ollama", 10, 100, 50, true);
        let parsed = LedgerRow::parse(&r.to_tsv_line()).expect("parse");
        assert_eq!(parsed, r);
        // target was normalized to lowercase on construction.
        assert_eq!(parsed.target, "ollama");
    }

    #[test]
    fn windows_bucket_by_age() {
        let now = 1_000_000;
        let rows = vec![
            row("ollama", 60, 10, 10, true),                    // in all windows
            row("ollama", WINDOW_5H_SECS + 1, 100, 0, true),    // not in 5h, in 24h/7d
            row("ollama", WINDOW_24H_SECS + 1, 1000, 0, false), // only in 7d
        ];
        let windows = windows_from_rows(&rows, now);
        let ollama = windows.get("ollama").expect("ollama present");
        assert_eq!(ollama.last_5h.requests, 1);
        assert_eq!(ollama.last_5h.tokens, 20);
        assert_eq!(ollama.last_24h.requests, 2);
        assert_eq!(ollama.last_24h.tokens, 20 + 100);
        assert_eq!(ollama.last_7d.requests, 3);
        assert_eq!(ollama.last_7d.tokens, 20 + 100 + 1000);
        assert!(ollama.last_5h.has_estimates());
    }

    #[test]
    fn headroom_uses_24h_window_against_daily_limit() {
        let mut config = Config::default();
        config.limits.targets.insert(
            "openai".to_string(),
            rtrt_core::TargetLimit {
                daily_tokens: Some(1000),
                daily_requests: Some(10),
            },
        );
        let now = 1_000_000;
        let rows = vec![
            row("openai", 60, 100, 50, false),
            row("openai", WINDOW_24H_SECS + 5, 100000, 0, false), // outside 24h, ignored
        ];
        let windows = windows_from_rows(&rows, now);
        let headroom = headroom_for("openai", &windows, &config);
        assert_eq!(headroom.used_tokens, 150);
        assert_eq!(headroom.limit_tokens, Some(1000));
        assert_eq!(headroom.remaining_tokens, Some(850));
        assert_eq!(headroom.used_requests, 1);
        assert_eq!(headroom.request_limit, Some(10));
        assert_eq!(headroom.remaining_requests, Some(9));
        assert!(!headroom.limits_unknown());
        assert!(!headroom.tokens_estimated);
    }

    #[test]
    fn headroom_without_limits_does_not_fabricate_a_cap() {
        let config = Config::default();
        let now = 1_000_000;
        let rows = vec![row("ollama", 60, 100, 50, true)];
        let windows = windows_from_rows(&rows, now);
        let headroom = headroom_for("ollama", &windows, &config);
        assert_eq!(headroom.used_tokens, 150);
        assert_eq!(headroom.limit_tokens, None);
        assert_eq!(headroom.remaining_tokens, None);
        assert_eq!(headroom.request_limit, None);
        assert_eq!(headroom.remaining_requests, None);
        assert!(headroom.limits_unknown());
        assert!(headroom.tokens_estimated);
    }
}
