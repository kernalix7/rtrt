//! Background watcher that tails Claude Code session transcripts (the JSONL
//! files under `~/.claude/projects/`) and saves every new assistant turn into
//! the rtrt memory store. Closes the capture gap for teammate / subagent work
//! that runs in its own session (FleetView, Task-tool subagents) and never
//! reaches the main agent's transcript.
//!
//! Layout the watcher knows about:
//!   ~/.claude/projects/<encoded-cwd>/<session>.jsonl
//!   ~/.claude/projects/<encoded-cwd>/<session>/subagents/agent-*.jsonl
//!
//! Both shapes carry standard Claude transcript lines with `cwd`, `sessionId`,
//! optional `agentId` / `slug`, and `message.content[]` parts. The watcher
//! picks `cwd`'s basename as the rtrt project bucket so captures land next to
//! everything else for that repo, and dedups via `MemoryStore::body_seen_at`
//! so existing rows from the SessionStart / Stop / SubagentStop hooks don't
//! get duplicated.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rtrt_memory::MemoryStore;
use serde_json::Value;
use tokio::sync::Mutex;
use walkdir::WalkDir;

/// Polling interval. Cheap — the hot path is reading appended bytes off a few
/// JSONL files, not walking the whole tree (mtime check filters out idle ones).
const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

/// Boot migration: re-home every transcript row onto the project of its
/// `<encoded>` dir (Claude Code's per-project session dir), folding rows that a
/// per-line worktree cwd had scattered into bogus buckets (feat-*, wf_*,
/// agent-*, p<n>-*) back under their real project. No name patterns — purely
/// the file's encoded dir. Idempotent: a settled row is skipped, so the work
/// shrinks to zero across runs.
pub fn spawn_reattribution(memory: Option<Arc<Mutex<MemoryStore>>>) {
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
        for (id, tf, project, source_kind) in candidates {
            let is_subagent = tf.contains("/subagents/");
            let kind = if is_subagent { "subagent" } else { "main" };
            let resolved = project_for_transcript(Path::new(&tf), &base, &mut cache);
            let move_to = match &resolved {
                Some(p) if *p != project => Some(p.as_str()),
                _ => None,
            };
            // Skip the row entirely when it's already in the right project and
            // already classified — no wasted UPDATE on a settled store.
            if move_to.is_none() && source_kind.as_deref() == Some(kind) {
                continue;
            }
            let guard = memory.lock().await;
            if guard.reattribute(id, kind, move_to).is_ok() {
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
pub fn spawn_transcript_watcher(memory: Option<Arc<Mutex<MemoryStore>>>) {
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
        let mut offsets: HashMap<PathBuf, u64> = HashMap::new();
        let mut proj_cache: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut tick = tokio::time::interval(DEFAULT_INTERVAL);
        loop {
            tick.tick().await;
            if let Err(e) = sweep(&base, &memory, &mut offsets, &mut proj_cache).await {
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
    offsets: &mut HashMap<PathBuf, u64>,
    proj_cache: &mut HashMap<PathBuf, Option<String>>,
) -> anyhow::Result<()> {
    let files: Vec<PathBuf> = WalkDir::new(base)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .map(|e| e.into_path())
        .collect();

    for path in files {
        let len = match std::fs::metadata(&path).map(|m| m.len()) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let start = offsets.get(&path).copied().unwrap_or(0);
        // File truncated / rotated — restart from the top.
        let start = if len < start { 0 } else { start };
        if len == start {
            continue;
        }
        let new_bytes = match read_range(&path, start, len) {
            Ok(b) => b,
            Err(_) => continue,
        };
        // Resolve the project from the file's `<encoded>` dir (the real project,
        // worktree-stable), computed once per file and cached per encoded dir.
        let resolved_project = project_for_transcript(&path, base, proj_cache);
        // Track the offset of the *last full* line so we resume cleanly even
        // when the writer is mid-write at the EOF (partial trailing line).
        let mut consumed = start;
        for line in new_bytes.split_inclusive(|&b| b == b'\n') {
            if !line.ends_with(b"\n") {
                break; // partial line — wait for next sweep
            }
            consumed += line.len() as u64;
            // Strip the trailing newline before parsing.
            let s = match std::str::from_utf8(&line[..line.len() - 1]) {
                Ok(s) if !s.trim().is_empty() => s,
                _ => continue,
            };
            if let Some(turn) = parse_assistant_turn(s, &path, resolved_project.as_deref()) {
                if let Err(e) = save_turn(memory, &turn).await {
                    tracing::warn!("transcript save {}: {e}", path.display());
                }
            }
        }
        offsets.insert(path, consumed);
    }
    Ok(())
}

fn read_range(path: &Path, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (end - start) as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

struct AssistantTurn {
    project: String,
    text: String,
    session_id: String,
    agent_id: Option<String>,
    slug: Option<String>,
    file: PathBuf,
    is_subagent: bool,
}

/// The project a transcript file belongs to. Claude Code stores every session
/// of one project under a single `~/.claude/projects/<encoded>/` directory
/// (keyed by the session's starting cwd). We derive the project from that
/// `<encoded>` dir's representative cwd basename — NOT the per-line cwd, which
/// can switch to a git-worktree path (a branch name like `feat-discovery-core`)
/// mid-session and scatter rows into bogus buckets. Subagent / workflow
/// transcripts live under the same `<encoded>` dir, so they resolve to the same
/// real project automatically. Result is cached per `<encoded>` dir.
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

/// Representative project name for an `<encoded>` dir: the basename of the cwd
/// found in its first top-level session transcript (deterministic by sorted
/// filename). Top-level only — we skip the `subagents/` subtree, whose cwds may
/// be worktrees.
fn representative_project(encoded_dir: &Path) -> Option<String> {
    let rd = std::fs::read_dir(encoded_dir).ok()?;
    let mut sessions: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect();
    sessions.sort();
    for s in &sessions {
        if let Some(cwd) = first_cwd_in(s) {
            return Path::new(&cwd)
                .file_name()
                .and_then(|n| n.to_str())
                .map(String::from);
        }
    }
    None
}

/// Read the first `cwd` field from a transcript file (scanning the first lines).
fn first_cwd_in(jsonl: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(jsonl).ok()?;
    for line in BufReader::new(f).lines().map_while(Result::ok).take(50) {
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                return Some(c.to_string());
            }
        }
    }
    None
}

/// Returns `Some` only for lines that look like an `assistant` turn carrying
/// non-empty visible text. Skips thinking-only, tool-use-only, or partial lines.
/// `resolved_project` (the file's `<encoded>` dir project) is authoritative and
/// overrides the line's own cwd for BOTH main and subagent rows.
fn parse_assistant_turn(
    line: &str,
    file: &Path,
    resolved_project: Option<&str>,
) -> Option<AssistantTurn> {
    let v: Value = serde_json::from_str(line).ok()?;
    // Top-level `type` is "assistant" on every Claude transcript line that
    // carries an assistant message; also accept `message.role == "assistant"`
    // as a fallback for older formats.
    let is_assistant = v.get("type").and_then(|t| t.as_str()) == Some("assistant")
        || v.get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            == Some("assistant");
    if !is_assistant {
        return None;
    }
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
        return None;
    }

    let is_subagent = file
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("subagents"));

    // Project: the file's `<encoded>` dir project is authoritative (one project
    // per dir, worktree-stable) for both main and subagent rows. Fall back to
    // the line's own cwd basename only if the dir couldn't be resolved.
    let line_project = v
        .get("cwd")
        .and_then(|c| c.as_str())
        .and_then(|p| Path::new(p).file_name())
        .and_then(|n| n.to_str())
        .map(String::from);
    let project = resolved_project.map(String::from).or(line_project)?;

    let session_id = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let agent_id = v.get("agentId").and_then(|s| s.as_str()).map(String::from);
    let slug = v.get("slug").and_then(|s| s.as_str()).map(String::from);

    Some(AssistantTurn {
        project,
        text: text.to_string(),
        session_id,
        agent_id,
        slug,
        file: file.to_path_buf(),
        is_subagent,
    })
}

async fn save_turn(memory: &Arc<Mutex<MemoryStore>>, t: &AssistantTurn) -> anyhow::Result<()> {
    let sha = rtrt_memory::MemoryStore::body_sha(&t.text);
    let guard = memory.lock().await;
    // Dedup against everything already in this project's bucket — the
    // SessionStart / Stop / SubagentStop hooks already cover the main agent's
    // turns, so this watcher only adds the teammate / subagent outputs they
    // miss without doubling up on what's there.
    if guard
        .body_seen_at(&t.project, &sha)
        .ok()
        .flatten()
        .is_some()
    {
        return Ok(());
    }
    let kind = if t.is_subagent {
        "teammate-message"
    } else {
        "assistant-turn"
    };
    let mut meta: BTreeMap<String, String> = BTreeMap::new();
    meta.insert("source".into(), "transcript".into());
    // Classify the row so the UI can split a project's main-agent work from its
    // subagent / teammate work.
    meta.insert(
        "source_kind".into(),
        if t.is_subagent { "subagent" } else { "main" }.into(),
    );
    if !t.session_id.is_empty() {
        meta.insert("session_id".into(), t.session_id.clone());
    }
    if let Some(a) = &t.agent_id {
        meta.insert("agent_id".into(), a.clone());
    }
    if let Some(s) = &t.slug {
        meta.insert("slug".into(), s.clone());
    }
    meta.insert(
        "transcript_file".into(),
        t.file.to_string_lossy().into_owned(),
    );
    let id = guard.save_with_metadata(&t.project, kind, &t.text, &meta)?;
    let _ = guard.tag_row(id, Some(&t.session_id), Some(&sha));
    Ok(())
}
