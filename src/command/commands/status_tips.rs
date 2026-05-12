//! Status-screen tips. Ported verbatim from `oldsrc/commands/status.rs::TIPS`.

/// 50 tips shown at the bottom of the status dashboard. The tip displayed on
/// any given invocation is selected by [`select_random_tip`] using the current
/// unix-second as a seed.
pub const TIPS: &[&str] = &[
    "`amux status` shows all running code agents.",
    "`amux status --watch` auto-refreshes every 3 seconds. Press Ctrl-C to stop.",
    "`amux exec workflow <file>` runs a workflow inside a container.",
    "`amux chat` opens an interactive chat session with your configured agent.",
    "`amux ready` checks your environment and builds the Docker image if needed.",
    "`amux ready --refresh` re-runs the OAuth token refresh before launching.",
    "`amux ready --build` forces a Docker image rebuild even if one exists.",
    "`amux ready --no-cache` rebuilds the Docker image from scratch with no layer cache.",
    "`amux ready --build --no-cache` is the nuclear option for a fully clean image.",
    "`amux new` guides you through creating a new work item interactively.",
    "Work items live in `aspec/work-items/` and use a numbered Markdown format.",
    "Per-repo config lives at `<git-root>/aspec/.amux.json`.",
    "Global config lives at `~/.amux/config.json`.",
    "Agent data and state is stored in `~/.amux/`.",
    "Agents always run inside Docker containers — never directly on the host.",
    "Only the current Git repo root is mounted into agent containers.",
    "The `amux` binary is statically linked — no runtime dependencies to install.",
    "Press Ctrl+T in the TUI to open a new tab with its own working directory.",
    "Use Ctrl+A and Ctrl+D to switch between tabs in the TUI.",
    "Press Ctrl+C in the TUI (single tab) to open the quit confirmation dialog.",
    "Press `q` in an empty command box to open the quit confirmation dialog.",
    "Press the Up arrow in the command box to navigate to the execution window.",
    "In the execution window, press `b` to jump to the start of output.",
    "In the execution window, press `e` to jump to the end (latest) output.",
    "In the execution window, press Up/Down arrows to scroll through output.",
    "Press Esc in the execution window to return focus to the command box.",
    "When a container is running, press `c` to maximise its window for full interaction.",
    "The container window can be minimised with Esc, leaving the outer window scrollable.",
    "A yellow tab name means the container has been idle for over 30 seconds.",
    "CPU and memory stats for running containers are polled and displayed live.",
    "Agent credentials are read from the system keychain automatically.",
    "Multiple tabs let you monitor and run agents in different repos simultaneously.",
    "The `ready` command checks local agent installation before launching a container.",
    "Docker images are built from `Dockerfile.dev` in your repo root.",
    "amux supports Claude Code, Codex, and Opencode as agent backends.",
    "Work items can be of type Feature, Bug, or Task.",
    "The TUI auto-starts `status --watch` when launched outside a Git repo.",
    "`amux exec workflow` runs a workflow file inside a sandboxed container.",
    "The `new` command creates work items using the template in `aspec/work-items/0000-template.md`.",
    "Container output streams live to the TUI execution window with full ANSI colour.",
    "The VT100 terminal emulator in the container window supports colours, bold, and cursor movement.",
    "Scroll the container window with the mouse wheel when it is maximised.",
    "Each amux tab maintains independent output history that you can scroll through after a command.",
    "Run `amux` from any subdirectory of a Git repo — it locates the root automatically.",
    "amux never mounts parent directories above the Git root into containers.",
];

/// Select a tip using the current unix-second as a seed. Seconds (not nanos)
/// are used because nanosecond timers on common platforms are often multiples
/// of `TIPS.len()`, defeating variance.
pub fn select_random_tip() -> &'static str {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    TIPS[(secs % TIPS.len() as u64) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tips_count_is_45() {
        assert_eq!(TIPS.len(), 45);
    }

    #[test]
    fn select_random_tip_returns_a_tip_from_the_list() {
        let tip = select_random_tip();
        assert!(TIPS.contains(&tip));
    }

    #[test]
    fn no_tip_is_empty() {
        for t in TIPS {
            assert!(!t.is_empty());
        }
    }
}
