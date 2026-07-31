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
//!
//! # Pools
//!
//! The row format is unchanged, but the `model` column is no longer inert: one
//! target routinely fronts several unrelated upstream quotas, and the model
//! prefix names which one ([`rtrt_core::PoolKey`]). [`pool_usage_windows`]
//! buckets by that finer identity; [`provider_usage_windows`] is the same data
//! folded back to target level, so every pre-existing caller — and every row
//! already on disk — behaves exactly as before.
//!
//! # Provider-reported rate limits
//!
//! A configured `[limits]` cap is the owner's *belief* about a quota. The
//! provider ships the actual numbers in the rate-limit headers of every HTTP
//! response, and rtrt used to discard them. [`record_response_rate_limit`]
//! keeps them in a sibling append-only file, `provider-ratelimit.tsv`, in the
//! same directory and under the same advisory-lock / row-cap / trim discipline
//! as the ledger itself.
//!
//! A sibling file rather than extra ledger columns, because a signal is
//! per-RESPONSE, not per-invocation: it arrives on rejections too (a 429 is
//! exactly when the numbers matter), it can arrive when no tokens were spent,
//! and widening the seven-field row would have invalidated every row already on
//! disk.
//!
//! What is recorded outranks what is configured — [`CapScope::Reported`] beats
//! [`CapScope::Pool`]/[`CapScope::Shared`], which beat the usage-derived
//! ordering of [`RoomBasis::ObservedUsage`] — but only while it is still
//! current. Past the reset instant the provider itself named, the number has
//! already refilled: that is history, not headroom.

use std::{
    cmp::Ordering,
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use reqwest::header::HeaderMap;
use rtrt_core::{Config, PoolKey};
use serde::{Deserialize, Serialize};

const LEDGER_FILE_NAME: &str = "provider-usage.tsv";
/// Keep the ledger bounded; on append we truncate to the most-recent rows.
const MAX_LEDGER_ROWS: usize = 5000;
/// CLI shell-outs return only text, so tokens are estimated at ~4 chars/token.
const ESTIMATED_CHARS_PER_TOKEN: u64 = 4;
/// A `.lock` file older than this is treated as leftover from a crashed
/// process and stolen — an append + trim takes milliseconds, never seconds.
const LOCK_STALE_SECS: u64 = 10;
/// How many times to retry lock acquisition (with [`LOCK_RETRY_WAIT`] between
/// attempts) before falling back to an untrimmed append. Bounded so recording
/// can never block a caller for long.
const LOCK_RETRY_ATTEMPTS: u32 = 5;
const LOCK_RETRY_WAIT: std::time::Duration = std::time::Duration::from_millis(10);

/// Rolling windows surfaced by [`provider_usage_windows`], in seconds.
const WINDOW_5H_SECS: u64 = 5 * 60 * 60;
const WINDOW_24H_SECS: u64 = 24 * 60 * 60;
const WINDOW_7D_SECS: u64 = 7 * 24 * 60 * 60;

/// Sibling of the usage ledger holding provider-reported rate-limit signals.
/// Shares the ledger's directory, lock discipline and [`MAX_LEDGER_ROWS`] cap.
const RATELIMIT_FILE_NAME: &str = "provider-ratelimit.tsv";
/// Written where the provider reported no number. Deliberately not `0`: an
/// absent header is "unknown", never "exhausted".
const ABSENT_FIELD: &str = "-";

/// Unit conversions for the duration formats rate-limit headers use.
const MILLIS_PER_SEC: u64 = 1_000;
const SECS_PER_MINUTE: f64 = 60.0;
const SECS_PER_HOUR: f64 = 3_600.0;
/// Seconds per day, for resolving an RFC 3339 reset instant to an epoch second.
const SECS_PER_DAY: i64 = 86_400;

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

    /// The quota bucket this row actually drew from: its target, plus the pool
    /// named by the model prefix when there is one.
    pub fn pool_key(&self) -> PoolKey {
        PoolKey::from_target_model(&self.target, Some(&self.model))
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

    /// Absorb another window's totals — used to fold sibling pools back into
    /// the target they share.
    fn merge(&mut self, other: &Self) {
        self.requests = self.requests.saturating_add(other.requests);
        self.tokens = self.tokens.saturating_add(other.tokens);
        self.estimated_requests = self
            .estimated_requests
            .saturating_add(other.estimated_requests);
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

impl TargetWindows {
    fn merge(&mut self, other: &Self) {
        self.last_5h.merge(&other.last_5h);
        self.last_24h.merge(&other.last_24h);
        self.last_7d.merge(&other.last_7d);
    }
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

/// Where the cap a pool is measured against comes from.
///
/// The distinction matters because a target-level cap with several pools under
/// it is genuinely *shared*: the remaining room is one pot the siblings draw
/// from together. rtrt reports it as such instead of splitting the cap into
/// invented per-pool slices.
///
/// It matters again because the numbers have different provenance, and a human
/// reading `rtrt route --explain` has to be able to tell them apart. In
/// descending order of trust:
///
/// 1. [`CapScope::Reported`] — the provider's own rate-limit headers, i.e. a
///    reading of the quota.
/// 2. [`CapScope::Pool`] / [`CapScope::Shared`] — a configured `[limits]`
///    ceiling, i.e. a human's belief about the quota.
/// 3. [`CapScope::Unknown`] — nothing is known, and nothing is invented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapScope {
    /// The provider itself reported this pool's remaining quota, in the
    /// rate-limit headers of a response rtrt actually received, and that report
    /// has not yet passed the reset instant it named.
    ///
    /// `limit` / `remaining` are the provider's numbers verbatim. `used` is
    /// `limit - remaining` when a ceiling came with them — arithmetic on two
    /// reported numbers — and otherwise rtrt's own observed usage, which is
    /// measured rather than inferred.
    Reported,
    /// `[limits.<target>.pools.<pool>]` — this pool's own ceiling. `used` is
    /// this pool's usage alone.
    Pool,
    /// No pool cap, but `[limits.<target>]` sets one: every pool under the
    /// target draws on it. `used` is the target-wide total (all siblings), so
    /// `remaining` is the shared room, not a per-pool entitlement.
    Shared,
    /// No cap at either level. Nothing is known — no ceiling is fabricated.
    Unknown,
}

impl CapScope {
    /// True when a real ceiling backs this axis (either scope that is not
    /// [`CapScope::Unknown`]).
    pub fn is_capped(self) -> bool {
        !matches!(self, Self::Unknown)
    }

    /// True when the ceiling is shared with sibling pools.
    pub fn is_shared(self) -> bool {
        matches!(self, Self::Shared)
    }

    /// True when the number came from the provider rather than from config.
    pub fn is_reported(self) -> bool {
        matches!(self, Self::Reported)
    }

    /// How a human should read this axis. Spelled out per scope so a real
    /// provider number, a configured guess and "nothing is known" can never be
    /// mistaken for one another on a `--explain` line.
    pub fn label(self) -> &'static str {
        match self {
            Self::Reported => {
                "provider-reported (remaining quota from the provider's own rate-limit headers)"
            }
            Self::Pool => {
                "configured (this pool's own [limits] cap — a set belief, not a measurement)"
            }
            Self::Shared => {
                "configured (the target's [limits] cap, shared with every sibling pool)"
            }
            Self::Unknown => "unknown (nothing reported by the provider, nothing configured)",
        }
    }
}

/// One capped axis (tokens or requests) of a pool's 24h headroom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolCap {
    pub scope: CapScope,
    /// The configured ceiling; `None` when [`CapScope::Unknown`].
    pub limit: Option<u64>,
    /// Usage measured against `limit`: this pool's own for [`CapScope::Pool`],
    /// the target-wide total for [`CapScope::Shared`] (because that is what the
    /// shared cap is actually being drawn down by).
    pub used: u64,
    /// `limit - used`, saturating; `None` when no cap is configured.
    pub remaining: Option<u64>,
}

impl PoolCap {
    fn unknown(used: u64) -> Self {
        Self {
            scope: CapScope::Unknown,
            limit: None,
            used,
            remaining: None,
        }
    }

    fn capped(scope: CapScope, limit: Option<u64>, used: u64) -> Self {
        match limit {
            Some(limit) => Self {
                scope,
                limit: Some(limit),
                used,
                remaining: Some(limit.saturating_sub(used)),
            },
            None => Self::unknown(used),
        }
    }

    /// A ceiling the provider reported for itself.
    ///
    /// `remaining` is the provider's number verbatim. `used` is
    /// `limit - remaining` when a ceiling came with it, and otherwise the usage
    /// rtrt observed — never a derived stand-in for the ceiling. A reported
    /// remaining with no reported ceiling therefore has no fraction, and
    /// [`PoolCap::remaining_fraction`] stays `None` rather than inventing a
    /// denominator.
    fn reported(limit: Option<u64>, remaining: u64, observed_used: u64) -> Self {
        Self {
            scope: CapScope::Reported,
            limit,
            used: limit.map_or(observed_used, |limit| limit.saturating_sub(remaining)),
            remaining: Some(remaining),
        }
    }

    /// Fraction of the ceiling still available, in `0.0..=1.0`. `None` when no
    /// cap is configured — an unknown ceiling has no fraction, and guessing one
    /// would fabricate a number.
    ///
    /// A zero limit is fully consumed by definition, so it reports `0.0` rather
    /// than dividing by zero.
    pub fn remaining_fraction(&self) -> Option<f64> {
        let limit = self.limit?;
        let remaining = self.remaining.unwrap_or(0);
        if limit == 0 {
            return Some(0.0);
        }
        Some(remaining as f64 / limit as f64)
    }
}

/// 24h headroom for one pool, per axis.
///
/// `used_tokens` / `used_requests` are always this pool's own slice. What the
/// caps mean is spelled out per axis by [`PoolCap::scope`], so a shared target
/// cap can never be mistaken for a per-pool entitlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolHeadroom {
    /// [`PoolKey::canonical`] of this bucket.
    pub key: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    pub used_tokens: u64,
    pub used_requests: u64,
    /// Any contributing 24h row carried an estimated token count.
    pub tokens_estimated: bool,
    pub tokens: PoolCap,
    pub requests: PoolCap,
    /// How many pools (including this one) drew on the same target in the 24h
    /// window. `> 1` with a [`CapScope::Shared`] axis means the ceiling is
    /// actively contended.
    pub sibling_pools: u64,
}

impl PoolHeadroom {
    /// True when neither axis has a configured ceiling.
    pub fn limits_unknown(&self) -> bool {
        !self.tokens.scope.is_capped() && !self.requests.scope.is_capped()
    }

    /// True when any configured ceiling is shared with sibling pools.
    pub fn shares_a_cap(&self) -> bool {
        self.tokens.scope.is_shared() || self.requests.scope.is_shared()
    }

    /// The binding constraint: the smallest remaining fraction across the
    /// capped axes. `None` when no axis is capped.
    pub fn room_fraction(&self) -> Option<f64> {
        match (
            self.tokens.remaining_fraction(),
            self.requests.remaining_fraction(),
        ) {
            (Some(tokens), Some(requests)) => Some(tokens.min(requests)),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        }
    }

    /// The provenance of the axis [`PoolHeadroom::room_fraction`] is actually
    /// decided by — the binding (smallest-fraction) capped axis. `None` when no
    /// axis is capped.
    pub fn room_scope(&self) -> Option<CapScope> {
        match (
            self.tokens.remaining_fraction(),
            self.requests.remaining_fraction(),
        ) {
            (Some(tokens), Some(requests)) => Some(if tokens <= requests {
                self.tokens.scope
            } else {
                self.requests.scope
            }),
            (Some(_), None) => Some(self.tokens.scope),
            (None, Some(_)) => Some(self.requests.scope),
            (None, None) => None,
        }
    }

    /// True when either axis rests on a number the provider reported rather
    /// than on a configured cap.
    pub fn has_reported_quota(&self) -> bool {
        self.tokens.scope.is_reported() || self.requests.scope.is_reported()
    }

    /// The structured identity behind [`PoolHeadroom::key`].
    pub fn pool_key(&self) -> PoolKey {
        PoolKey::new(&self.target, self.pool.as_deref())
    }
}

/// What a room ranking was actually derived from.
///
/// Callers must not read an [`RoomBasis::ObservedUsage`] ordering as quota
/// information — it says "this pool has been used less lately", not "this pool
/// has more quota left", which is unknowable without a configured cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomBasis {
    /// Every compared pool had a real cap: ranked by remaining fraction.
    ///
    /// `RoomBasis` names the *kind* of ordering only. The cap behind it may be
    /// provider-reported or configured; which one it was lives on each axis's
    /// [`CapScope`], and is summarised by [`PoolRanking::cap_scope`] /
    /// [`PoolRanking::basis_label`].
    Quota,
    /// At least one pool had no cap: ranked by observed 24h usage, ascending.
    ObservedUsage,
}

impl RoomBasis {
    pub fn label(self) -> &'static str {
        match self {
            Self::Quota => "quota-derived (remaining fraction of a configured cap)",
            Self::ObservedUsage => "usage-derived (observed 24h usage, not a quota measurement)",
        }
    }
}

/// A room comparison plus the basis it was decided on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomComparison {
    /// `Greater` when the left pool has more room than the right.
    pub order: Ordering,
    pub basis: RoomBasis,
}

/// Pools ordered most-room-first, labelled with what the order means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolRanking {
    pub basis: RoomBasis,
    pub ranked: Vec<PoolHeadroom>,
}

impl PoolRanking {
    /// The pool with the most room, if any.
    pub fn best(&self) -> Option<&PoolHeadroom> {
        self.ranked.first()
    }

    /// The cap provenance every ranked pool shares, or `None` when the set is
    /// empty, uncapped, or mixed. `Some(CapScope::Reported)` is the strongest
    /// reading available: every number in this order came from the providers
    /// themselves.
    pub fn cap_scope(&self) -> Option<CapScope> {
        let mut scopes = self.ranked.iter().map(PoolHeadroom::room_scope);
        let first = scopes.next()??;
        scopes.all(|scope| scope == Some(first)).then_some(first)
    }

    /// What this order means *and* where its numbers came from, in one line.
    ///
    /// [`RoomBasis::label`] states only the former, so on its own a
    /// provider-reported remaining would read exactly like a configured guess.
    pub fn basis_label(&self) -> String {
        match self.cap_scope() {
            Some(scope) if scope.is_capped() => {
                format!("{} — {}", self.basis.label(), scope.label())
            }
            _ => self.basis.label().to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Provider-reported rate-limit signals
// ---------------------------------------------------------------------------

/// The header names carrying one fact about one axis, in every dialect rtrt
/// speaks. Anthropic and OpenAI (and OpenAI-compatible servers that bother)
/// name the same three facts differently; both spellings are checked for every
/// axis, which is what makes the two dialects parse into one record.
const REQUEST_LIMIT_HEADERS: [&str; 2] = [
    "anthropic-ratelimit-requests-limit",
    "x-ratelimit-limit-requests",
];
const REQUEST_REMAINING_HEADERS: [&str; 2] = [
    "anthropic-ratelimit-requests-remaining",
    "x-ratelimit-remaining-requests",
];
const REQUEST_RESET_HEADERS: [&str; 2] = [
    "anthropic-ratelimit-requests-reset",
    "x-ratelimit-reset-requests",
];
const TOKEN_LIMIT_HEADERS: [&str; 2] = [
    "anthropic-ratelimit-tokens-limit",
    "x-ratelimit-limit-tokens",
];
const TOKEN_REMAINING_HEADERS: [&str; 2] = [
    "anthropic-ratelimit-tokens-remaining",
    "x-ratelimit-remaining-tokens",
];
const TOKEN_RESET_HEADERS: [&str; 2] = [
    "anthropic-ratelimit-tokens-reset",
    "x-ratelimit-reset-tokens",
];
/// Sent by both dialects on a 429 (and, per RFC 9110, by neither otherwise).
const RETRY_AFTER_HEADERS: [&str; 1] = ["retry-after"];

/// One axis — requests or tokens — of what a provider reported about its own
/// rate limit.
///
/// Every field is independently optional: a provider may send a remaining with
/// no ceiling, a ceiling with no reset, or a reset alone. A header that is
/// absent, empty or unparseable leaves its field `None`, never `0` — a zero
/// would read as "fully exhausted", which is a fabrication.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitAxis {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<u64>,
    /// The epoch second the provider says this axis refills at. Durations
    /// (`6m0s`, `250ms`, `30`) are resolved against the observation time;
    /// Anthropic's RFC 3339 instants are absolute already.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<u64>,
}

impl RateLimitAxis {
    /// True when the provider reported nothing at all on this axis.
    pub fn is_empty(&self) -> bool {
        self.limit.is_none() && self.remaining.is_none() && self.reset_at.is_none()
    }

    /// Whether this axis still describes the present.
    ///
    /// Past the reset instant the provider itself named, whatever it reported
    /// has already refilled — presenting it as current would be presenting
    /// history as headroom. When no reset instant was reported the window is
    /// unknowable, so the axis is honoured only inside the same
    /// [`WINDOW_24H_SECS`] window the headroom itself is measured over, and
    /// never longer. No separate staleness constant is invented for it.
    pub fn is_current(&self, observed_at: u64, now: u64) -> bool {
        if self.is_empty() {
            return false;
        }
        match self.reset_at {
            Some(reset_at) => now < reset_at,
            None => now.saturating_sub(observed_at) < WINDOW_24H_SECS,
        }
    }
}

/// Everything one provider HTTP response disclosed about the quota it drew
/// from — the normalized record both dialects parse into.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitSignal {
    /// [`PoolKey::canonical`] of the bucket the response drew from.
    pub key: String,
    /// Epoch second the response carrying these headers was observed. Also the
    /// origin any duration-style reset was resolved against.
    pub observed_at: u64,
    /// HTTP status of that response. Kept because a signal read off a 429 is
    /// worth at least as much as one read off a success, and a reader should be
    /// able to see which it was.
    pub status: u16,
    #[serde(default)]
    pub requests: RateLimitAxis,
    #[serde(default)]
    pub tokens: RateLimitAxis,
    /// `retry-after`, resolved to the epoch second it points at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_at: Option<u64>,
}

impl RateLimitSignal {
    /// True when the response disclosed nothing usable. Such a signal is never
    /// written: once on disk, an all-absent row is indistinguishable from a
    /// fabricated one.
    pub fn is_empty(&self) -> bool {
        self.requests.is_empty() && self.tokens.is_empty() && self.retry_after_at.is_none()
    }

    /// The structured identity behind [`RateLimitSignal::key`].
    pub fn pool_key(&self) -> PoolKey {
        PoolKey::parse(&self.key)
    }

    /// This signal with everything past its own reset instant dropped, or
    /// `None` when nothing about it is still current.
    pub fn current_at(&self, now: u64) -> Option<Self> {
        let mut fresh = self.clone();
        if !fresh.requests.is_current(self.observed_at, now) {
            fresh.requests = RateLimitAxis::default();
        }
        if !fresh.tokens.is_current(self.observed_at, now) {
            fresh.tokens = RateLimitAxis::default();
        }
        fresh.retry_after_at = fresh.retry_after_at.filter(|at| now < *at);
        (!fresh.is_empty()).then_some(fresh)
    }

    /// The epoch second before which the provider asked rtrt not to come back,
    /// when it said so at all.
    pub fn backoff_until(&self) -> Option<u64> {
        self.retry_after_at
    }

    fn to_tsv_line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.observed_at,
            self.key,
            self.status,
            optional_field(self.requests.limit),
            optional_field(self.requests.remaining),
            optional_field(self.requests.reset_at),
            optional_field(self.tokens.limit),
            optional_field(self.tokens.remaining),
            optional_field(self.tokens.reset_at),
            optional_field(self.retry_after_at),
        )
    }

    fn parse(line: &str) -> Option<Self> {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() < 10 {
            return None;
        }
        Some(Self {
            observed_at: fields[0].trim().parse().ok()?,
            key: PoolKey::parse(fields[1]).canonical(),
            status: fields[2].trim().parse().ok()?,
            requests: RateLimitAxis {
                limit: parse_optional_field(fields[3]),
                remaining: parse_optional_field(fields[4]),
                reset_at: parse_optional_field(fields[5]),
            },
            tokens: RateLimitAxis {
                limit: parse_optional_field(fields[6]),
                remaining: parse_optional_field(fields[7]),
                reset_at: parse_optional_field(fields[8]),
            },
            retry_after_at: parse_optional_field(fields[9]),
        })
    }
}

fn optional_field(value: Option<u64>) -> String {
    value.map_or_else(|| ABSENT_FIELD.to_string(), |value| value.to_string())
}

fn parse_optional_field(field: &str) -> Option<u64> {
    let field = field.trim();
    if field == ABSENT_FIELD {
        return None;
    }
    field.parse().ok()
}

/// Read a provider response's rate-limit headers into the normalized record.
///
/// `None` when the response carried nothing usable: an absent, empty or garbage
/// header contributes nothing — never a zero and never a guess. `observed_at`
/// is a parameter rather than "now" so the caller (and the tests) control the
/// origin that duration-style resets resolve against.
pub fn rate_limit_signal(
    key: &PoolKey,
    status: u16,
    observed_at: u64,
    headers: &HeaderMap,
) -> Option<RateLimitSignal> {
    let signal = RateLimitSignal {
        key: key.canonical(),
        observed_at,
        status,
        requests: axis_from_headers(
            headers,
            observed_at,
            &REQUEST_LIMIT_HEADERS,
            &REQUEST_REMAINING_HEADERS,
            &REQUEST_RESET_HEADERS,
        ),
        tokens: axis_from_headers(
            headers,
            observed_at,
            &TOKEN_LIMIT_HEADERS,
            &TOKEN_REMAINING_HEADERS,
            &TOKEN_RESET_HEADERS,
        ),
        retry_after_at: first_header(headers, &RETRY_AFTER_HEADERS)
            .and_then(|raw| parse_reset_instant(raw, observed_at)),
    };
    (!signal.is_empty()).then_some(signal)
}

fn axis_from_headers(
    headers: &HeaderMap,
    observed_at: u64,
    limit: &[&str],
    remaining: &[&str],
    reset: &[&str],
) -> RateLimitAxis {
    RateLimitAxis {
        limit: first_header(headers, limit).and_then(parse_count),
        remaining: first_header(headers, remaining).and_then(parse_count),
        reset_at: first_header(headers, reset)
            .and_then(|raw| parse_reset_instant(raw, observed_at)),
    }
}

/// The first of `names` present with a non-empty, valid-UTF-8 value.
/// [`HeaderMap`] lookups are already case-insensitive.
fn first_header<'a>(headers: &'a HeaderMap, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

/// A whole-number header value. Anything that is not a plain non-negative
/// integer is `None`: a header rtrt cannot read is not a quota of zero.
fn parse_count(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// Resolve a reset / retry-after header to the epoch second it points at.
///
/// Anthropic reports an absolute RFC 3339 instant; OpenAI reports a duration
/// (`6m0s`, `250ms`) or a bare count of seconds. Both normalize to one absolute
/// instant, which is also what makes staleness decidable without a separate
/// staleness constant. Sub-second durations round *up* so that a real, if tiny,
/// window never collapses into "already reset".
fn parse_reset_instant(raw: &str, observed_at: u64) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(epoch) = parse_rfc3339_epoch(raw) {
        return Some(epoch);
    }
    let millis = parse_duration_millis(raw)?;
    Some(observed_at.saturating_add(millis.div_ceil(MILLIS_PER_SEC)))
}

/// Parse a rate-limit duration into milliseconds.
///
/// Accepts a bare number (seconds, e.g. `30`) or a Go-style unit string
/// (`6m0s`, `1s`, `250ms`, `1h30m`). Empty, negative, non-finite, unit-less or
/// unknown-unit input is `None` — garbage must never become a number.
pub(crate) fn parse_duration_millis(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // A bare number is a count of seconds, which is what OpenAI-compatible
    // servers most often send.
    if let Ok(secs) = raw.parse::<f64>() {
        return millis_from_secs(secs);
    }
    let mut total = 0.0_f64;
    let mut rest = raw;
    let mut segments = 0usize;
    while !rest.is_empty() {
        // A trailing number with no unit (`6m0`) is malformed, not "0 of
        // something", so a missing unit boundary rejects the whole value.
        let value_len = rest.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        if value_len == 0 {
            return None;
        }
        let value = rest[..value_len].parse::<f64>().ok()?;
        rest = &rest[value_len..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(rest.len());
        total += value * unit_secs(&rest[..unit_len])?;
        rest = &rest[unit_len..];
        segments += 1;
    }
    (segments > 0).then_some(())?;
    millis_from_secs(total)
}

fn unit_secs(unit: &str) -> Option<f64> {
    Some(match unit {
        "ns" => 1e-9,
        "us" | "\u{b5}s" | "\u{3bc}s" => 1e-6,
        "ms" => 1e-3,
        "s" => 1.0,
        "m" => SECS_PER_MINUTE,
        "h" => SECS_PER_HOUR,
        _ => return None,
    })
}

fn millis_from_secs(secs: f64) -> Option<u64> {
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    let millis = (secs * MILLIS_PER_SEC as f64).round();
    (millis <= u64::MAX as f64).then_some(millis as u64)
}

/// Parse an RFC 3339 timestamp — `2026-07-31T12:34:56Z`, optional fractional
/// seconds, `Z` or `\u{b1}HH:MM` offset — into an epoch second.
///
/// Hand-rolled rather than pulling a date crate into a provider adapter for one
/// header. `None` on anything malformed, which is what routes a value like
/// `6m0s` on to the duration parser.
pub(crate) fn parse_rfc3339_epoch(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let bytes = raw.as_bytes();
    if bytes.len() < 19 || !raw.is_ascii() {
        return None;
    }
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    if !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    let year = ascii_number(&raw[0..4])?;
    let month = ascii_number(&raw[5..7])?;
    let day = ascii_number(&raw[8..10])?;
    let hour = ascii_number(&raw[11..13])?;
    let minute = ascii_number(&raw[14..16])?;
    // A leap second (`:60`) is clamped rather than rejected — the instant it
    // names is real even though the arithmetic below has no room for it.
    let second = ascii_number(&raw[17..19])?.min(59);
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || minute > 59 {
        return None;
    }

    let mut rest = &raw[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(fraction.len());
        if digits == 0 {
            return None;
        }
        rest = &fraction[digits..];
    }
    let offset_secs = match rest.as_bytes().first() {
        // No offset at all is not strictly RFC 3339, but reading it as UTC is
        // the only interpretation that does not silently discard a real number.
        None => 0,
        Some(b'Z' | b'z') if rest.len() == 1 => 0,
        Some(sign @ (b'+' | b'-')) => {
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            let offset_hour = ascii_number(&rest[1..3])?;
            let offset_minute = ascii_number(&rest[4..6])?;
            if offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let magnitude = offset_hour * SECS_PER_HOUR as i64 + offset_minute * 60;
            if *sign == b'+' { magnitude } else { -magnitude }
        }
        _ => return None,
    };

    let epoch = days_from_civil(year, month, day) * SECS_PER_DAY
        + hour * SECS_PER_HOUR as i64
        + minute * 60
        + second
        - offset_secs;
    u64::try_from(epoch).ok()
}

fn ascii_number(raw: &str) -> Option<i64> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days between 1970-01-01 and the given proleptic-Gregorian date (Hinnant's
/// `days_from_civil`), exact for every date this parser accepts.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Record what one provider response said about its own rate limit.
///
/// Best-effort by exactly the same contract as [`record_invocation`]: the same
/// bounded advisory lock, one appended line, the same [`MAX_LEDGER_ROWS`] trim,
/// and failure reported as `false` rather than as an error the request path
/// could ever surface. A signal with nothing in it is not written at all.
pub fn record_rate_limit(signal: &RateLimitSignal) -> bool {
    record_rate_limit_at(&ratelimit_path(), signal)
}

fn record_rate_limit_at(path: &Path, signal: &RateLimitSignal) -> bool {
    if signal.is_empty() {
        return false;
    }
    // Same rationale as the ledger: trim_to_cap is a read-rewrite, so a
    // concurrent append during a trim could be lost. When the lock cannot be
    // taken within the bounded retries we still append and skip only the trim.
    let lock = LedgerLock::try_acquire(lock_path(path), LOCK_STALE_SECS);
    if append_line(path, &signal.to_tsv_line()).is_err() {
        return false;
    }
    if lock.is_some() {
        let _ = trim_to_cap(path, MAX_LEDGER_ROWS);
    }
    true
}

/// Parse a provider response's rate-limit headers and record them.
///
/// The entry point every adapter calls, on the success *and* the error path.
/// Returns the signal that was written, or `None` when the response disclosed
/// nothing or the write was skipped — never an error.
pub fn record_response_rate_limit(
    key: &PoolKey,
    status: u16,
    headers: &HeaderMap,
) -> Option<RateLimitSignal> {
    let signal = rate_limit_signal(key, status, now_epoch_secs(), headers)?;
    record_rate_limit(&signal).then_some(signal)
}

/// The newest signal recorded for each pool, whether or not it is still
/// current — [`RateLimitSignal::current_at`] is what decides that.
pub fn rate_limit_signals() -> BTreeMap<String, RateLimitSignal> {
    latest_signals(&read_signals(&ratelimit_path()))
}

/// The newest signal recorded for one pool.
pub fn rate_limit_signal_for(key: &PoolKey) -> Option<RateLimitSignal> {
    rate_limit_signals().remove(&key.canonical())
}

/// Collapse the append-only log to one row per pool: newest observation wins,
/// and within one second the later row wins, because file order is the only
/// further ordering there is.
fn latest_signals(rows: &[RateLimitSignal]) -> BTreeMap<String, RateLimitSignal> {
    let mut out: BTreeMap<String, RateLimitSignal> = BTreeMap::new();
    for row in rows {
        match out.get(&row.key) {
            Some(seen) if seen.observed_at > row.observed_at => {}
            _ => {
                out.insert(row.key.clone(), row.clone());
            }
        }
    }
    out
}

fn read_signals(path: &Path) -> Vec<RateLimitSignal> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(RateLimitSignal::parse)
        .collect()
}

/// Sibling of the usage ledger, so a redirected ledger (tests, sandboxes) takes
/// its rate-limit signals with it.
fn ratelimit_path() -> PathBuf {
    if let Some(custom) = std::env::var_os("RTRT_PROVIDER_RATELIMIT_PATH") {
        return PathBuf::from(custom);
    }
    ledger_path().with_file_name(RATELIMIT_FILE_NAME)
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
    // Serialize writers across concurrent rtrt processes: trim_to_cap is a
    // read-rewrite, so a concurrent append during a trim could be lost. The
    // lock is best-effort — when it cannot be acquired within the bounded
    // retries we still append (recording must never be dropped or block) and
    // only skip the trim; the next successful writer trims the backlog.
    let lock = LedgerLock::try_acquire(lock_path(&path), LOCK_STALE_SECS);
    if append_row(&path, &row).is_err() {
        return None;
    }
    if lock.is_some() {
        // Cap the file on a best-effort basis; ignore any trim failure.
        let _ = trim_to_cap(&path, MAX_LEDGER_ROWS);
    }
    Some(row)
}

/// Best-effort advisory lock: an `O_EXCL`-created sibling `.lock` file, removed
/// on drop. Dependency-light by design — no OS advisory-lock crate. A lock file
/// whose mtime is older than `stale_secs` is treated as leftover from a crashed
/// process and stolen.
struct LedgerLock {
    path: PathBuf,
}

impl LedgerLock {
    fn try_acquire(path: PathBuf, stale_secs: u64) -> Option<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        for attempt in 0..LOCK_RETRY_ATTEMPTS {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Some(Self { path }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path, stale_secs) {
                        // Steal the abandoned lock and retry the exclusive
                        // create immediately (another process may win the
                        // race — that is fine, we just keep retrying).
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if attempt + 1 < LOCK_RETRY_ATTEMPTS {
                        std::thread::sleep(LOCK_RETRY_WAIT);
                    }
                }
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for LedgerLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lock_is_stale(path: &Path, stale_secs: u64) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age.as_secs() >= stale_secs)
}

fn lock_path(ledger: &Path) -> PathBuf {
    let mut name = ledger
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from(LEDGER_FILE_NAME));
    name.push(".lock");
    ledger.with_file_name(name)
}

/// Estimate token count for a CLI text body (`chars / 4`, rounded up).
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(ESTIMATED_CHARS_PER_TOKEN)
}

/// Read the ledger and bucket every target into the 5h / 24h / 7d windows.
///
/// Target-level view: sibling pools inside one target are summed. This is the
/// fold of [`pool_usage_windows`], so the two views can never disagree.
pub fn provider_usage_windows() -> BTreeMap<String, TargetWindows> {
    fold_pools_to_targets(&pool_usage_windows())
}

/// Read the ledger and bucket every *pool* into the 5h / 24h / 7d windows.
///
/// Keyed by [`PoolKey::canonical`] (`opencode#opencode-go`), so two models that
/// reach different upstream backends through one target no longer share a
/// bucket. Rows whose model carries no pool prefix key by the bare target, which
/// is every row written before pools existed.
pub fn pool_usage_windows() -> BTreeMap<String, TargetWindows> {
    pool_windows_from_rows(&read_rows(&ledger_path()), now_epoch_secs())
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

/// Per-pool windowed headroom for the 24h window.
///
/// Every pool seen in the ledger is included, plus every pool that carries a
/// configured `[limits.<target>.pools.<pool>]` cap (so a configured ceiling is
/// always visible even before its first invocation). Target-level visibility is
/// unchanged and still comes from [`target_headroom`].
pub fn pool_headroom(config: &Config) -> BTreeMap<String, PoolHeadroom> {
    pool_headroom_from_parts(
        &pool_usage_windows(),
        config,
        &rate_limit_signals(),
        now_epoch_secs(),
    )
}

/// Headroom for a single pool. Public so a caller can query one candidate
/// without materializing the whole map.
pub fn headroom_for_pool(key: &PoolKey, config: &Config) -> PoolHeadroom {
    let pooled = pool_usage_windows();
    let targets = fold_pools_to_targets(&pooled);
    headroom_for_pool_in(
        key,
        &pooled,
        &targets,
        config,
        &rate_limit_signals(),
        now_epoch_secs(),
    )
}

/// The pools of one target, ranked most-room-first — the "which sibling should
/// take the next task" question, answered without inventing numbers.
///
/// See [`rank_pools_by_room`] for what the ranking is derived from.
pub fn rank_target_pools(target: &str, config: &Config) -> PoolRanking {
    let target = normalize_target(target);
    let pools = pool_headroom(config)
        .into_values()
        .filter(|headroom| headroom.target == target)
        .collect::<Vec<_>>();
    rank_pools_by_room(&pools)
}

/// Same inputs the ledger has always used, plus whatever the providers
/// themselves reported. With no signals on disk the extra argument is empty and
/// every number is what it was before.
fn pool_headroom_from_parts(
    pooled: &BTreeMap<String, TargetWindows>,
    config: &Config,
    signals: &BTreeMap<String, RateLimitSignal>,
    now: u64,
) -> BTreeMap<String, PoolHeadroom> {
    let targets = fold_pools_to_targets(pooled);
    let mut keys = pooled
        .keys()
        .map(|key| PoolKey::parse(key))
        .collect::<Vec<_>>();
    for (target, limit) in &config.limits.targets {
        for (pool, cap) in &limit.pools {
            if cap.is_set() {
                keys.push(PoolKey::new(target, Some(pool)));
            }
        }
    }
    // A pool the provider has reported on is visible even with no ledger rows
    // yet — the report is the strongest thing rtrt knows about it.
    keys.extend(signals.keys().map(|key| PoolKey::parse(key)));
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .map(|key| {
            (
                key.canonical(),
                headroom_for_pool_in(&key, pooled, &targets, config, signals, now),
            )
        })
        .collect()
}

/// The pre-signal call shape, kept so the tests that pin the configured-cap
/// behaviour exercise exactly the path they always did.
#[cfg(test)]
fn pool_headroom_from_windows(
    pooled: &BTreeMap<String, TargetWindows>,
    config: &Config,
) -> BTreeMap<String, PoolHeadroom> {
    pool_headroom_from_parts(pooled, config, &BTreeMap::new(), now_epoch_secs())
}

fn headroom_for_pool_in(
    key: &PoolKey,
    pooled: &BTreeMap<String, TargetWindows>,
    targets: &BTreeMap<String, TargetWindows>,
    config: &Config,
    signals: &BTreeMap<String, RateLimitSignal>,
    now: u64,
) -> PoolHeadroom {
    let window = pooled
        .get(&key.canonical())
        .copied()
        .unwrap_or_default()
        .last_24h;
    let target_window = targets
        .get(key.target_key())
        .copied()
        .unwrap_or_default()
        .last_24h;
    let target_limit = config.limits.target(key.target_key());
    // A pool cap only counts when it actually pins an axis; an empty
    // `[limits.<t>.pools.<p>]` table is not a ceiling.
    let pool_limit = key
        .pool()
        .and_then(|pool| target_limit.and_then(|limit| limit.pool(pool)))
        .filter(|limit| limit.is_set());

    // The provider's own reading of this exact pool, with anything past its
    // reset instant already dropped. Never inherited from the target: a
    // rate-limit header describes the bucket the response drew from, and
    // spreading it over siblings would be a guess.
    let reported = signals
        .get(&key.canonical())
        .and_then(|signal| signal.current_at(now))
        .unwrap_or_default();

    // Resolved per axis: a pool may pin tokens while inheriting a shared
    // request cap, and collapsing that into one scope would misreport one of
    // them.
    let tokens = axis_cap_with_reported(
        reported.tokens,
        pool_limit.and_then(|limit| limit.daily_tokens),
        target_limit.and_then(|limit| limit.daily_tokens),
        window.tokens,
        target_window.tokens,
    );
    let requests = axis_cap_with_reported(
        reported.requests,
        pool_limit.and_then(|limit| limit.daily_requests),
        target_limit.and_then(|limit| limit.daily_requests),
        window.requests,
        target_window.requests,
    );

    PoolHeadroom {
        key: key.canonical(),
        target: key.target.clone(),
        pool: key.pool().map(str::to_string),
        used_tokens: window.tokens,
        used_requests: window.requests,
        tokens_estimated: window.has_estimates(),
        tokens,
        requests,
        sibling_pools: sibling_pool_count(key.target_key(), pooled),
    }
}

/// [`axis_cap`] with the provider's own reading of this axis taking precedence.
///
/// Trust order, highest first: a still-current provider-reported `remaining`,
/// then a configured `[limits]` cap (this pool's, else the target's shared
/// one), then nothing. A reported axis with no `remaining` is not a ceiling — a
/// bare limit or reset says nothing about what is left — so it falls through to
/// config rather than being dressed up as one.
pub(crate) fn axis_cap_with_reported(
    reported: RateLimitAxis,
    pool_limit: Option<u64>,
    target_limit: Option<u64>,
    pool_used: u64,
    target_used: u64,
) -> PoolCap {
    match reported.remaining {
        Some(remaining) => PoolCap::reported(reported.limit, remaining, pool_used),
        None => axis_cap(pool_limit, target_limit, pool_used, target_used),
    }
}

/// One axis of a pool's cap: its own ceiling if configured, else the target's
/// (shared with every sibling), else nothing.
pub(crate) fn axis_cap(
    pool_limit: Option<u64>,
    target_limit: Option<u64>,
    pool_used: u64,
    target_used: u64,
) -> PoolCap {
    match (pool_limit, target_limit) {
        (Some(limit), _) => PoolCap::capped(CapScope::Pool, Some(limit), pool_used),
        // Shared: the cap belongs to the target, so the draw-down is the
        // target-wide total. Splitting it per pool would be a fabricated number.
        (None, Some(limit)) => PoolCap::capped(CapScope::Shared, Some(limit), target_used),
        (None, None) => PoolCap::unknown(pool_used),
    }
}

/// How many distinct pool buckets exist under a target in the ledger.
fn sibling_pool_count(target: &str, pooled: &BTreeMap<String, TargetWindows>) -> u64 {
    pooled
        .keys()
        .filter(|key| PoolKey::parse(key).target == target)
        .count() as u64
}

/// Compare two pools by available room.
///
/// Quota-derived when both sides have a real cap (remaining fraction wins);
/// otherwise usage-derived — less-used-first, which is a fallback heuristic and
/// is labelled as such so it cannot be mistaken for a headroom number.
pub fn compare_room(left: &PoolHeadroom, right: &PoolHeadroom) -> RoomComparison {
    match (left.room_fraction(), right.room_fraction()) {
        (Some(left_room), Some(right_room)) => RoomComparison {
            order: left_room.total_cmp(&right_room),
            basis: RoomBasis::Quota,
        },
        _ => RoomComparison {
            // Less observed usage == more room, hence the reversed compare.
            order: right.used_tokens.cmp(&left.used_tokens),
            basis: RoomBasis::ObservedUsage,
        },
    }
}

/// Rank pools most-room-first with a single, stated basis.
///
/// The basis is [`RoomBasis::Quota`] only when *every* pool in the set has a
/// configured cap; one uncapped pool makes fraction comparison meaningless for
/// the set, so the whole ranking falls back to observed 24h usage (ascending)
/// and says so. Ties break on the canonical key for a deterministic order.
pub fn rank_pools_by_room(pools: &[PoolHeadroom]) -> PoolRanking {
    let basis = if !pools.is_empty() && pools.iter().all(|pool| pool.room_fraction().is_some()) {
        RoomBasis::Quota
    } else {
        RoomBasis::ObservedUsage
    };
    let mut ranked = pools.to_vec();
    ranked.sort_by(|left, right| match basis {
        RoomBasis::Quota => right
            .room_fraction()
            .unwrap_or(0.0)
            .total_cmp(&left.room_fraction().unwrap_or(0.0))
            .then_with(|| left.key.cmp(&right.key)),
        RoomBasis::ObservedUsage => left
            .used_tokens
            .cmp(&right.used_tokens)
            .then_with(|| left.used_requests.cmp(&right.used_requests))
            .then_with(|| left.key.cmp(&right.key)),
    });
    PoolRanking { basis, ranked }
}

/// Bucket rows by pool identity. The only aggregation pass over the ledger —
/// the target-level view is derived from this one by [`fold_pools_to_targets`].
fn pool_windows_from_rows(rows: &[LedgerRow], now: u64) -> BTreeMap<String, TargetWindows> {
    let mut out: BTreeMap<String, TargetWindows> = BTreeMap::new();
    for row in rows {
        // Future-dated rows (clock skew) are treated as "just now" via
        // saturating_sub, so they still count toward the recent windows.
        let age = now.saturating_sub(row.epoch_ts);
        // Entry created before the window checks so a target/pool seen only
        // outside every window still appears (with zeroes), exactly as the
        // target-only aggregation always did.
        let entry = out.entry(row.pool_key().canonical()).or_default();
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

/// Sum every pool back into the target it belongs to.
///
/// Backward compatibility hinges on this being loss-free at target level: an
/// unpooled key folds to itself, so a ledger written before pools existed
/// produces the identical map it always did.
pub(crate) fn fold_pools_to_targets(
    pooled: &BTreeMap<String, TargetWindows>,
) -> BTreeMap<String, TargetWindows> {
    let mut out: BTreeMap<String, TargetWindows> = BTreeMap::new();
    for (key, windows) in pooled {
        let target = PoolKey::parse(key).target;
        out.entry(target).or_default().merge(windows);
    }
    out
}

#[cfg(test)]
fn windows_from_rows(rows: &[LedgerRow], now: u64) -> BTreeMap<String, TargetWindows> {
    fold_pools_to_targets(&pool_windows_from_rows(rows, now))
}

fn append_row(path: &Path, row: &LedgerRow) -> std::io::Result<()> {
    append_line(path, &row.to_tsv_line())
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")
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
        model_row(target, "m", age_secs, tokens_in, tokens_out, est)
    }

    fn model_row(
        target: &str,
        model: &str,
        age_secs: u64,
        tokens_in: u64,
        tokens_out: u64,
        est: bool,
    ) -> LedgerRow {
        LedgerRow {
            epoch_ts: 1_000_000 - age_secs,
            target: normalize_target(target),
            model: model.to_string(),
            input_tokens: tokens_in,
            output_tokens: tokens_out,
            estimated: est,
            ok: true,
        }
    }

    /// The aggregation exactly as it was before pool identity existed: bucket
    /// straight into targets, model column ignored. The golden test below pins
    /// the new fold to this reference output.
    fn legacy_windows_from_rows(rows: &[LedgerRow], now: u64) -> BTreeMap<String, TargetWindows> {
        let mut out: BTreeMap<String, TargetWindows> = BTreeMap::new();
        for row in rows {
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

    /// A fixture of rows as the ledger has always written them: real targets,
    /// model strings that carry no pool prefix, spread across the windows.
    fn legacy_fixture() -> Vec<LedgerRow> {
        vec![
            model_row("claude", "sonnet", 30, 900, 120, false),
            model_row("claude", "opus", 60 * 90, 1_500, 300, false),
            model_row("ollama", "granite4:350m", 120, 40, 10, true),
            model_row("ollama", "granite4:350m", WINDOW_5H_SECS + 1, 80, 20, true),
            model_row(
                "openai",
                "gpt-5.6-sol",
                WINDOW_24H_SECS + 60,
                5_000,
                900,
                false,
            ),
            // Only in the 7d window: the target must still appear, with zeroed
            // 5h/24h windows.
            model_row("codex", "gpt-5.6", WINDOW_24H_SECS * 3, 10, 10, false),
            // Older than every window: still creates a (zeroed) target entry.
            model_row("aider", "sonnet", WINDOW_7D_SECS + 1, 77, 77, false),
        ]
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
        config
            .limits
            .targets
            .insert("openai".to_string(), target_limit(Some(1000), Some(10)));
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

    fn temp_ledger_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rtrt-ledger-{tag}-{}-{}.tsv",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ))
    }

    #[test]
    fn ledger_lock_is_exclusive_and_released_on_drop() {
        let ledger = temp_ledger_path("lock");
        let lock_file = lock_path(&ledger);

        let held = LedgerLock::try_acquire(lock_file.clone(), LOCK_STALE_SECS);
        assert!(held.is_some(), "first acquire should succeed");
        // Contended (and fresh, so not stealable): second acquire fails after
        // its bounded retries instead of blocking or clobbering.
        assert!(LedgerLock::try_acquire(lock_file.clone(), LOCK_STALE_SECS).is_none());

        drop(held);
        assert!(!lock_file.exists(), "drop must remove the lock file");
        let reacquired = LedgerLock::try_acquire(lock_file.clone(), LOCK_STALE_SECS);
        assert!(reacquired.is_some(), "released lock is acquirable again");
    }

    #[test]
    fn stale_ledger_lock_is_stolen() {
        let ledger = temp_ledger_path("stale");
        let lock_file = lock_path(&ledger);
        // Simulate a crashed process: a lock file nobody will ever remove.
        std::fs::write(&lock_file, b"").unwrap();
        // With a zero stale timeout the leftover file is immediately stale.
        let stolen = LedgerLock::try_acquire(lock_file.clone(), 0);
        assert!(stolen.is_some(), "stale lock must be stolen");
        drop(stolen);
        assert!(!lock_file.exists());
    }

    #[test]
    fn lock_path_appends_lock_suffix() {
        let ledger = PathBuf::from("/tmp/x/provider-usage.tsv");
        assert_eq!(
            lock_path(&ledger),
            PathBuf::from("/tmp/x/provider-usage.tsv.lock")
        );
    }

    fn target_limit(
        daily_tokens: Option<u64>,
        daily_requests: Option<u64>,
    ) -> rtrt_core::TargetLimit {
        rtrt_core::TargetLimit {
            daily_tokens,
            daily_requests,
            ..Default::default()
        }
    }

    fn config_with_pool_cap(
        target: &str,
        target_limit_values: (Option<u64>, Option<u64>),
        pool: &str,
        pool_limit_values: (Option<u64>, Option<u64>),
    ) -> Config {
        let mut config = Config::default();
        let mut limit = target_limit(target_limit_values.0, target_limit_values.1);
        limit.pools.insert(
            pool.to_string(),
            rtrt_core::PoolLimit {
                daily_tokens: pool_limit_values.0,
                daily_requests: pool_limit_values.1,
            },
        );
        config.limits.targets.insert(target.to_string(), limit);
        config
    }

    /// The two `opencode` roster entries that share a target but not a quota.
    fn two_pool_rows() -> Vec<LedgerRow> {
        vec![
            model_row("opencode", "opencode-go/glm-5.2", 60, 100, 50, false),
            model_row("opencode", "ollama/glm-5.2:cloud", 90, 400, 200, false),
        ]
    }

    #[test]
    fn legacy_rows_aggregate_byte_identically_through_the_fold() {
        let now = 1_000_000;
        let rows = legacy_fixture();
        let legacy = legacy_windows_from_rows(&rows, now);
        let folded = windows_from_rows(&rows, now);
        // Byte-identical: same keys, same order, same counters.
        assert_eq!(format!("{legacy:#?}"), format!("{folded:#?}"));
        assert_eq!(
            serde_json::to_string(&legacy).unwrap(),
            serde_json::to_string(&folded).unwrap()
        );
        // Pin a couple of concrete numbers so the reference cannot drift with
        // the implementation it is guarding.
        assert_eq!(folded.keys().collect::<Vec<_>>().len(), 5);
        let claude = folded.get("claude").expect("claude present");
        assert_eq!(claude.last_5h.requests, 2);
        assert_eq!(claude.last_5h.tokens, 900 + 120 + 1_500 + 300);
        let aider = folded.get("aider").expect("aider present even when stale");
        assert_eq!(aider.last_7d, WindowUsage::default());
    }

    #[test]
    fn same_target_different_model_prefix_are_separate_pools() {
        let now = 1_000_000;
        let rows = two_pool_rows();
        let pooled = pool_windows_from_rows(&rows, now);

        assert_eq!(
            pooled.keys().cloned().collect::<Vec<_>>(),
            vec![
                "opencode#ollama".to_string(),
                "opencode#opencode-go".to_string()
            ]
        );
        assert_eq!(pooled["opencode#opencode-go"].last_24h.tokens, 150);
        assert_eq!(pooled["opencode#ollama"].last_24h.tokens, 600);

        // Folded back: one target bucket carrying the summed totals.
        let folded = fold_pools_to_targets(&pooled);
        assert_eq!(folded.keys().cloned().collect::<Vec<_>>(), vec!["opencode"]);
        let opencode = folded["opencode"];
        assert_eq!(opencode.last_24h.tokens, 750);
        assert_eq!(opencode.last_24h.requests, 2);
        // ...and identical to what the target-only aggregation would produce.
        assert_eq!(
            format!("{folded:#?}"),
            format!("{:#?}", legacy_windows_from_rows(&rows, now))
        );
    }

    #[test]
    fn unpooled_and_pooled_rows_coexist_under_one_target() {
        let now = 1_000_000;
        let mut rows = two_pool_rows();
        // A prefix-less model keys by the bare target, alongside its pools.
        rows.push(model_row("opencode", "glm-5.2", 60, 7, 3, false));
        let pooled = pool_windows_from_rows(&rows, now);
        assert_eq!(pooled["opencode"].last_24h.tokens, 10);
        assert_eq!(pooled.len(), 3);
        let folded = fold_pools_to_targets(&pooled);
        assert_eq!(folded["opencode"].last_24h.tokens, 760);
        assert_eq!(
            format!("{folded:#?}"),
            format!("{:#?}", legacy_windows_from_rows(&rows, now))
        );
    }

    #[test]
    fn target_cap_with_pools_underneath_is_shared_not_split() {
        let now = 1_000_000;
        let mut config = Config::default();
        config
            .limits
            .targets
            .insert("opencode".to_string(), target_limit(Some(1_000), Some(10)));
        let pooled = pool_windows_from_rows(&two_pool_rows(), now);
        let headroom = pool_headroom_from_windows(&pooled, &config);

        let go = &headroom["opencode#opencode-go"];
        let cloud = &headroom["opencode#ollama"];
        for pool in [go, cloud] {
            assert_eq!(pool.tokens.scope, CapScope::Shared);
            assert_eq!(pool.requests.scope, CapScope::Shared);
            // The whole cap is reported, never a per-pool slice of it.
            assert_eq!(pool.tokens.limit, Some(1_000));
            assert_eq!(pool.requests.limit, Some(10));
            // Drawn down by every sibling together, so both see the same
            // remaining pot.
            assert_eq!(pool.tokens.used, 750);
            assert_eq!(pool.tokens.remaining, Some(250));
            assert_eq!(pool.requests.remaining, Some(8));
            assert!(pool.shares_a_cap());
            assert!(!pool.limits_unknown());
            assert_eq!(pool.sibling_pools, 2);
        }
        // Each pool's OWN usage stays visible and un-merged.
        assert_eq!(go.used_tokens, 150);
        assert_eq!(cloud.used_tokens, 600);
        // Nothing anywhere equals cap/2 — the cap was never divided.
        assert!(
            headroom
                .values()
                .all(|pool| pool.tokens.limit == Some(1_000))
        );
    }

    #[test]
    fn pool_cap_takes_precedence_and_charges_only_its_own_usage() {
        let now = 1_000_000;
        let config = config_with_pool_cap(
            "opencode",
            (Some(1_000), Some(10)),
            "opencode-go",
            (Some(400), None),
        );
        let pooled = pool_windows_from_rows(&two_pool_rows(), now);
        let headroom = pool_headroom_from_windows(&pooled, &config);

        let go = &headroom["opencode#opencode-go"];
        assert_eq!(go.tokens.scope, CapScope::Pool);
        assert_eq!(go.tokens.limit, Some(400));
        assert_eq!(go.tokens.used, 150, "pool cap charges this pool only");
        assert_eq!(go.tokens.remaining, Some(250));
        // The un-capped axis still falls back to the shared target cap, and is
        // labelled as shared rather than silently reported as this pool's own.
        assert_eq!(go.requests.scope, CapScope::Shared);
        assert_eq!(go.requests.used, 2);
        assert_eq!(go.requests.remaining, Some(8));

        // The sibling has no cap of its own, so it keeps sharing the target's.
        let cloud = &headroom["opencode#ollama"];
        assert_eq!(cloud.tokens.scope, CapScope::Shared);
        assert_eq!(cloud.tokens.limit, Some(1_000));
        assert_eq!(cloud.tokens.used, 750);
    }

    #[test]
    fn pool_without_any_cap_reports_unknown_not_a_synthesised_one() {
        let now = 1_000_000;
        let config = Config::default();
        let pooled = pool_windows_from_rows(&two_pool_rows(), now);
        let headroom = pool_headroom_from_windows(&pooled, &config);

        for pool in headroom.values() {
            assert_eq!(pool.tokens.scope, CapScope::Unknown);
            assert_eq!(pool.requests.scope, CapScope::Unknown);
            assert_eq!(pool.tokens.limit, None);
            assert_eq!(pool.tokens.remaining, None);
            assert_eq!(pool.requests.limit, None);
            assert_eq!(pool.requests.remaining, None);
            assert!(pool.limits_unknown());
            assert!(!pool.shares_a_cap());
            assert_eq!(pool.room_fraction(), None);
        }
        // Observed usage is still reported — it is measured, not inferred.
        assert_eq!(headroom["opencode#opencode-go"].used_tokens, 150);
    }

    #[test]
    fn configured_pool_cap_is_visible_before_its_first_invocation() {
        let config = config_with_pool_cap(
            "opencode",
            (None, None),
            "opencode-go",
            (Some(400), Some(4)),
        );
        let headroom = pool_headroom_from_windows(&BTreeMap::new(), &config);
        let go = &headroom["opencode#opencode-go"];
        assert_eq!(go.tokens.scope, CapScope::Pool);
        assert_eq!(go.tokens.limit, Some(400));
        assert_eq!(go.used_tokens, 0);
        assert_eq!(go.tokens.remaining, Some(400));
        assert_eq!(go.sibling_pools, 0);
    }

    #[test]
    fn room_ranking_is_quota_derived_only_when_every_pool_has_a_cap() {
        let now = 1_000_000;
        // Both capped: 150/400 used vs 600/2000 used -> the cloud pool has the
        // larger remaining fraction (70% vs 62.5%).
        let mut config =
            config_with_pool_cap("opencode", (None, None), "opencode-go", (Some(400), None));
        config
            .limits
            .targets
            .get_mut("opencode")
            .unwrap()
            .pools
            .insert(
                "ollama".to_string(),
                rtrt_core::PoolLimit {
                    daily_tokens: Some(2_000),
                    daily_requests: None,
                },
            );
        let pooled = pool_windows_from_rows(&two_pool_rows(), now);
        let pools = pool_headroom_from_windows(&pooled, &config)
            .into_values()
            .collect::<Vec<_>>();

        let ranking = rank_pools_by_room(&pools);
        assert_eq!(ranking.basis, RoomBasis::Quota);
        assert_eq!(ranking.best().unwrap().key, "opencode#ollama");
        assert!(ranking.basis.label().contains("quota-derived"));

        let go = pools.iter().find(|p| p.key.ends_with("go")).unwrap();
        let cloud = pools.iter().find(|p| p.key.ends_with("ollama")).unwrap();
        let comparison = compare_room(cloud, go);
        assert_eq!(comparison.basis, RoomBasis::Quota);
        assert_eq!(comparison.order, Ordering::Greater);
    }

    #[test]
    fn uncapped_pools_rank_by_observed_usage_and_say_so() {
        let now = 1_000_000;
        let pooled = pool_windows_from_rows(&two_pool_rows(), now);
        let pools = pool_headroom_from_windows(&pooled, &Config::default())
            .into_values()
            .collect::<Vec<_>>();

        let ranking = rank_pools_by_room(&pools);
        assert_eq!(ranking.basis, RoomBasis::ObservedUsage);
        // Less-used first — an ordering, explicitly NOT a headroom number.
        assert_eq!(ranking.best().unwrap().key, "opencode#opencode-go");
        assert!(ranking.basis.label().contains("not a quota measurement"));
        assert!(
            ranking
                .ranked
                .iter()
                .all(|pool| pool.tokens.limit.is_none())
        );
    }

    #[test]
    fn a_single_uncapped_pool_downgrades_the_whole_ranking_basis() {
        let now = 1_000_000;
        // Only one of the two pools is capped: comparing a fraction against
        // nothing is meaningless, so the set falls back to observed usage.
        let config =
            config_with_pool_cap("opencode", (None, None), "opencode-go", (Some(400), None));
        let pooled = pool_windows_from_rows(&two_pool_rows(), now);
        let pools = pool_headroom_from_windows(&pooled, &config)
            .into_values()
            .collect::<Vec<_>>();
        let ranking = rank_pools_by_room(&pools);
        assert_eq!(ranking.basis, RoomBasis::ObservedUsage);
        assert_eq!(ranking.ranked.len(), 2);
    }

    #[test]
    fn target_headroom_is_unchanged_by_pooled_rows() {
        let now = 1_000_000;
        let mut config = Config::default();
        config
            .limits
            .targets
            .insert("opencode".to_string(), target_limit(Some(1_000), Some(10)));
        // The target view sums the pools, so it matches what it reported before
        // pools existed.
        let windows = windows_from_rows(&two_pool_rows(), now);
        let headroom = headroom_for("opencode", &windows, &config);
        assert_eq!(headroom.used_tokens, 750);
        assert_eq!(headroom.remaining_tokens, Some(250));
        assert_eq!(headroom.used_requests, 2);
        assert_eq!(headroom.remaining_requests, Some(8));
        assert!(!headroom.limits_unknown());
    }

    // -----------------------------------------------------------------------
    // Provider-reported rate-limit signals
    // -----------------------------------------------------------------------

    /// A fixed observation instant so duration-resolved and RFC 3339 resets can
    /// be compared against each other exactly. `1_700_000_000` is
    /// `2023-11-14T22:13:20Z`, which the RFC 3339 test pins independently.
    const OBSERVED: u64 = 1_700_000_000;
    /// `OBSERVED` + 6 minutes, i.e. the same instant `6m0s` and `360` resolve to.
    const RESET_INSTANT: &str = "2023-11-14T22:19:20Z";

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        use reqwest::header::{HeaderName, HeaderValue};
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        map
    }

    /// A reading that pins only the tokens axis — the axis the two-pool fixture
    /// binds on, so the assertions stay about provenance rather than arithmetic.
    fn reported_signal(
        key: &str,
        observed_at: u64,
        limit: Option<u64>,
        remaining: u64,
        reset_at: u64,
    ) -> RateLimitSignal {
        RateLimitSignal {
            key: key.to_string(),
            observed_at,
            status: 200,
            requests: RateLimitAxis::default(),
            tokens: RateLimitAxis {
                limit,
                remaining: Some(remaining),
                reset_at: Some(reset_at),
            },
            retry_after_at: None,
        }
    }

    fn signal_map<const N: usize>(
        signals: [RateLimitSignal; N],
    ) -> BTreeMap<String, RateLimitSignal> {
        signals
            .into_iter()
            .map(|signal| (signal.key.clone(), signal))
            .collect()
    }

    #[test]
    fn anthropic_and_openai_header_sets_parse_into_the_same_record() {
        let key = PoolKey::unpooled("anthropic");
        let anthropic = rate_limit_signal(
            &key,
            200,
            OBSERVED,
            &headers(&[
                ("anthropic-ratelimit-requests-limit", "1000"),
                ("anthropic-ratelimit-requests-remaining", "999"),
                ("anthropic-ratelimit-requests-reset", RESET_INSTANT),
                ("anthropic-ratelimit-tokens-limit", "80000"),
                ("anthropic-ratelimit-tokens-remaining", "12345"),
                ("anthropic-ratelimit-tokens-reset", RESET_INSTANT),
            ]),
        )
        .expect("anthropic headers are a signal");
        let openai = rate_limit_signal(
            &key,
            200,
            OBSERVED,
            &headers(&[
                ("x-ratelimit-limit-requests", "1000"),
                ("x-ratelimit-remaining-requests", "999"),
                // The same instant, expressed the way OpenAI expresses it.
                ("x-ratelimit-reset-requests", "6m0s"),
                ("x-ratelimit-limit-tokens", "80000"),
                ("x-ratelimit-remaining-tokens", "12345"),
                ("x-ratelimit-reset-tokens", "360"),
            ]),
        )
        .expect("openai headers are a signal");

        assert_eq!(anthropic, openai, "one normalized record, two dialects");
        assert_eq!(
            anthropic.requests,
            RateLimitAxis {
                limit: Some(1_000),
                remaining: Some(999),
                reset_at: Some(OBSERVED + 360),
            }
        );
        assert_eq!(anthropic.tokens.remaining, Some(12_345));
        assert_eq!(anthropic.retry_after_at, None);
    }

    #[test]
    fn every_documented_duration_shape_parses_and_garbage_never_does() {
        assert_eq!(parse_duration_millis("6m0s"), Some(360_000));
        assert_eq!(parse_duration_millis("1s"), Some(1_000));
        assert_eq!(parse_duration_millis("250ms"), Some(250));
        assert_eq!(parse_duration_millis("30"), Some(30_000));
        assert_eq!(parse_duration_millis("1h30m"), Some(5_400_000));
        assert_eq!(parse_duration_millis("1.5s"), Some(1_500));
        for garbage in ["", "   ", "soon", "-5", "s", "6m0", "1x", "nan", "inf"] {
            assert_eq!(
                parse_duration_millis(garbage),
                None,
                "{garbage:?} must never become a number"
            );
        }
        // A sub-second window is real; rounding it down would make a live
        // reading read as already reset.
        assert_eq!(parse_reset_instant("250ms", OBSERVED), Some(OBSERVED + 1));
        assert_eq!(parse_reset_instant("6m0s", OBSERVED), Some(OBSERVED + 360));
    }

    #[test]
    fn rfc3339_reset_instants_resolve_and_garbage_never_does() {
        assert_eq!(parse_rfc3339_epoch("2023-11-14T22:13:20Z"), Some(OBSERVED));
        assert_eq!(
            parse_rfc3339_epoch("2023-11-14T22:13:20.123456Z"),
            Some(OBSERVED)
        );
        assert_eq!(
            parse_rfc3339_epoch("2023-11-14T23:13:20+01:00"),
            Some(OBSERVED)
        );
        assert_eq!(
            parse_rfc3339_epoch("2023-11-14T21:13:20-01:00"),
            Some(OBSERVED)
        );
        for garbage in [
            "",
            "2023-11-14",
            "2023-13-01T00:00:00Z",
            "2023-02-30T00:00:00Z",
            "2023-11-14T25:00:00Z",
            "2023-11-14T22:13:20QQ",
            "not-a-real-timestamp",
            "6m0s",
        ] {
            assert_eq!(parse_rfc3339_epoch(garbage), None, "{garbage:?}");
        }
    }

    #[test]
    fn absent_empty_and_garbage_headers_record_nothing_rather_than_zero() {
        let key = PoolKey::unpooled("openai");
        assert!(
            rate_limit_signal(&key, 200, OBSERVED, &HeaderMap::new()).is_none(),
            "no headers at all is not a signal"
        );
        assert!(
            rate_limit_signal(
                &key,
                200,
                OBSERVED,
                &headers(&[
                    ("x-ratelimit-limit-requests", ""),
                    ("x-ratelimit-remaining-requests", "   "),
                    ("x-ratelimit-reset-requests", "whenever"),
                    ("retry-after", "soon"),
                ])
            )
            .is_none(),
            "empty and unreadable headers carry no number"
        );

        // One readable header is still a reading, and the unreadable one beside
        // it stays absent instead of collapsing to zero.
        let partial = rate_limit_signal(
            &key,
            200,
            OBSERVED,
            &headers(&[
                ("x-ratelimit-limit-requests", "-1"),
                ("x-ratelimit-remaining-requests", "17"),
            ]),
        )
        .expect("one readable header is a signal");
        assert_eq!(partial.requests.limit, None);
        assert_eq!(partial.requests.remaining, Some(17));
        assert_eq!(partial.requests.reset_at, None);

        // An empty signal is never written to disk.
        assert!(!record_rate_limit_at(
            &temp_ledger_path("ratelimit-empty"),
            &RateLimitSignal::default()
        ));
    }

    #[test]
    fn a_429_still_records_the_remaining_and_reset_it_carries() {
        let key = PoolKey::new("openai", Some("gpt-5"));
        let signal = rate_limit_signal(
            &key,
            429,
            OBSERVED,
            &headers(&[
                ("x-ratelimit-limit-requests", "1000"),
                ("x-ratelimit-remaining-requests", "0"),
                ("x-ratelimit-reset-requests", "6m0s"),
                ("retry-after", "60"),
            ]),
        )
        .expect("a rejection is still a reading");
        assert_eq!(signal.status, 429);
        assert_eq!(signal.requests.remaining, Some(0));
        assert_eq!(signal.requests.reset_at, Some(OBSERVED + 360));
        assert_eq!(signal.backoff_until(), Some(OBSERVED + 60));

        let path = temp_ledger_path("ratelimit-429");
        assert!(record_rate_limit_at(&path, &signal));
        let stored = latest_signals(&read_signals(&path));
        assert_eq!(stored[&key.canonical()], signal);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn signal_rows_round_trip_and_absent_numbers_stay_absent() {
        let signal = RateLimitSignal {
            key: "opencode#ollama".to_string(),
            observed_at: OBSERVED,
            status: 429,
            requests: RateLimitAxis {
                limit: Some(1_000),
                remaining: Some(0),
                reset_at: Some(OBSERVED + 60),
            },
            tokens: RateLimitAxis::default(),
            retry_after_at: Some(OBSERVED + 30),
        };
        let line = signal.to_tsv_line();
        assert_eq!(
            line.split('\t').filter(|f| *f == ABSENT_FIELD).count(),
            3,
            "the unreported tokens axis is absent, not zero: {line}"
        );
        assert_eq!(RateLimitSignal::parse(&line).expect("round trip"), signal);
        assert!(RateLimitSignal::parse("garbage").is_none());
        assert!(RateLimitSignal::parse("").is_none());
    }

    #[test]
    fn the_newest_row_per_pool_wins() {
        let path = temp_ledger_path("ratelimit-newest");
        let older = reported_signal("openai", OBSERVED, Some(1_000), 900, OBSERVED + 60);
        let newer = reported_signal("openai", OBSERVED + 5, Some(1_000), 100, OBSERVED + 60);
        assert!(record_rate_limit_at(&path, &older));
        assert!(record_rate_limit_at(&path, &newer));
        let stored = latest_signals(&read_signals(&path));
        assert_eq!(stored.len(), 1);
        assert_eq!(stored["openai"].tokens.remaining, Some(100));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_reading_past_its_own_reset_is_history_not_headroom() {
        let now = 1_000_000;
        let config = config_with_pool_cap(
            "opencode",
            (Some(1_000), None),
            "opencode-go",
            (Some(400), None),
        );
        let pooled = pool_windows_from_rows(&two_pool_rows(), now);
        let baseline = pool_headroom_from_windows(&pooled, &config);

        // Observed an hour ago, self-declared to refill a minute after that:
        // whatever it reported refilled long before now.
        let expired = reported_signal(
            "opencode#opencode-go",
            now - 3_600,
            Some(4_000),
            12,
            now - 3_540,
        );
        assert_eq!(expired.current_at(now), None);
        let with_expired = pool_headroom_from_parts(&pooled, &config, &signal_map([expired]), now);
        assert_eq!(
            format!("{baseline:#?}"),
            format!("{with_expired:#?}"),
            "a stale reading must leave every number exactly as it was"
        );
        assert_eq!(
            serde_json::to_string(&baseline).unwrap(),
            serde_json::to_string(&with_expired).unwrap()
        );

        // The same reading inside its own window does govern.
        let live = reported_signal("opencode#opencode-go", now, Some(4_000), 12, now + 60);
        let with_live = pool_headroom_from_parts(&pooled, &config, &signal_map([live]), now);
        assert_eq!(
            with_live["opencode#opencode-go"].tokens.scope,
            CapScope::Reported
        );
        assert_eq!(with_live["opencode#opencode-go"].tokens.remaining, Some(12));

        // A reading with no reset instant has no window of its own, so it is
        // honoured only inside the 24h window the headroom is measured over.
        let undated = |observed_at: u64| RateLimitSignal {
            key: "opencode#opencode-go".to_string(),
            observed_at,
            status: 200,
            requests: RateLimitAxis::default(),
            tokens: RateLimitAxis {
                limit: Some(4_000),
                remaining: Some(12),
                reset_at: None,
            },
            retry_after_at: None,
        };
        assert!(undated(now).current_at(now).is_some());
        assert!(undated(now - WINDOW_24H_SECS).current_at(now).is_none());
    }

    #[test]
    fn provider_reported_outranks_configured_which_outranks_observed_usage() {
        let now = 1_000_000;
        let mut config = Config::default();
        config
            .limits
            .targets
            .insert("opencode".to_string(), target_limit(Some(1_000), None));
        let mut rows = two_pool_rows();
        // A third pool with neither a reading nor a cap.
        rows.push(model_row("ollama", "granite4:350m", 60, 5, 5, false));
        let pooled = pool_windows_from_rows(&rows, now);
        let signals = signal_map([reported_signal(
            "opencode#opencode-go",
            now,
            Some(4_000),
            3_000,
            now + 300,
        )]);
        let headroom = pool_headroom_from_parts(&pooled, &config, &signals, now);

        // 1. A live provider reading beats the configured cap for the SAME pool.
        let go = &headroom["opencode#opencode-go"];
        assert_eq!(go.tokens.scope, CapScope::Reported);
        assert_eq!(go.tokens.limit, Some(4_000));
        assert_eq!(go.tokens.remaining, Some(3_000));
        assert_eq!(
            go.tokens.used, 1_000,
            "used is limit - remaining, both of them reported"
        );
        assert!(go.has_reported_quota());
        assert_eq!(go.room_scope(), Some(CapScope::Reported));
        assert!(go.tokens.scope.label().starts_with("provider-reported"));

        // 2. The configured cap still governs the sibling with no reading, and
        //    still says it is shared rather than this pool's own.
        let cloud = &headroom["opencode#ollama"];
        assert_eq!(cloud.tokens.scope, CapScope::Shared);
        assert_eq!(cloud.tokens.limit, Some(1_000));
        assert!(cloud.tokens.scope.label().starts_with("configured"));
        assert!(cloud.tokens.scope.label().contains("shared"));

        // 3. Neither: nothing is invented.
        let uncapped = &headroom["ollama"];
        assert_eq!(uncapped.tokens.scope, CapScope::Unknown);
        assert_eq!(uncapped.tokens.limit, None);
        assert_eq!(uncapped.tokens.remaining, None);
        assert!(uncapped.tokens.scope.label().starts_with("unknown"));

        // Both capped pools rank on a real fraction (75% reported vs 25%
        // configured); the uncapped one drags a set down to the usage-derived
        // fallback exactly as it always did.
        let ranking = rank_pools_by_room(&[go.clone(), cloud.clone()]);
        assert_eq!(ranking.basis, RoomBasis::Quota);
        assert_eq!(ranking.best().unwrap().key, "opencode#opencode-go");
        assert_eq!(
            ranking.cap_scope(),
            None,
            "a mixed set must not claim one provenance"
        );
        let mixed = rank_pools_by_room(&[go.clone(), uncapped.clone()]);
        assert_eq!(mixed.basis, RoomBasis::ObservedUsage);
        assert!(mixed.basis.label().contains("not a quota measurement"));
    }

    #[test]
    fn a_ranking_built_only_on_provider_numbers_says_so() {
        let now = 1_000_000;
        let pooled = pool_windows_from_rows(&two_pool_rows(), now);
        let signals = signal_map([
            reported_signal("opencode#opencode-go", now, Some(4_000), 1_000, now + 300),
            reported_signal("opencode#ollama", now, Some(4_000), 3_000, now + 300),
        ]);
        let pools = pool_headroom_from_parts(&pooled, &Config::default(), &signals, now)
            .into_values()
            .collect::<Vec<_>>();

        let ranking = rank_pools_by_room(&pools);
        assert_eq!(ranking.basis, RoomBasis::Quota);
        assert_eq!(ranking.cap_scope(), Some(CapScope::Reported));
        assert_eq!(ranking.best().unwrap().key, "opencode#ollama");
        let label = ranking.basis_label();
        assert!(label.contains("quota-derived"), "{label}");
        assert!(label.contains("provider-reported"), "{label}");

        // The pre-existing wording of a basis is untouched — provenance is
        // added alongside it, never spliced into it.
        assert_eq!(
            RoomBasis::Quota.label(),
            "quota-derived (remaining fraction of a configured cap)"
        );
    }

    #[test]
    fn a_reported_remaining_without_a_ceiling_is_not_given_a_denominator() {
        let now = 1_000_000;
        let pooled = pool_windows_from_rows(&two_pool_rows(), now);
        let signals = signal_map([reported_signal(
            "opencode#opencode-go",
            now,
            None,
            900,
            now + 300,
        )]);
        let headroom = pool_headroom_from_parts(&pooled, &Config::default(), &signals, now);

        let go = &headroom["opencode#opencode-go"];
        assert_eq!(go.tokens.scope, CapScope::Reported);
        assert_eq!(go.tokens.remaining, Some(900));
        assert_eq!(go.tokens.limit, None);
        assert_eq!(
            go.tokens.used, 150,
            "no reported ceiling => rtrt's own observed usage, not a guess"
        );
        assert_eq!(go.tokens.remaining_fraction(), None);
        assert_eq!(go.room_fraction(), None);
    }

    #[test]
    fn a_pool_rtrt_only_has_a_reading_for_is_still_visible() {
        let now = 1_000_000;
        let signals = signal_map([reported_signal(
            "anthropic",
            now,
            Some(80_000),
            79_000,
            now + 300,
        )]);
        let headroom =
            pool_headroom_from_parts(&BTreeMap::new(), &Config::default(), &signals, now);
        let anthropic = &headroom["anthropic"];
        assert_eq!(anthropic.tokens.scope, CapScope::Reported);
        assert_eq!(anthropic.tokens.remaining, Some(79_000));
        assert_eq!(anthropic.used_tokens, 0);
        assert_eq!(anthropic.sibling_pools, 0);
    }

    #[test]
    fn a_reading_is_never_inherited_by_a_sibling_pool() {
        let now = 1_000_000;
        let pooled = pool_windows_from_rows(&two_pool_rows(), now);
        let signals = signal_map([reported_signal(
            "opencode#opencode-go",
            now,
            Some(4_000),
            3_000,
            now + 300,
        )]);
        let headroom = pool_headroom_from_parts(&pooled, &Config::default(), &signals, now);
        // A rate-limit header describes the bucket the response drew from.
        // Spreading it across siblings, or up to the target, would be a guess.
        assert_eq!(
            headroom["opencode#ollama"].tokens.scope,
            CapScope::Unknown,
            "the sibling has no reading of its own"
        );
        assert_eq!(headroom["opencode#ollama"].tokens.remaining, None);
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
