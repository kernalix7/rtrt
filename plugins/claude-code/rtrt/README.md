# rtrt — Claude Code plugin

Auto-captures every Claude Code event (PreToolUse / PostToolUse / Stop /
SessionStart / SessionEnd / UserPromptSubmit) into the rtrt memory store
so the conversation history shows up under the project's timeline
without manual `memory save` calls.

## Install

```bash
# Copy the plugin into Claude Code's plugin cache.
mkdir -p ~/.claude/plugins/cache/rtrt
cp -R plugins/claude-code/rtrt/* ~/.claude/plugins/cache/rtrt/
chmod +x ~/.claude/plugins/cache/rtrt/hooks/*.sh

# Enable in your global Claude Code settings.json:
#   "plugins": ["rtrt"]
```

## Configuration

The hooks read these env vars at fire time:

| Env | Default | Purpose |
|------|---------|---------|
| `RTRT_PROJECT` | `$(basename $PWD)` | Project bucket the events land in |
| `RTRT_BIN` | `rtrt` | CLI used to write rows (path or name on PATH) |
| `RTRT_DASHBOARD_URL` | (unset) | Fallback POST target — e.g. `http://127.0.0.1:7311` |
| `RTRT_DASHBOARD_TOKEN` | (unset) | Bearer token for the dashboard fallback |

Write order: CLI first, then dashboard POST. Both paths are best-effort
— a capture failure never blocks the conversation. Payloads are
stripped of control bytes and clipped to 4 KB.

## What lands in memory

| Hook | Memory kind |
|------|-------------|
| PreToolUse | `pre-tool-use` |
| PostToolUse | `post-tool-use` |
| UserPromptSubmit | `user-prompt-submit` |
| Stop | `stop` |
| SessionStart | `session-start` |
| SessionEnd | `session-end` |

Every row carries `metadata.source = "claude-code"` so the qdrant-style
filter (`source=claude-code`) on `/api/memory/recall` returns just the
auto-captured slice.

## Verify

```bash
# After running a Claude Code session, list the project's timeline:
rtrt memory recall --project "$(basename "$PWD")" --query post-tool-use --filter source=claude-code

# Or watch the live feed in the dashboard:
rtrt-dashboard
# → http://127.0.0.1:7311 · 메모리 → (project card) → 히스토리
```
