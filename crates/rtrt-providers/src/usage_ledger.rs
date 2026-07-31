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

use std::{
    cmp::Ordering,
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapScope {
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
    pool_headroom_from_windows(&pool_usage_windows(), config)
}

/// Headroom for a single pool. Public so a caller can query one candidate
/// without materializing the whole map.
pub fn headroom_for_pool(key: &PoolKey, config: &Config) -> PoolHeadroom {
    let pooled = pool_usage_windows();
    let targets = fold_pools_to_targets(&pooled);
    headroom_for_pool_in(key, &pooled, &targets, config)
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

fn pool_headroom_from_windows(
    pooled: &BTreeMap<String, TargetWindows>,
    config: &Config,
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
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .map(|key| {
            (
                key.canonical(),
                headroom_for_pool_in(&key, pooled, &targets, config),
            )
        })
        .collect()
}

fn headroom_for_pool_in(
    key: &PoolKey,
    pooled: &BTreeMap<String, TargetWindows>,
    targets: &BTreeMap<String, TargetWindows>,
    config: &Config,
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

    // Resolved per axis: a pool may pin tokens while inheriting a shared
    // request cap, and collapsing that into one scope would misreport one of
    // them.
    let tokens = axis_cap(
        pool_limit.and_then(|limit| limit.daily_tokens),
        target_limit.and_then(|limit| limit.daily_tokens),
        window.tokens,
        target_window.tokens,
    );
    let requests = axis_cap(
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
