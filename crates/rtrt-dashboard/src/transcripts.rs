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
        let mut tick = tokio::time::interval(DEFAULT_INTERVAL);
        loop {
            tick.tick().await;
            if let Err(e) = sweep(&base, &memory, &mut offsets).await {
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
            if let Some(turn) = parse_assistant_turn(s, &path) {
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

/// Returns `Some` only for lines that look like an `assistant` turn carrying
/// non-empty visible text. Skips thinking-only, tool-use-only, or partial lines.
fn parse_assistant_turn(line: &str, file: &Path) -> Option<AssistantTurn> {
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

    // Project = basename of the line's `cwd`. Require it: the directory
    // fallback would bucket cwd-less lines under junk names like the
    // `subagents` dir or an `agent-<id>` segment, so skip those instead.
    let project = v
        .get("cwd")
        .and_then(|c| c.as_str())
        .and_then(|p| Path::new(p).file_name())
        .and_then(|n| n.to_str())
        .map(String::from)?;

    let session_id = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let agent_id = v.get("agentId").and_then(|s| s.as_str()).map(String::from);
    let slug = v.get("slug").and_then(|s| s.as_str()).map(String::from);

    let is_subagent = file
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("subagents"));

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
