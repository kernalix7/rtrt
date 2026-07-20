//! Shared prompt-capture classification used by every surface that turns a
//! Claude Code hook payload or transcript line into a memory row.
//!
//! [`is_synthetic_prompt`] is the single source of truth for "is this text a
//! harness-injected event rather than something the user typed" — the live
//! `UserPromptSubmit` CLI hook (`rtrt-cli`) and the transcript watcher
//! (`rtrt-dashboard`) both route through it so a background-task
//! notification or `<task-notification>` block is filtered out identically
//! everywhere, instead of each capture site growing its own (possibly
//! diverging) copy of the same heuristic.

/// True when a "user prompt" is actually a harness-injected event rather than
/// something the user typed — a background-task notification, a
/// `<task-notification>` block, or bare `task-id:` / `tool-use-id:` metadata.
/// Claude Code routes these through the same channel real user prompts use
/// (the `UserPromptSubmit` hook payload, and — for the transcript watcher —
/// `user`-role transcript lines), so every capture site must filter them out
/// to keep the memory prompt stream clean.
pub fn is_synthetic_prompt(prompt: &str) -> bool {
    let p = prompt.trim_start();
    // Explicit harness markers — these payloads announce themselves.
    if p.contains("SYSTEM NOTIFICATION - NOT USER INPUT")
        || p.contains("<task-notification>")
        || p.contains("This is an automated background-task event")
    {
        return true;
    }
    // Bare task/tool-use metadata dumps (e.g. "task-id: bxyz, tool-use-id:
    // toolu_..., output-file: ..."). Real prompts don't lead with these keys.
    let head = p.get(..64).unwrap_or(p);
    (head.starts_with("task-id:") || head.starts_with("task-id "))
        && (p.contains("tool-use-id") || p.contains("output-file"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_prompts_are_skipped() {
        // Harness-injected events that arrive through the UserPromptSubmit channel.
        assert!(is_synthetic_prompt(
            "[SYSTEM NOTIFICATION - NOT USER INPUT]\nThis is an automated background-task event"
        ));
        assert!(is_synthetic_prompt(
            "<task-notification>\n<task-id>bxyz</task-id>\n</task-notification>"
        ));
        assert!(is_synthetic_prompt(
            "task-id: bs2ne03kz, tool-use-id: toolu_01QyNuv9, output-file: /tmp/x.output"
        ));
        assert!(is_synthetic_prompt(
            "task-id:a5c726cce05,tool-use-id:toolu_01BpPZ,output-file:/tmp/y"
        ));
    }

    #[test]
    fn real_prompts_are_kept() {
        assert!(!is_synthetic_prompt(
            "근데 내가 입력한 항목은 왜 안떠 메모리에?"
        ));
        assert!(!is_synthetic_prompt("지금 왜 서비스 죽어있어?"));
        assert!(!is_synthetic_prompt(
            "fix the task-id parsing in the ledger" // mentions task-id but is real typing
        ));
        assert!(!is_synthetic_prompt("進行시켜"));
    }
}
