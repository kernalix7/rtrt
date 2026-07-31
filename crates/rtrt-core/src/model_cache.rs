//! Cache for CLI-enumerated model lists.
//!
//! [`crate::detect`] asks CLI-hosted multi-provider agents to list their own
//! models (`RuntimeProbe::CliModelList`). That answer is worth having — it is
//! what lets routing bind a lane to a concrete `(target, model)` — but it costs
//! a process start plus the tool's own round-trips to its provider registry,
//! and detection runs on every routing decision. Paying that on every route is
//! exactly the kind of overhead this toolkit exists to remove, so the answer is
//! persisted next to the other rtrt state (`~/.rtrt/`) and re-probed only when
//! it could plausibly have changed.
//!
//! The cache is *derived* data: it is never user configuration, every write is
//! best-effort, and every failure mode (missing, unreadable, corrupt,
//! unwritable) degrades to exactly the behaviour that existed before the cache —
//! probe, and return whatever the probe produced.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::detect::{MODEL_LIST_TIMEOUT, VERSION_POLL_INTERVAL};

/// Lives beside `provider-usage.tsv` in the rtrt state directory.
const CACHE_FILE_NAME: &str = "cli-models.json";
/// Redirect the cache file — the same override shape the usage ledger uses
/// (`RTRT_PROVIDER_USAGE_PATH`), so tests and sandboxes can point rtrt state
/// somewhere disposable.
const CACHE_PATH_ENV_VAR: &str = "RTRT_MODEL_CACHE_PATH";
/// Bypass switch: `off` disables the cache entirely (probe every time, write
/// nothing), `refresh` forces one re-probe and rewrites the entry.
const CACHE_MODE_ENV_VAR: &str = "RTRT_MODEL_CACHE";
const CACHE_OFF_VALUES: &[&str] = &["0", "off", "false", "no"];
const CACHE_REFRESH_VALUES: &[&str] = &["refresh", "force", "reprobe"];
const STATE_DIR_NAME: &str = ".rtrt";

/// How the cache behaves for one lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheMode {
    /// Serve a fresh entry when there is one; otherwise probe and store.
    Use,
    /// Ignore any stored entry, probe, and store the result.
    Refresh,
    /// Do not read and do not write — behave as if the cache did not exist.
    Disabled,
}

/// Everything about *how* a model list was obtained that could change the
/// answer. An entry is only served when all of it still matches, so upgrading
/// or reinstalling the tool invalidates the list without anyone deciding how
/// long an upgrade "usually" takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProbeIdentity {
    /// Resolved binary path — a different `opencode` on `PATH` is a different
    /// tool with a different account and therefore a different model list.
    binary: String,
    /// The version string detection already probed for this binary.
    #[serde(default)]
    version: Option<String>,
    /// The list subcommand itself: changing the arguments changes the question.
    args: Vec<String>,
    /// Size + mtime of the binary, so a reinstall that keeps the version string
    /// (nightly builds, `--version` that reports a release channel) still
    /// invalidates.
    #[serde(default)]
    binary_len: Option<u64>,
    #[serde(default)]
    binary_mtime_epoch: Option<u64>,
}

impl ProbeIdentity {
    pub(crate) fn new(binary: &Path, version: Option<&str>, args: &[&str]) -> Self {
        let metadata = fs::metadata(binary).ok();
        Self {
            binary: binary.display().to_string(),
            version: version.map(str::to_string),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            binary_len: metadata.as_ref().map(fs::Metadata::len),
            binary_mtime_epoch: metadata
                .as_ref()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(epoch_secs),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    identity: ProbeIdentity,
    probed_at_epoch: u64,
    /// What the probe actually cost, in milliseconds. This is the input to the
    /// reuse window — see [`reuse_window`].
    probe_millis: u64,
    models: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    entries: BTreeMap<String, CacheEntry>,
}

/// A cache file plus the mode it is consulted under.
pub(crate) struct Store {
    path: PathBuf,
    mode: CacheMode,
}

impl Store {
    pub(crate) fn at(path: PathBuf, mode: CacheMode) -> Self {
        Self { path, mode }
    }

    /// The real store: `~/.rtrt/cli-models.json` unless redirected, in whatever
    /// mode the environment asks for.
    pub(crate) fn from_env() -> Self {
        Self::at(
            resolve_path(
                std::env::var_os(CACHE_PATH_ENV_VAR),
                dirs::home_dir(),
                CACHE_FILE_NAME,
            ),
            mode_from_env(std::env::var_os(CACHE_MODE_ENV_VAR)),
        )
    }

    /// The same store, but forced to re-probe. A standing `off` is respected:
    /// a user who asked rtrt not to persist model lists does not get a file
    /// written behind their back by an explicit refresh either.
    pub(crate) fn refreshing(self) -> Self {
        let mode = match self.mode {
            CacheMode::Disabled => CacheMode::Disabled,
            CacheMode::Use | CacheMode::Refresh => CacheMode::Refresh,
        };
        Self { mode, ..self }
    }

    /// Serve `tool`'s model list from the cache, or run `probe` and store what
    /// it returned.
    pub(crate) fn models_or_probe(
        &self,
        tool: &str,
        identity: &ProbeIdentity,
        probe: impl FnOnce() -> Vec<String>,
    ) -> Vec<String> {
        self.models_or_probe_at(now_epoch_secs(), tool, identity, probe)
    }

    fn models_or_probe_at(
        &self,
        now: u64,
        tool: &str,
        identity: &ProbeIdentity,
        probe: impl FnOnce() -> Vec<String>,
    ) -> Vec<String> {
        if self.mode == CacheMode::Use
            && let Some(models) = self.fresh_models(tool, identity, now)
        {
            return models;
        }

        let started = Instant::now();
        let models = probe();
        let probe_cost = started.elapsed();

        // A probe that produced nothing is not an answer, it is a failure (not
        // logged in, tool mid-upgrade, list command changed). Storing it would
        // pin "no models" for a whole reuse window, so the miss is left to be
        // retried on the next detection — the pre-cache behaviour.
        if self.mode != CacheMode::Disabled && !models.is_empty() {
            self.store(
                tool,
                &CacheEntry {
                    identity: identity.clone(),
                    probed_at_epoch: now,
                    probe_millis: u64::try_from(probe_cost.as_millis()).unwrap_or(u64::MAX),
                    models: models.clone(),
                },
            );
        }
        models
    }

    fn fresh_models(&self, tool: &str, identity: &ProbeIdentity, now: u64) -> Option<Vec<String>> {
        let entry = load(&self.path)?.entries.remove(tool)?;
        if &entry.identity != identity || entry.models.is_empty() || !is_fresh(&entry, now) {
            return None;
        }
        Some(entry.models)
    }

    /// Best-effort write. Detection must never fail, slow down, or panic
    /// because a derived file could not be updated, so every error is dropped.
    fn store(&self, tool: &str, entry: &CacheEntry) {
        let _ = write_entry(&self.path, tool, entry);
    }
}

/// How long a probed list stays servable.
///
/// Derived from what the probe actually cost on *this* machine, not from a
/// picked interval: an entry is served until re-probing it would cost more than
/// detection can even measure. Detection polls its children every
/// [`VERSION_POLL_INTERVAL`] and allows one model-list probe to run for
/// [`MODEL_LIST_TIMEOUT`], so `VERSION_POLL_INTERVAL / MODEL_LIST_TIMEOUT` is
/// the share of a worst-case probe that falls below detection's own timing
/// floor — spending that fraction of wall-clock time on re-probing is free by
/// detection's own standard. Amortising a probe of cost `c` down to that share
/// gives `c / (VERSION_POLL_INTERVAL / MODEL_LIST_TIMEOUT)`.
///
/// The useful property is the scaling, not the number it happens to produce: a
/// cheap probe earns a short window (re-probe often, it costs nothing), an
/// expensive one earns a proportionally longer window, and a probe that costs
/// nothing is not cached at all. A tool that gets faster automatically starts
/// refreshing more often, with no constant to revisit.
fn reuse_window(probe_cost: Duration) -> Duration {
    let millis = probe_cost.as_millis().saturating_mul(amortization_factor());
    Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
}

/// `MODEL_LIST_TIMEOUT / VERSION_POLL_INTERVAL` — how many times detection's
/// timing floor fits inside one probe budget.
fn amortization_factor() -> u128 {
    MODEL_LIST_TIMEOUT
        .as_millis()
        .saturating_div(VERSION_POLL_INTERVAL.as_millis().max(1))
}

fn is_fresh(entry: &CacheEntry, now: u64) -> bool {
    // A timestamp in the future means the clock moved (or the file was hand
    // edited); distrust the entry rather than serve it until the clock catches
    // up.
    let Some(age) = now.checked_sub(entry.probed_at_epoch) else {
        return false;
    };
    Duration::from_secs(age) < reuse_window(Duration::from_millis(entry.probe_millis))
}

/// Read the whole cache file. Missing, unreadable and malformed all collapse to
/// `None` — the caller treats every one of them as "no usable entry".
fn load(path: &Path) -> Option<CacheFile> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Insert one entry and swap the file into place atomically.
///
/// A rename over the destination is a single filesystem operation, so a reader
/// in another process sees either the previous complete file or the new one and
/// never a half-written one — no lockfile needed for readers. Writers merge
/// into whatever is on disk at the time, so the worst outcome of two rtrt
/// processes storing different tools at once is that one entry is dropped and
/// re-probed later. Corruption is not reachable, which is why this does not
/// need the ledger's `O_EXCL` lock discipline.
fn write_entry(path: &Path, tool: &str, entry: &CacheEntry) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = load(path).unwrap_or_default();
    file.entries.insert(tool.to_string(), entry.clone());
    let json = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;

    let temp = temp_sibling(path);
    fs::write(&temp, json.as_bytes())?;
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

/// A per-process, per-call scratch name in the destination directory, so the
/// rename stays on one filesystem and two writers never share a temp file.
fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| CACHE_FILE_NAME.to_string());
    path.with_file_name(format!(
        "{name}.{}.{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default()
    ))
}

fn resolve_path(
    override_path: Option<OsString>,
    home: Option<PathBuf>,
    file_name: &str,
) -> PathBuf {
    if let Some(custom) = override_path.filter(|custom| !custom.is_empty()) {
        return PathBuf::from(custom);
    }
    home.unwrap_or_else(|| PathBuf::from("."))
        .join(STATE_DIR_NAME)
        .join(file_name)
}

fn mode_from_env(raw: Option<OsString>) -> CacheMode {
    let Some(raw) = raw else {
        return CacheMode::Use;
    };
    let value = raw.to_string_lossy().trim().to_ascii_lowercase();
    if CACHE_OFF_VALUES.contains(&value.as_str()) {
        CacheMode::Disabled
    } else if CACHE_REFRESH_VALUES.contains(&value.as_str()) {
        CacheMode::Refresh
    } else {
        CacheMode::Use
    }
}

fn epoch_secs(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs())
}

fn now_epoch_secs() -> u64 {
    epoch_secs(SystemTime::now()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// Stand-in for a real `opencode models` run: counts its invocations so a
    /// test can assert the cache did (or did not) spawn anything.
    struct CountingProbe {
        calls: Cell<u32>,
        models: Vec<String>,
    }

    impl CountingProbe {
        fn new(models: &[&str]) -> Self {
            Self {
                calls: Cell::new(0),
                models: models.iter().map(|model| (*model).to_string()).collect(),
            }
        }

        fn run(&self) -> Vec<String> {
            self.calls.set(self.calls.get() + 1);
            self.models.clone()
        }

        fn calls(&self) -> u32 {
            self.calls.get()
        }
    }

    fn temp_store_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rtrt-model-cache-{tag}-{}-{}/cli-models.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default()
        ))
    }

    fn identity(binary: &str, version: Option<&str>) -> ProbeIdentity {
        ProbeIdentity::new(Path::new(binary), version, &["models"])
    }

    /// An entry whose probe cost is high enough that its reuse window is long
    /// on any machine — the probe cost this cache exists for.
    fn measured_probe_millis() -> u64 {
        1_200
    }

    fn seeded_store(tag: &str, tool: &str, identity: &ProbeIdentity, models: &[&str]) -> Store {
        let store = Store::at(temp_store_path(tag), CacheMode::Use);
        store.store(
            tool,
            &CacheEntry {
                identity: identity.clone(),
                probed_at_epoch: 1_000,
                probe_millis: measured_probe_millis(),
                models: models.iter().map(|model| (*model).to_string()).collect(),
            },
        );
        store
    }

    #[test]
    fn cold_lookup_probes_once_and_stores_the_list() {
        let store = Store::at(temp_store_path("cold"), CacheMode::Use);
        let identity = identity("/usr/bin/opencode", Some("1.0.0"));
        let probe = CountingProbe::new(&["opencode-go/glm-5.2", "openai/gpt-5.6-luna"]);

        let models = store.models_or_probe_at(1_000, "opencode", &identity, || probe.run());

        assert_eq!(probe.calls(), 1);
        assert_eq!(models, vec!["opencode-go/glm-5.2", "openai/gpt-5.6-luna"]);
        let stored = load(&store.path).expect("cache file written");
        assert_eq!(stored.entries["opencode"].models, models);
        assert_eq!(stored.entries["opencode"].probed_at_epoch, 1_000);
    }

    #[test]
    fn fresh_entry_is_served_without_probing() {
        let identity = identity("/usr/bin/opencode", Some("1.0.0"));
        let store = seeded_store("warm", "opencode", &identity, &["opencode-go/glm-5.2"]);
        let probe = CountingProbe::new(&["never/used"]);

        let models = store.models_or_probe_at(1_001, "opencode", &identity, || probe.run());

        assert_eq!(probe.calls(), 0, "a fresh entry must not spawn the tool");
        assert_eq!(models, vec!["opencode-go/glm-5.2"]);
    }

    #[test]
    fn upgraded_or_moved_binary_invalidates_the_entry() {
        let stored_identity = identity("/usr/bin/opencode", Some("1.0.0"));
        for changed in [
            identity("/usr/bin/opencode", Some("1.1.0")),
            identity("/opt/opencode/bin/opencode", Some("1.0.0")),
            ProbeIdentity::new(
                Path::new("/usr/bin/opencode"),
                Some("1.0.0"),
                &["models", "--all"],
            ),
        ] {
            let store = seeded_store("upgrade", "opencode", &stored_identity, &["stale/model"]);
            let probe = CountingProbe::new(&["fresh/model"]);

            let models = store.models_or_probe_at(1_001, "opencode", &changed, || probe.run());

            assert_eq!(probe.calls(), 1, "changed identity must re-probe");
            assert_eq!(models, vec!["fresh/model"]);
            assert_eq!(
                load(&store.path).expect("rewritten").entries["opencode"].identity,
                changed
            );
        }
    }

    #[test]
    fn entry_past_its_reuse_window_is_reprobed() {
        let identity = identity("/usr/bin/opencode", Some("1.0.0"));
        let store = seeded_store("stale", "opencode", &identity, &["stale/model"]);
        let window = reuse_window(Duration::from_millis(measured_probe_millis()));
        let probe = CountingProbe::new(&["fresh/model"]);

        let models =
            store.models_or_probe_at(1_000 + window.as_secs() + 1, "opencode", &identity, || {
                probe.run()
            });

        assert_eq!(probe.calls(), 1);
        assert_eq!(models, vec!["fresh/model"]);
    }

    #[test]
    fn a_timestamp_from_the_future_is_not_treated_as_fresh() {
        let identity = identity("/usr/bin/opencode", Some("1.0.0"));
        let store = seeded_store("skew", "opencode", &identity, &["stale/model"]);
        let probe = CountingProbe::new(&["fresh/model"]);

        // `now` before the entry's timestamp: the clock moved backwards.
        let models = store.models_or_probe_at(999, "opencode", &identity, || probe.run());

        assert_eq!(probe.calls(), 1);
        assert_eq!(models, vec!["fresh/model"]);
    }

    #[test]
    fn corrupt_cache_falls_back_to_probing_and_is_repaired() {
        for corrupt in [
            "",
            "{",
            "not json at all",
            "{\"entries\":{\"opencode\":{}}}",
        ] {
            let store = Store::at(temp_store_path("corrupt"), CacheMode::Use);
            fs::create_dir_all(store.path.parent().unwrap()).unwrap();
            fs::write(&store.path, corrupt).unwrap();
            let identity = identity("/usr/bin/opencode", Some("1.0.0"));
            let probe = CountingProbe::new(&["opencode-go/glm-5.2"]);

            let models = store.models_or_probe_at(1_000, "opencode", &identity, || probe.run());

            assert_eq!(probe.calls(), 1, "corrupt cache must fall back to probing");
            assert_eq!(models, vec!["opencode-go/glm-5.2"]);
            let repaired = load(&store.path).expect("corrupt file replaced by a valid one");
            assert_eq!(repaired.entries["opencode"].models, models);
        }
    }

    #[test]
    fn unwritable_cache_location_still_yields_a_probed_list() {
        // A regular file where the cache directory should be: creating the
        // parent, and therefore the write, cannot succeed.
        let blocker = temp_store_path("unwritable");
        let blocker = blocker.parent().unwrap().to_path_buf();
        fs::create_dir_all(blocker.parent().unwrap()).unwrap();
        fs::write(&blocker, b"not a directory").unwrap();
        let store = Store::at(blocker.join("cli-models.json"), CacheMode::Use);
        let identity = identity("/usr/bin/opencode", Some("1.0.0"));
        let probe = CountingProbe::new(&["opencode-go/glm-5.2"]);

        let models = store.models_or_probe_at(1_000, "opencode", &identity, || probe.run());

        assert_eq!(models, vec!["opencode-go/glm-5.2"]);
        assert_eq!(probe.calls(), 1);
    }

    #[test]
    fn failed_probe_yields_an_empty_list_and_stores_nothing() {
        let store = Store::at(temp_store_path("failed"), CacheMode::Use);
        let identity = identity("/usr/bin/opencode", Some("1.0.0"));
        let probe = CountingProbe::new(&[]);

        assert!(
            store
                .models_or_probe_at(1_000, "opencode", &identity, || probe.run())
                .is_empty()
        );
        assert_eq!(probe.calls(), 1);
        assert!(
            !store.path.exists(),
            "a failed probe must not pin an empty list"
        );
    }

    #[test]
    fn refresh_reprobes_even_when_the_entry_is_fresh() {
        let identity = identity("/usr/bin/opencode", Some("1.0.0"));
        let store = seeded_store("refresh", "opencode", &identity, &["stale/model"]).refreshing();
        let probe = CountingProbe::new(&["fresh/model"]);

        let models = store.models_or_probe_at(1_001, "opencode", &identity, || probe.run());

        assert_eq!(probe.calls(), 1);
        assert_eq!(models, vec!["fresh/model"]);
        assert_eq!(
            load(&store.path).expect("rewritten").entries["opencode"].models,
            models
        );
    }

    #[test]
    fn disabled_cache_neither_reads_nor_writes() {
        let identity = identity("/usr/bin/opencode", Some("1.0.0"));
        let seeded = seeded_store("disabled", "opencode", &identity, &["stale/model"]);
        let store = Store::at(seeded.path.clone(), CacheMode::Disabled);
        let probe = CountingProbe::new(&["fresh/model"]);

        let models = store.models_or_probe_at(1_001, "opencode", &identity, || probe.run());

        assert_eq!(probe.calls(), 1, "disabled cache must always probe");
        assert_eq!(models, vec!["fresh/model"]);
        assert_eq!(
            load(&store.path).expect("still readable").entries["opencode"].models,
            vec!["stale/model"],
            "disabled cache must not write"
        );
        // Disabling stays disabled even when a refresh is requested on top.
        assert_eq!(store.refreshing().mode, CacheMode::Disabled);
    }

    #[test]
    fn storing_one_tool_keeps_other_tools_entries() {
        let opencode = identity("/usr/bin/opencode", Some("1.0.0"));
        let other = identity("/usr/bin/other", Some("2.0.0"));
        let store = seeded_store("merge", "opencode", &opencode, &["opencode-go/glm-5.2"]);
        let probe = CountingProbe::new(&["other/model"]);

        store.models_or_probe_at(1_001, "other", &other, || probe.run());

        let stored = load(&store.path).expect("cache file");
        assert_eq!(stored.entries.len(), 2);
        assert_eq!(
            stored.entries["opencode"].models,
            vec!["opencode-go/glm-5.2"]
        );
        assert_eq!(stored.entries["other"].models, vec!["other/model"]);
    }

    #[test]
    fn reuse_window_scales_with_the_measured_probe_cost() {
        let factor = amortization_factor();
        assert!(factor > 1, "a probe budget spans many polling intervals");
        assert_eq!(
            factor,
            MODEL_LIST_TIMEOUT.as_millis() / VERSION_POLL_INTERVAL.as_millis()
        );

        // Free probes earn no cache; costlier probes earn proportionally more.
        assert_eq!(reuse_window(Duration::ZERO), Duration::ZERO);
        let cheap = reuse_window(Duration::from_millis(10));
        let measured = reuse_window(Duration::from_millis(measured_probe_millis()));
        assert!(measured > cheap);
        assert_eq!(
            measured.as_millis(),
            u128::from(measured_probe_millis()) * factor
        );
        // Nothing overflows for an absurd probe cost.
        assert!(reuse_window(Duration::from_millis(u64::MAX)) > measured);
    }

    #[test]
    fn cache_path_defaults_beside_the_other_rtrt_state_and_honours_the_override() {
        assert_eq!(
            resolve_path(None, Some(PathBuf::from("/home/u")), CACHE_FILE_NAME),
            PathBuf::from("/home/u/.rtrt/cli-models.json")
        );
        assert_eq!(
            resolve_path(
                Some(OsString::from("/tmp/custom.json")),
                Some(PathBuf::from("/home/u")),
                CACHE_FILE_NAME
            ),
            PathBuf::from("/tmp/custom.json")
        );
        // No home directory: never panic, never write outside the working tree.
        assert_eq!(
            resolve_path(None, None, CACHE_FILE_NAME),
            PathBuf::from("./.rtrt/cli-models.json")
        );
        // An empty override is not an override.
        assert_eq!(
            resolve_path(
                Some(OsString::new()),
                Some(PathBuf::from("/home/u")),
                CACHE_FILE_NAME
            ),
            PathBuf::from("/home/u/.rtrt/cli-models.json")
        );
    }

    #[test]
    fn cache_mode_reads_the_env_override() {
        assert_eq!(mode_from_env(None), CacheMode::Use);
        for off in ["off", "OFF", "0", " false ", "no"] {
            assert_eq!(
                mode_from_env(Some(OsString::from(off))),
                CacheMode::Disabled
            );
        }
        for refresh in ["refresh", "Force", "reprobe"] {
            assert_eq!(
                mode_from_env(Some(OsString::from(refresh))),
                CacheMode::Refresh
            );
        }
        // Anything unrecognised keeps the default rather than silently
        // disabling the cache.
        for other in ["", "1", "on", "yes", "maybe"] {
            assert_eq!(mode_from_env(Some(OsString::from(other))), CacheMode::Use);
        }
    }

    #[test]
    fn temp_file_is_a_sibling_of_the_cache() {
        let path = PathBuf::from("/home/u/.rtrt/cli-models.json");
        let temp = temp_sibling(&path);
        assert_eq!(temp.parent(), path.parent());
        assert_ne!(temp, path);
        assert!(
            temp.to_string_lossy().ends_with(".tmp"),
            "temp files must be recognisable"
        );
    }
}
