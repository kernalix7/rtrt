//! Background watcher that tails Claude Code session transcripts (the JSONL
//! files under `~/.claude/projects/`) and saves every new assistant turn AND
//! genuine user prompt into the rtrt memory store. Closes two capture gaps at
//! once: teammate / subagent work that runs in its own session (FleetView,
//! Task-tool subagents) and never reaches the main agent's transcript, and
//! user input from backfilled or subagent transcripts — the live
//! `UserPromptSubmit` CLI hook only sees the main session as it happens, so
//! without this, old/backfilled and subagent sessions end up with answers
//! that have no matching question.
//!
//! Layout the watcher knows about:
//!   ~/.claude/projects/<encoded-cwd>/<session>.jsonl
//!   ~/.claude/projects/<encoded-cwd>/<session>/subagents/agent-*.jsonl
//!
//! Both shapes carry standard Claude transcript lines with `cwd`, `sessionId`,
//! optional `agentId` / `slug`, and `message.content[]` parts. The watcher
//! resolves `cwd` to its GIT REPOSITORY ROOT (via `rtrt_core::project_for_cwd`)
//! and uses that basename as the rtrt project bucket — so a capture in a
//! sub-dir (`src`, `web`, …) or a git worktree lands under the real repo
//! instead of its own bogus bucket. It dedups via `MemoryStore::body_seen_at`
//! so existing rows from the SessionStart / Stop / SubagentStop / live
//! UserPromptSubmit hooks don't get duplicated.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rtrt_memory::{InvocationProvenance, MemoryStore, is_synthetic_prompt};
use serde_json::Value;
use tokio::sync::Mutex;
use walkdir::WalkDir;

/// Polling interval. Cheap — the hot path is reading appended bytes off a few
/// JSONL files, not walking the whole tree (mtime check filters out idle ones).
const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);
const MAX_ENTRIES_PER_SWEEP: usize = 4096;
const MAX_FILES_PER_SWEEP: usize = 512;
const MAX_READ_PER_FILE: u64 = 1024 * 1024;
const MAX_JSONL_LINE: usize = 256 * 1024;
const MAX_ATTRIBUTION_BYTES: u64 = 256 * 1024;
const MAX_ATTRIBUTION_FILES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    created: Option<std::time::SystemTime>,
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
}

impl FileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                created: metadata.created().ok(),
                modified: metadata.modified().ok(),
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct FileCursor {
    offset: u64,
    discarding_oversized_line: bool,
    identity: FileIdentity,
}

/// Boot migration: re-home every transcript row onto the project of its
/// `<encoded>` dir (Claude Code's per-project session dir), folding rows that a
/// per-line worktree cwd had scattered into bogus buckets (feat-*, wf_*,
/// agent-*, p<n>-*) back under their real project. No name patterns — purely
/// the file's encoded dir. Idempotent: a settled row is skipped, so the work
/// shrinks to zero across runs.
pub fn spawn_reattribution(
    memory: Option<Arc<Mutex<MemoryStore>>>,
    identity: Arc<rtrt_core::ProjectIdentity>,
) {
    let Some(memory) = memory else { return };
    tokio::spawn(async move {
        let candidates = {
            let guard = memory.lock().await;
            match guard.reattribution_candidates() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("reattribution: query candidates: {e}");
                    return;
                }
            }
        };
        if candidates.is_empty() {
            return;
        }
        // A row's project is decided purely by the `<encoded>` dir of its
        // transcript file (Claude Code's per-project session dir) — no name
        // patterns. So worktree-scattered main rows (feat-*, p<n>-*) and
        // subagent / workflow rows (agent-*, wf_*) all fold to the real project.
        let Some(base) = transcripts_base_dir() else {
            return;
        };
        let mut cache: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut moved = 0usize;
        let mut tagged = 0usize;
        for (id, tf, project, source_kind, session_id, parent_session_id) in candidates {
            if project != identity.slug() || !transcript_matches(Path::new(&tf), &base, &identity) {
                continue;
            }
            let is_subagent = tf.contains("/subagents/");
            let kind = if is_subagent { "subagent" } else { "main" };
            let provenance = {
                let guard = memory.lock().await;
                provenance_for_sessions(&guard, session_id.as_deref(), parent_session_id.as_deref())
            };
            let _ = project_for_transcript(Path::new(&tf), &base, &mut cache);
            let move_to: Option<&str> = None;
            // Skip a settled row only when there is no durable provenance
            // metadata left to merge.
            if move_to.is_none() && source_kind.as_deref() == Some(kind) && provenance.is_none() {
                continue;
            }
            let guard = memory.lock().await;
            if guard
                .reattribute_with_provenance(id, kind, move_to, provenance.as_ref())
                .is_ok()
            {
                tagged += 1;
                if move_to.is_some() {
                    moved += 1;
                }
            }
        }
        tracing::info!(
            "reattribution: {tagged} transcript rows tagged, {moved} moved to real project"
        );
    });
}

/// Spawn the transcript watcher as a background task. No-op when `memory` is
/// `None` (memory disabled at the dashboard level).
pub fn spawn_transcript_watcher(
    memory: Option<Arc<Mutex<MemoryStore>>>,
    identity: Arc<rtrt_core::ProjectIdentity>,
) {
    let Some(memory) = memory else {
        tracing::info!("transcript watcher disabled (memory store not available)");
        return;
    };
    let base = match transcripts_base_dir() {
        Some(p) => p,
        None => {
            tracing::info!(
                "transcript watcher disabled ($HOME unset; no ~/.claude/projects/ to watch)"
            );
            return;
        }
    };
    if !base.exists() {
        tracing::info!(
            "transcript watcher disabled ({} not present yet)",
            base.display()
        );
        return;
    }
    tracing::info!("transcript watcher on: {}", base.display());
    tokio::spawn(async move {
        let mut offsets: HashMap<PathBuf, FileCursor> = HashMap::new();
        let mut proj_cache: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut tick = tokio::time::interval(DEFAULT_INTERVAL);
        loop {
            tick.tick().await;
            if let Err(e) = sweep(&base, &memory, &mut offsets, &mut proj_cache, &identity).await {
                tracing::warn!("transcript sweep failed: {e}");
            }
        }
    });
}

fn transcripts_base_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

/// One sweep: walk every `.jsonl` under `base`, read appended bytes since the
/// last sweep, parse each new line, save any new assistant turn.
async fn sweep(
    base: &Path,
    memory: &Arc<Mutex<MemoryStore>>,
    offsets: &mut HashMap<PathBuf, FileCursor>,
    proj_cache: &mut HashMap<PathBuf, Option<String>>,
    identity: &rtrt_core::ProjectIdentity,
) -> anyhow::Result<()> {
    let files: Vec<PathBuf> = WalkDir::new(base)
        .max_depth(4)
        .follow_links(false)
        .into_iter()
        .take(MAX_ENTRIES_PER_SWEEP)
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .map(|e| e.into_path())
        .take(MAX_FILES_PER_SWEEP)
        .collect();

    // Prevent churn in the watched tree from growing cursor/cache state forever.
    offsets.retain(|path, _| files.contains(path));
    proj_cache.retain(|dir, _| files.iter().any(|path| path.starts_with(dir)));

    for path in files {
        let (mut file, opened_identity, len) = match open_regular_file(&path) {
            Ok(opened) => opened,
            Err(_) => continue,
        };
        if !transcript_matches(&path, base, identity) {
            continue;
        }
        let previous = offsets.get(&path).copied();
        let reset =
            previous.is_none_or(|cursor| cursor.identity != opened_identity || len < cursor.offset);
        let mut cursor = if reset {
            FileCursor {
                offset: 0,
                discarding_oversized_line: false,
                identity: opened_identity,
            }
        } else {
            previous.expect("checked above")
        };
        let start = cursor.offset;
        if len == start {
            continue;
        }
        let end = len.min(start.saturating_add(MAX_READ_PER_FILE));
        let new_bytes = match read_range(&mut file, start, end) {
            Ok(b) => b,
            Err(_) => continue,
        };
        // Resolve the project from the file's `<encoded>` dir (the real project,
        // worktree-stable), computed once per file and cached per encoded dir.
        let resolved_project = project_for_transcript(&path, base, proj_cache);
        // Attribution inspected path names. Ensure path still names exact file
        // opened above before any bytes from that handle can be accepted.
        if !opened_file_is_current(&path, opened_identity) {
            offsets.remove(&path);
            continue;
        }
        // Track the offset of the *last full* line so we resume cleanly even
        // when the writer is mid-write at the EOF (partial trailing line).
        let mut consumed = start;
        for line in new_bytes.split_inclusive(|&b| b == b'\n') {
            let next_offset = consumed + line.len() as u64;
            if cursor.discarding_oversized_line {
                consumed = next_offset;
                if line.ends_with(b"\n") {
                    cursor.discarding_oversized_line = false;
                }
                continue;
            }
            if !line.ends_with(b"\n") {
                if line.len() > MAX_JSONL_LINE {
                    // Consume bounded chunks until newline. This hostile line
                    // is never parsed and cannot pin cursor at its beginning.
                    cursor.discarding_oversized_line = true;
                    consumed = next_offset;
                }
                break;
            }
            if line.len() - 1 > MAX_JSONL_LINE {
                consumed = next_offset;
                continue;
            }
            // Strip the trailing newline before parsing.
            let s = match std::str::from_utf8(&line[..line.len() - 1]) {
                Ok(s) if !s.trim().is_empty() => s,
                _ => {
                    consumed = next_offset;
                    continue;
                }
            };
            if let Some(mut turn) = parse_line(s, &path, resolved_project.as_deref()) {
                turn.project = identity.slug().to_string();
                if let Err(e) = save_turn(memory, &turn, identity.slug()).await {
                    tracing::warn!("transcript save {}: {e}", path.display());
                    // Keep the failed line pending. Advancing here silently
                    // loses it when another writer holds SQLite past the busy
                    // timeout.
                    break;
                }
            }
            consumed = next_offset;
        }
        cursor.offset = consumed;
        offsets.insert(path, cursor);
    }
    Ok(())
}

fn transcript_matches(file: &Path, base: &Path, identity: &rtrt_core::ProjectIdentity) -> bool {
    let Some(encoded) = file
        .strip_prefix(base)
        .ok()
        .and_then(|path| path.components().next())
    else {
        return false;
    };
    let encoded_dir = base.join(encoded.as_os_str());
    let Ok(entries) = std::fs::read_dir(encoded_dir) else {
        return false;
    };
    let mut sessions: Vec<_> = entries
        .take(MAX_ATTRIBUTION_FILES)
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_regular_nonsymlink(path))
        .filter(|path| path.extension().and_then(|v| v.to_str()) == Some("jsonl"))
        .collect();
    sessions.sort();
    sessions
        .into_iter()
        .filter_map(|path| first_cwd_in(&path))
        .any(|cwd| {
            rtrt_core::ProjectIdentity::derive(cwd)
                .is_ok_and(|candidate| candidate.fingerprint() == identity.fingerprint())
        })
}

/// External `claude -p` lanes launched by OpenCode may run from a disposable
/// `/tmp/opencode/<project>-<lane>` directory. Once that directory disappears,
/// Git-root attribution can only see the lane basename. Fold only this explicit
/// transcript shape onto an already-known scoped project (`00G_oxrdp`, etc.)
/// whose stem is an exact lane-name prefix.
#[cfg(test)]
fn canonical_project_for_opencode_temp(
    file: &Path,
    base: &Path,
    current: &str,
    known_projects: &[String],
) -> String {
    let is_opencode_temp = file
        .strip_prefix(base)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .is_some_and(|encoded| encoded.starts_with("-tmp-opencode-"));
    if !is_opencode_temp {
        return current.to_string();
    }

    let lane = normalize_project_stem(current);
    let mut best: Option<(&str, usize)> = None;
    let mut ambiguous = false;
    for project in known_projects {
        let Some(stem) = scoped_project_stem(project) else {
            continue;
        };
        if lane != stem && !lane.starts_with(&format!("{stem}-")) {
            continue;
        }
        match best {
            Some((_, length)) if stem.len() < length => {}
            Some((_, length)) if stem.len() == length => ambiguous = true,
            _ => {
                best = Some((project, stem.len()));
                ambiguous = false;
            }
        }
    }
    match (best, ambiguous) {
        (Some((project, _)), false) => project.to_string(),
        _ => current.to_string(),
    }
}

#[cfg(test)]
fn scoped_project_stem(project: &str) -> Option<String> {
    let (scope, name) = project.split_once('_')?;
    if scope.len() < 2
        || name.is_empty()
        || !scope.as_bytes()[..2].iter().all(u8::is_ascii_digit)
        || !scope.as_bytes().iter().skip(2).all(u8::is_ascii_uppercase)
    {
        return None;
    }
    Some(normalize_project_stem(name))
}

#[cfg(test)]
fn normalize_project_stem(project: &str) -> String {
    project.replace('_', "-").to_ascii_lowercase()
}

fn read_range(f: &mut File, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (end - start) as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn is_regular_nonsymlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn open_regular_file(path: &Path) -> std::io::Result<(File, FileIdentity, u64)> {
    let path_metadata = std::fs::symlink_metadata(path)?;
    if !path_metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "transcript path is not a regular file",
        ));
    }
    let file = File::open(path)?;
    let opened_metadata = file.metadata()?;
    let identity = FileIdentity::from_metadata(&opened_metadata);
    if !opened_metadata.is_file() || identity != FileIdentity::from_metadata(&path_metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "transcript changed while opening",
        ));
    }
    Ok((file, identity, opened_metadata.len()))
}

fn opened_file_is_current(path: &Path, identity: FileIdentity) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_file() && FileIdentity::from_metadata(&metadata) == identity
    })
}

/// A single capturable transcript line — either an assistant/teammate turn or
/// a genuine user-authored prompt.
struct Turn {
    project: String,
    text: String,
    session_id: String,
    /// The main session a subagent transcript ran under — the `<session>`
    /// path component two levels above `.../subagents/<file>.jsonl`. `None`
    /// for a top-level (main) transcript, which has no parent.
    parent_session: Option<String>,
    agent_id: Option<String>,
    slug: Option<String>,
    file: PathBuf,
    /// Row kind: `"assistant-turn"`, `"teammate-message"`, or
    /// `"user-prompt-submit"`.
    kind: &'static str,
    /// `"main"` or `"subagent"` — classifies whose work this row represents,
    /// same as the `source_kind` metadata the live hooks write. A captured
    /// user prompt is always `"main"`, even inside a `/subagents/`
    /// transcript: that line is the parent handing the subagent its task —
    /// human-authored main-session input, not subagent-produced output.
    source_kind: &'static str,
}

/// The project a transcript file belongs to. Claude Code stores every session
/// of one project under a single `~/.claude/projects/<encoded>/` directory
/// (keyed by the session's starting cwd). We derive the project from that
/// `<encoded>` dir's representative cwd resolved to its GIT REPOSITORY ROOT —
/// NOT the per-line cwd (which can switch to a git-worktree path mid-session)
/// and NOT the raw cwd basename (which scatters sub-dir cwds like `src` into
/// bogus buckets). Subagent / workflow transcripts live under the same
/// `<encoded>` dir, so they resolve to the same real project automatically.
/// Result is cached per `<encoded>` dir.
fn project_for_transcript(
    file: &Path,
    base: &Path,
    cache: &mut HashMap<PathBuf, Option<String>>,
) -> Option<String> {
    let rel = file.strip_prefix(base).ok()?;
    let encoded = rel.components().next()?.as_os_str();
    let encoded_dir = base.join(encoded);
    cache
        .entry(encoded_dir.clone())
        .or_insert_with(|| representative_project(&encoded_dir))
        .clone()
}

/// Representative project name for an `<encoded>` dir: the GIT-ROOT project of
/// the cwd found in its first top-level session transcript (deterministic by
/// sorted filename). The cwd is run through [`rtrt_core::project_for_cwd`], so a
/// session whose cwd was `.../00G_ONCRIX/crates/drivers/src` (a sub-dir) or a
/// git worktree folds to its real repo (`00G_ONCRIX`) instead of the cwd
/// basename (`src`). Top-level only — we skip the `subagents/` subtree, whose
/// cwds may be worktrees.
fn representative_project(encoded_dir: &Path) -> Option<String> {
    let rd = std::fs::read_dir(encoded_dir).ok()?;
    let mut sessions: Vec<PathBuf> = rd
        .take(MAX_ATTRIBUTION_FILES)
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_regular_nonsymlink(p))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect();
    sessions.sort();
    for s in &sessions {
        if let Some(cwd) = first_cwd_in(s) {
            // Git-root attribution: walk the cwd up to its repo root (or the main
            // repo for a worktree) and use that basename. Falls back to the cwd
            // basename internally when no `.git` is found.
            return Some(rtrt_core::project_for_cwd_str(&cwd));
        }
    }
    None
}

/// For a subagent transcript at `<encoded>/<session>/subagents/<file>.jsonl`,
/// returns the parent `<session>` id — pure path parsing, no filesystem
/// access, so it stays correct even after the transcript file itself is
/// gone. `None` for a top-level (main) transcript, which has no parent.
fn parent_session_from_path(file: &Path) -> Option<String> {
    let subagents_dir = file.parent()?; // .../<session>/subagents
    if subagents_dir.file_name()? != std::ffi::OsStr::new("subagents") {
        return None;
    }
    let session_dir = subagents_dir.parent()?; // .../<session>
    session_dir.file_name()?.to_str().map(String::from)
}

/// Synthesizes a classifiable capture-bucket name for the rare case where
/// project resolution genuinely fails at capture time — no `<encoded>` dir
/// project (e.g. the dir held no top-level session transcript yet) AND no
/// resolvable line `cwd`. Shaped `agent-<session>[-<agent>]` so
/// [`rtrt_memory::is_capture_bucket_name`] always recognises it: the row is
/// never silently dropped, and instead of parking under an unclassified name
/// forever it surfaces immediately via the dashboard's hidden-bucket count
/// (and can be folded into its real project with `/api/projects/reassign`
/// once a human figures out which one that is).
fn fallback_capture_bucket(session_id: &str, agent_id: Option<&str>) -> String {
    let session = if session_id.is_empty() {
        "unknown"
    } else {
        session_id
    };
    match agent_id {
        Some(a) if !a.is_empty() => format!("agent-{session}-{a}"),
        _ => format!("agent-{session}"),
    }
}

/// Read the first `cwd` field from a transcript file (scanning the first lines).
fn first_cwd_in(jsonl: &Path) -> Option<String> {
    let (mut file, identity, len) = open_regular_file(jsonl).ok()?;
    let bytes = read_range(&mut file, 0, len.min(MAX_ATTRIBUTION_BYTES)).ok()?;
    if !opened_file_is_current(jsonl, identity) {
        return None;
    }
    for line in bytes.split_inclusive(|byte| *byte == b'\n').take(50) {
        if !line.ends_with(b"\n") || line.len() - 1 > MAX_JSONL_LINE {
            continue;
        }
        if let Ok(v) = serde_json::from_slice::<Value>(&line[..line.len() - 1]) {
            if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                return Some(c.to_string());
            }
        }
    }
    None
}

/// Top-level line role: `"assistant"` or `"user"` when recognisable, else
/// `None`. Checks `type` first (the modern transcript field); falls back to
/// `message.role` for lines where `type` isn't one of those two values —
/// matches Claude Code's transcript shape across format revisions.
fn line_role(v: &Value) -> Option<&str> {
    match v.get("type").and_then(|t| t.as_str()) {
        Some(t @ ("assistant" | "user")) => Some(t),
        _ => v
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .filter(|r| matches!(*r, "assistant" | "user")),
    }
}

/// Extracts visible text from an `assistant`-role line's `message.content`
/// parts array. `None` when there's no text part (thinking-only, tool-use-only)
/// or the content isn't an array.
fn extract_assistant_text(v: &Value) -> Option<String> {
    let content = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())?;
    let mut text = String::new();
    for part in content {
        if part.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(s) = part.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(s);
            }
        }
    }
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Extracts real user-typed text from a `user`-role line's `message.content`,
/// or `None` when the line isn't a genuine prompt.
///
/// `content` is either a plain string (a real prompt) or an array of parts.
/// Claude Code also routes tool_result echoes back to the harness through a
/// `user`-role line — those carry a `tool_result` part in the array and MUST
/// NOT be mistaken for something the user typed. Only a plain string, or an
/// array with `text` parts and NO `tool_result` part, counts as real prompt
/// text.
fn extract_user_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let has_tool_result = parts
                .iter()
                .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_result"));
            if has_tool_result {
                return None;
            }
            let mut text = String::new();
            for part in parts {
                if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(s) = part.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(s);
                    }
                }
            }
            if text.is_empty() { None } else { Some(text) }
        }
        _ => None,
    }
}

/// Metadata shared by both the assistant and user parse paths, resolved
/// identically regardless of which role the line turns out to be.
struct LineContext {
    is_subagent: bool,
    parent_session: Option<String>,
    session_id: String,
    agent_id: Option<String>,
    slug: Option<String>,
    project: String,
}

/// Resolves session/parent/agent metadata plus the project bucket for a
/// transcript line. `resolved_project` (the file's `<encoded>` dir project) is
/// authoritative and overrides the line's own cwd for BOTH main and subagent
/// rows — resolved HERE, at capture time, while the transcript is still on
/// disk, so a later deletion/rotation of that file can never orphan the row.
/// Falls back to the line's own cwd, resolved to its git root, only if the
/// dir couldn't be resolved — never the raw cwd basename, which scatters
/// sub-dir / worktree sessions into bogus buckets. If BOTH fail (the rare
/// case where the encoded dir has no top-level session yet and the line
/// carries no usable cwd), synthesize a classifiable capture bucket instead
/// of silently dropping the turn — see [`fallback_capture_bucket`].
fn line_context(v: &Value, file: &Path, resolved_project: Option<&str>) -> LineContext {
    let is_subagent = file
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("subagents"));
    let parent_session = parent_session_from_path(file);

    let session_id = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let agent_id = v.get("agentId").and_then(|s| s.as_str()).map(String::from);
    let slug = v.get("slug").and_then(|s| s.as_str()).map(String::from);

    let line_project = v
        .get("cwd")
        .and_then(|c| c.as_str())
        .map(rtrt_core::project_for_cwd_str);
    let project = resolved_project
        .map(String::from)
        .or(line_project)
        .unwrap_or_else(|| fallback_capture_bucket(&session_id, agent_id.as_deref()));

    LineContext {
        is_subagent,
        parent_session,
        session_id,
        agent_id,
        slug,
        project,
    }
}

/// Parses one transcript line into a capturable [`Turn`] — either a genuine
/// assistant/teammate turn carrying non-empty visible text, or a genuine
/// user-authored prompt. Returns `None` for everything else: thinking-only or
/// tool-use-only assistant lines, tool_result echoes routed through a
/// `user`-role line, harness-injected synthetic prompts (see
/// [`rtrt_memory::is_synthetic_prompt`]), and partial/unparseable lines.
///
/// `resolved_project` (the file's `<encoded>` dir project) is authoritative
/// and overrides the line's own cwd for every captured kind.
fn parse_line(line: &str, file: &Path, resolved_project: Option<&str>) -> Option<Turn> {
    let v: Value = serde_json::from_str(line).ok()?;
    match line_role(&v) {
        Some("assistant") => {
            let text = extract_assistant_text(&v)?;
            let ctx = line_context(&v, file, resolved_project);
            let (kind, source_kind) = if ctx.is_subagent {
                ("teammate-message", "subagent")
            } else {
                ("assistant-turn", "main")
            };
            Some(Turn {
                project: ctx.project,
                text,
                session_id: ctx.session_id,
                parent_session: ctx.parent_session,
                agent_id: ctx.agent_id,
                slug: ctx.slug,
                file: file.to_path_buf(),
                kind,
                source_kind,
            })
        }
        Some("user") => {
            let content = v.get("message").and_then(|m| m.get("content"))?;
            let text = extract_user_text(content)?;
            let text = text.trim();
            if text.is_empty() || is_synthetic_prompt(text) {
                return None;
            }
            let ctx = line_context(&v, file, resolved_project);
            Some(Turn {
                project: ctx.project,
                text: text.to_string(),
                session_id: ctx.session_id,
                parent_session: ctx.parent_session,
                agent_id: ctx.agent_id,
                slug: ctx.slug,
                file: file.to_path_buf(),
                kind: "user-prompt-submit",
                // Always "main" — even inside a /subagents/ transcript this
                // line is the parent's own task text, not subagent-produced
                // output. See the field doc on `Turn::source_kind`.
                source_kind: "main",
            })
        }
        _ => None,
    }
}

async fn save_turn(
    memory: &Arc<Mutex<MemoryStore>>,
    t: &Turn,
    pinned_project: &str,
) -> anyhow::Result<()> {
    let sha = rtrt_memory::MemoryStore::body_sha(&t.text);
    let guard = memory.lock().await;
    let provenance = turn_provenance(&guard, t);
    let project = pinned_project;
    // Dedup against everything already in this project's bucket — e.g. the
    // live UserPromptSubmit hook and the SessionStart / Stop / SubagentStop
    // hooks already cover a lot of this ground, so the watcher only adds what
    // they miss (backfilled transcripts, subagent transcripts) without
    // doubling up on what's already there.
    if guard.body_seen_at(project, &sha).ok().flatten().is_some() {
        return Ok(());
    }
    let mut meta: BTreeMap<String, String> = BTreeMap::new();
    meta.insert("source".into(), "transcript".into());
    // Classify the row so the UI can split a project's main-agent work from its
    // subagent / teammate work.
    meta.insert("source_kind".into(), t.source_kind.into());
    if !t.session_id.is_empty() {
        meta.insert("session_id".into(), t.session_id.clone());
    }
    // The main session a subagent ran under, captured at write time from the
    // transcript's path — survives even after that transcript is deleted, so
    // a manual reassign later can still tell which project's work this was.
    if let Some(p) = &t.parent_session {
        meta.insert("parent_session".into(), p.clone());
    }
    if let Some(a) = &t.agent_id {
        meta.insert("agent_id".into(), a.clone());
    }
    if let Some(s) = &t.slug {
        meta.insert("slug".into(), s.clone());
    }
    if let Some(value) = &provenance {
        meta.insert("invocation_id".into(), value.invocation_id.clone());
        if let Some(parent) = &value.parent_session_id {
            meta.insert("parent_session_id".into(), parent.clone());
        }
        if let Some(call) = &value.parent_call_id {
            meta.insert("parent_call_id".into(), call.clone());
        }
        if let Some(agent) = &value.caller_agent {
            meta.insert("caller_agent".into(), agent.clone());
        }
        if let Some(target) = &value.target {
            meta.insert("child_target".into(), target.clone());
        }
        if let Some(model) = &value.model {
            meta.insert("child_model".into(), model.clone());
        }
    }
    meta.insert(
        "transcript_file".into(),
        t.file.to_string_lossy().into_owned(),
    );
    let id = guard.save_with_metadata(project, t.kind, &t.text, &meta)?;
    let _ = guard.tag_row(id, Some(&t.session_id), Some(&sha));
    Ok(())
}

fn turn_provenance(store: &MemoryStore, turn: &Turn) -> Option<InvocationProvenance> {
    provenance_for_sessions(
        store,
        Some(&turn.session_id),
        turn.parent_session.as_deref(),
    )
}

fn provenance_for_sessions(
    store: &MemoryStore,
    child_session_id: Option<&str>,
    parent_session_id: Option<&str>,
) -> Option<InvocationProvenance> {
    [child_session_id, parent_session_id]
        .into_iter()
        .flatten()
        .filter(|session_id| !session_id.trim().is_empty())
        .find_map(|session_id| store.invocation_provenance(session_id).ok().flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SweepFixture {
        _temp: tempfile::TempDir,
        base: PathBuf,
        transcript: PathBuf,
        identity: rtrt_core::ProjectIdentity,
        memory: Arc<Mutex<MemoryStore>>,
        offsets: HashMap<PathBuf, FileCursor>,
        cache: HashMap<PathBuf, Option<String>>,
    }

    impl SweepFixture {
        fn new(initial: &[u8]) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            std::fs::create_dir_all(project.join(".git")).unwrap();
            let base = temp.path().join("claude-projects");
            let encoded = base.join("encoded");
            std::fs::create_dir_all(&encoded).unwrap();
            std::fs::write(
                encoded.join("000-attribution.jsonl"),
                serde_json::json!({"cwd": project}).to_string() + "\n",
            )
            .unwrap();
            let transcript = encoded.join("session.jsonl");
            std::fs::write(&transcript, initial).unwrap();
            let identity = rtrt_core::ProjectIdentity::derive(&project).unwrap();
            let store = MemoryStore::open(temp.path().join("memory.sqlite")).unwrap();
            Self {
                _temp: temp,
                base,
                transcript,
                identity,
                memory: Arc::new(Mutex::new(store)),
                offsets: HashMap::new(),
                cache: HashMap::new(),
            }
        }

        async fn sweep(&mut self) {
            sweep(
                &self.base,
                &self.memory,
                &mut self.offsets,
                &mut self.cache,
                &self.identity,
            )
            .await
            .unwrap();
        }

        async fn bodies(&self) -> Vec<String> {
            self.memory
                .lock()
                .await
                .list_by_project(self.identity.slug(), 20)
                .unwrap()
                .into_iter()
                .map(|row| row.body)
                .collect()
        }
    }

    fn subagent_line(session_id: &str, agent_id: Option<&str>, cwd: Option<&str>) -> String {
        serde_json::json!({
            "type": "assistant",
            "sessionId": session_id,
            "agentId": agent_id,
            "cwd": cwd,
            "message": { "content": [{ "type": "text", "text": "teammate output" }] },
        })
        .to_string()
    }

    /// A `user`-role line with plain-string `message.content` — the common
    /// shape for a real, human-typed prompt.
    fn user_text_line(session_id: &str, cwd: Option<&str>, prompt: &str) -> String {
        serde_json::json!({
            "type": "user",
            "sessionId": session_id,
            "cwd": cwd,
            "message": { "role": "user", "content": prompt },
        })
        .to_string()
    }

    /// A `user`-role line whose `message.content` is a `tool_result` echo —
    /// the harness routes tool results back through the same `user`-role
    /// channel as real typing, so this must never be mistaken for a prompt.
    fn user_tool_result_line(session_id: &str, cwd: Option<&str>) -> String {
        serde_json::json!({
            "type": "user",
            "sessionId": session_id,
            "cwd": cwd,
            "message": {
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_01", "content": "ok" }
                ]
            },
        })
        .to_string()
    }

    #[test]
    fn parent_session_from_path_extracts_the_session_dir() {
        let file = Path::new("/home/u/.claude/projects/-enc-/sess-123/subagents/agent-x.jsonl");
        assert_eq!(parent_session_from_path(file), Some("sess-123".to_string()));
    }

    #[test]
    fn parent_session_from_path_is_none_for_a_main_transcript() {
        let file = Path::new("/home/u/.claude/projects/-enc-/sess-123.jsonl");
        assert_eq!(parent_session_from_path(file), None);
    }

    /// The core guarantee this change adds: a subagent row is attributed to
    /// its real (parent) project THE MOMENT it's captured, using whatever
    /// `resolved_project` the live sweep computed from the still-on-disk
    /// transcript. Once `parse_line` returns, the row no longer depends on
    /// that transcript file existing — even if it's deleted a moment later,
    /// the row it already produced still carries the correct project and
    /// parent session.
    #[test]
    fn captured_subagent_row_lands_in_the_parent_project_at_capture_time() {
        let file = Path::new(
            "/home/u/.claude/projects/-enc-/a1f52dae-1111-2222-3333-197bb559b207/subagents/agent-code-reviewer.jsonl",
        );
        let line = subagent_line(
            "a1f52dae-1111-2222-3333-197bb559b207",
            Some("agent-code-reviewer"),
            // A worktree cwd that, on its own, would NOT resolve to the real
            // project — the point is that `resolved_project` (computed once,
            // at capture time, from the encoded dir) wins regardless.
            Some("/home/u/repo/.worktrees/scratch"),
        );
        let turn = parse_line(&line, file, Some("00G_AI-Project-Setup"))
            .expect("assistant turn with text parses");
        assert_eq!(turn.project, "00G_AI-Project-Setup");
        assert_eq!(
            turn.parent_session,
            Some("a1f52dae-1111-2222-3333-197bb559b207".to_string())
        );
        assert_eq!(turn.kind, "teammate-message");
        assert_eq!(turn.source_kind, "subagent");
        // The transcript file is now free to disappear (rotation, cleanup,
        // whatever) — nothing about the saved row depends on it anymore.
    }

    #[test]
    fn fallback_capture_bucket_is_classifiable_and_never_empty() {
        let with_agent = fallback_capture_bucket("sess-1", Some("agent-7"));
        assert!(rtrt_memory::is_capture_bucket_name(&with_agent));

        let session_only = fallback_capture_bucket("sess-1", None);
        assert!(rtrt_memory::is_capture_bucket_name(&session_only));

        let unknown_everything = fallback_capture_bucket("", None);
        assert!(rtrt_memory::is_capture_bucket_name(&unknown_everything));
    }

    #[test]
    fn opencode_temp_lane_folds_to_the_longest_scoped_project_stem() {
        let base = Path::new("/home/u/.claude/projects");
        let file = base.join("-tmp-opencode-oxrdp-planar-sol/session.jsonl");
        let projects = vec![
            "00G_oxrdp".to_string(),
            "00G_oxrdp-tools".to_string(),
            "oxrdp-planar-sol".to_string(),
        ];

        assert_eq!(
            canonical_project_for_opencode_temp(&file, base, "oxrdp-planar-sol", &projects),
            "00G_oxrdp"
        );
        assert_eq!(
            canonical_project_for_opencode_temp(
                &base.join("-home-u-project/session.jsonl"),
                base,
                "oxrdp-planar-sol",
                &projects
            ),
            "oxrdp-planar-sol"
        );
    }

    /// When capture-time resolution genuinely can't determine a project (no
    /// encoded-dir project AND no resolvable line cwd), the turn must still
    /// be captured — never silently dropped — and land somewhere the orphan
    /// classifier (and the dashboard's hidden-bucket count) will catch.
    #[test]
    fn unresolvable_project_falls_back_to_a_classifiable_bucket_instead_of_dropping() {
        let file =
            Path::new("/home/u/.claude/projects/-enc-/sess-999/subagents/agent-orphan.jsonl");
        let line = subagent_line("sess-999", Some("agent-orphan"), None);
        let turn =
            parse_line(&line, file, None).expect("turn is still captured even when unattributable");
        assert!(
            rtrt_memory::is_capture_bucket_name(&turn.project),
            "fallback project `{}` should be a classifiable capture bucket",
            turn.project
        );
    }

    #[test]
    fn real_user_prompt_is_captured_as_user_prompt_submit() {
        let file = Path::new("/home/u/.claude/projects/-enc-/sess-1.jsonl");
        let line = user_text_line("sess-1", Some("/home/u/repo"), "fix the flaky test");
        let turn = parse_line(&line, file, Some("00G_rtrt")).expect("real user prompt line parses");
        assert_eq!(turn.kind, "user-prompt-submit");
        assert_eq!(turn.source_kind, "main");
        assert_eq!(turn.text, "fix the flaky test");
        assert_eq!(turn.project, "00G_rtrt");
    }

    /// Even inside a `/subagents/` transcript, a captured user prompt is the
    /// parent handing the subagent its task — human-authored main-session
    /// input, never subagent-produced output.
    #[test]
    fn user_prompt_inside_a_subagent_transcript_is_still_tagged_main() {
        let file =
            Path::new("/home/u/.claude/projects/-enc-/sess-1/subagents/agent-code-reviewer.jsonl");
        let line = user_text_line("sess-1", None, "review this diff for bugs");
        let turn = parse_line(&line, file, Some("00G_rtrt")).expect("user prompt line parses");
        assert_eq!(turn.kind, "user-prompt-submit");
        assert_eq!(turn.source_kind, "main");
    }

    #[test]
    fn tool_result_echo_is_not_captured_as_a_prompt() {
        let file = Path::new("/home/u/.claude/projects/-enc-/sess-1.jsonl");
        let line = user_tool_result_line("sess-1", Some("/home/u/repo"));
        assert!(
            parse_line(&line, file, Some("00G_rtrt")).is_none(),
            "a tool_result echo routed through a user-role line must not be captured"
        );
    }

    #[test]
    fn synthetic_task_notification_is_not_captured() {
        let file = Path::new("/home/u/.claude/projects/-enc-/sess-1.jsonl");
        let line = user_text_line(
            "sess-1",
            Some("/home/u/repo"),
            "<task-notification>\n<task-id>bxyz</task-id>\n</task-notification>",
        );
        assert!(
            parse_line(&line, file, Some("00G_rtrt")).is_none(),
            "a harness-injected synthetic prompt must not be captured"
        );
    }

    #[test]
    fn assistant_line_is_still_captured_as_assistant_turn() {
        let file = Path::new("/home/u/.claude/projects/-enc-/sess-1.jsonl");
        let line = serde_json::json!({
            "type": "assistant",
            "sessionId": "sess-1",
            "cwd": "/home/u/repo",
            "message": { "content": [{ "type": "text", "text": "here's the fix" }] },
        })
        .to_string();
        let turn = parse_line(&line, file, Some("00G_rtrt")).expect("assistant line parses");
        assert_eq!(turn.kind, "assistant-turn");
        assert_eq!(turn.source_kind, "main");
        assert_eq!(turn.text, "here's the fix");
    }

    #[test]
    fn transcript_directory_fingerprint_rejects_same_basename_spoof() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("one").join("repo");
        let foreign = temp.path().join("two").join("repo");
        std::fs::create_dir_all(current.join(".git")).unwrap();
        std::fs::create_dir_all(foreign.join(".git")).unwrap();
        let base = temp.path().join("claude-projects");
        let encoded = base.join("encoded-foreign");
        std::fs::create_dir_all(&encoded).unwrap();
        let transcript = encoded.join("session.jsonl");
        std::fs::write(
            &transcript,
            serde_json::json!({"cwd": foreign, "type": "user", "message": {"content": "spoof"}})
                .to_string()
                + "\n",
        )
        .unwrap();
        let identity = rtrt_core::ProjectIdentity::derive(current).unwrap();
        assert!(!transcript_matches(&transcript, &base, &identity));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_swapped_transcripts_are_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real.jsonl");
        let link = temp.path().join("link.jsonl");
        std::fs::write(&real, b"{}\n").unwrap();
        symlink(&real, &link).unwrap();
        assert!(open_regular_file(&link).is_err());

        let (_, identity, _) = open_regular_file(&real).unwrap();
        let replacement = temp.path().join("replacement.jsonl");
        std::fs::write(&replacement, b"{}\n").unwrap();
        std::fs::rename(&replacement, &real).unwrap();
        assert!(!opened_file_is_current(&real, identity));
    }

    #[tokio::test]
    async fn normal_incremental_capture_waits_for_complete_lines() {
        let mut fixture = SweepFixture::new(b"");
        let first = user_text_line("sess-1", None, "one");
        std::fs::write(&fixture.transcript, first.as_bytes()).unwrap();
        fixture.sweep().await;
        assert!(fixture.bodies().await.is_empty());

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&fixture.transcript)
            .unwrap();
        writeln!(file).unwrap();
        writeln!(file, "{}", user_text_line("sess-1", None, "two")).unwrap();
        fixture.sweep().await;
        let bodies = fixture.bodies().await;
        assert!(bodies.contains(&"one".to_string()));
        assert!(bodies.contains(&"two".to_string()));
    }

    #[tokio::test]
    async fn oversized_line_is_skipped_and_cannot_stall_following_capture() {
        let mut bytes = vec![b'x'; MAX_READ_PER_FILE as usize + MAX_JSONL_LINE];
        bytes.push(b'\n');
        bytes.extend_from_slice(user_text_line("sess-1", None, "after giant").as_bytes());
        bytes.push(b'\n');
        let mut fixture = SweepFixture::new(&bytes);

        fixture.sweep().await;
        let first_offset = fixture.offsets[&fixture.transcript].offset;
        assert_eq!(first_offset, MAX_READ_PER_FILE);
        assert!(fixture.bodies().await.is_empty());
        fixture.sweep().await;
        assert_eq!(fixture.bodies().await, vec!["after giant"]);
    }

    #[tokio::test]
    async fn truncation_resets_cursor_without_parsing_old_partial_bytes() {
        let initial = format!("{}\n", user_text_line("sess-1", None, "before"));
        let mut fixture = SweepFixture::new(initial.as_bytes());
        fixture.sweep().await;
        std::fs::write(
            &fixture.transcript,
            format!("{}\n", user_text_line("sess-2", None, "new")),
        )
        .unwrap();
        fixture.sweep().await;
        let bodies = fixture.bodies().await;
        assert!(bodies.contains(&"before".to_string()));
        assert!(bodies.contains(&"new".to_string()));
    }

    #[tokio::test]
    async fn duplicate_body_in_same_project_is_saved_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(tmp.path().join("mem.sqlite")).expect("open temp store");
        let memory = Arc::new(Mutex::new(store));

        let turn = Turn {
            project: "00G_rtrt".to_string(),
            text: "why is memory missing my questions?".to_string(),
            session_id: "sess-1".to_string(),
            parent_session: None,
            agent_id: None,
            slug: None,
            file: PathBuf::from("/home/u/.claude/projects/-enc-/sess-1.jsonl"),
            kind: "user-prompt-submit",
            source_kind: "main",
        };

        save_turn(&memory, &turn, "00G_rtrt")
            .await
            .expect("first save succeeds");
        save_turn(&memory, &turn, "00G_rtrt")
            .await
            .expect("second save succeeds");

        let guard = memory.lock().await;
        let rows = guard
            .list_by_project("00G_rtrt", 10)
            .expect("list rows back");
        assert_eq!(rows.len(), 1, "the same body must dedup to a single row");
        assert_eq!(rows[0].kind, "user-prompt-submit");
        assert_eq!(rows[0].body, turn.text);
    }

    #[tokio::test]
    async fn durable_provenance_overrides_transcript_path_attribution() {
        let store = MemoryStore::open_in_memory().expect("open memory store");
        store
            .upsert_invocation_provenance(&InvocationProvenance {
                child_session_id: "child-session".into(),
                invocation_id: "invocation-1".into(),
                parent_project: "00G_rtrt".into(),
                parent_session_id: Some("opencode-session".into()),
                parent_call_id: Some("call-1".into()),
                caller_agent: Some("build".into()),
                parent_cwd: Some("/tmp/opencode/rtrt-lane".into()),
                parent_worktree: Some("/repo/00G_rtrt".into()),
                target: Some("claude".into()),
                model: Some("sonnet".into()),
                created_at: 1,
            })
            .expect("save provenance");
        let memory = Arc::new(Mutex::new(store));
        let turn = Turn {
            project: "rtrt-lane".into(),
            text: "child result".into(),
            session_id: "child-session".into(),
            parent_session: None,
            agent_id: None,
            slug: None,
            file: PathBuf::from("/home/u/.claude/projects/-tmp-opencode-rtrt-lane/child.jsonl"),
            kind: "assistant-turn",
            source_kind: "main",
        };

        save_turn(&memory, &turn, "00G_rtrt")
            .await
            .expect("save attributed turn");

        let guard = memory.lock().await;
        assert!(guard.list_by_project("rtrt-lane", 10).unwrap().is_empty());
        let rows = guard.list_by_project("00G_rtrt", 10).unwrap();
        assert_eq!(rows.len(), 1);
        let metadata = guard.get_metadata(rows[0].id).unwrap();
        assert_eq!(
            metadata.get("invocation_id").map(String::as_str),
            Some("invocation-1")
        );
        assert_eq!(
            metadata.get("caller_agent").map(String::as_str),
            Some("build")
        );
    }

    #[test]
    fn provenance_lookup_prefers_child_then_falls_back_to_stored_parent() {
        let store = MemoryStore::open_in_memory().expect("open memory store");
        let provenance = |child: &str, project: &str| InvocationProvenance {
            child_session_id: child.into(),
            invocation_id: format!("invocation-{child}"),
            parent_project: project.into(),
            parent_session_id: None,
            parent_call_id: None,
            caller_agent: None,
            parent_cwd: None,
            parent_worktree: None,
            target: Some("claude".into()),
            model: None,
            created_at: 1,
        };
        store
            .upsert_invocation_provenance(&provenance("parent-session", "parent-project"))
            .unwrap();

        let fallback =
            provenance_for_sessions(&store, Some("missing-child"), Some("parent-session"))
                .expect("parent fallback");
        assert_eq!(fallback.parent_project, "parent-project");

        store
            .upsert_invocation_provenance(&provenance("child-session", "child-project"))
            .unwrap();
        let preferred =
            provenance_for_sessions(&store, Some("child-session"), Some("parent-session"))
                .expect("child provenance");
        assert_eq!(preferred.parent_project, "child-project");
    }
}
