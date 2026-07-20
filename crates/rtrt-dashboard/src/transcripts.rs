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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rtrt_memory::{MemoryStore, is_synthetic_prompt};
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
            if let Some(turn) = parse_line(s, &path, resolved_project.as_deref()) {
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
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
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

async fn save_turn(memory: &Arc<Mutex<MemoryStore>>, t: &Turn) -> anyhow::Result<()> {
    let sha = rtrt_memory::MemoryStore::body_sha(&t.text);
    let guard = memory.lock().await;
    // Dedup against everything already in this project's bucket — e.g. the
    // live UserPromptSubmit hook and the SessionStart / Stop / SubagentStop
    // hooks already cover a lot of this ground, so the watcher only adds what
    // they miss (backfilled transcripts, subagent transcripts) without
    // doubling up on what's already there.
    if guard
        .body_seen_at(&t.project, &sha)
        .ok()
        .flatten()
        .is_some()
    {
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
    meta.insert(
        "transcript_file".into(),
        t.file.to_string_lossy().into_owned(),
    );
    let id = guard.save_with_metadata(&t.project, t.kind, &t.text, &meta)?;
    let _ = guard.tag_row(id, Some(&t.session_id), Some(&sha));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

        save_turn(&memory, &turn)
            .await
            .expect("first save succeeds");
        save_turn(&memory, &turn)
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
}
