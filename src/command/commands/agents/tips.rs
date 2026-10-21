//! Tips shown in the `awman agents status` footer.
//!
//! Reviewed and refreshed before every release (see the `release-prep`
//! skill). Every `` `awman …` `` command and flag a tip names is checked
//! against the `CommandCatalogue` by the tests below, so a renamed or removed
//! command fails the build instead of leaving a stale tip behind.

/// Tips shown at the bottom of the agents dashboard, grouped by topic. The tip
/// displayed on any given invocation is selected by [`select_random_tip`]
/// using the current unix-second as a seed.
pub const TIPS: &[&str] = &[
    // ── Getting started ──────────────────────────────────────────────────────
    "`awman init` sets up the current Git repo; add `--agent codex` to pick a different agent.",
    "`awman init --aspec` also downloads the aspec spec templates into your project.",
    "`awman ready` checks your runtime and builds or imports the agent image if needed.",
    "`awman ready --build` forces an image rebuild even if one already exists.",
    "`awman ready --build --no-cache` rebuilds the agent image from scratch with no layer cache.",
    "`awman ready --refresh` runs the Dockerfile agent audit, which is skipped by default.",
    "`awman ready --json` prints a machine-readable readiness report for scripts and CI.",
    "Run `awman` from any subdirectory of a Git repo — it locates the root automatically.",
    "Run `awman` with no arguments to open the TUI; add a subcommand to run it once and exit.",
    "Supported agents: Claude, Codex, Opencode, Maki, Gemini, Copilot, Crush, Cline and Antigravity.",
    // ── Agent sessions ───────────────────────────────────────────────────────
    "`awman chat` opens an interactive chat session with your configured agent.",
    "`awman chat --plan` starts the agent in read-only plan mode.",
    "`awman chat --agent gemini --model <model>` overrides the agent and model for one session.",
    "`awman exec prompt \"<text>\"` sends a one-shot prompt to the agent.",
    "`awman exec prompt --issue 42` uses a GitHub issue as the prompt.",
    "`--yolo` enables fully autonomous mode; `--auto` enables auto permission mode.",
    "`--agent-flag=--verbose` passes an extra argument straight to the agent command.",
    "`--launch-mode acp` launches the agent over ACP instead of a raw terminal.",
    "`-n` runs the agent in non-interactive (print) mode instead of an interactive session.",
    // ── Workflows ────────────────────────────────────────────────────────────
    "`awman exec workflow <file>` (or `awman exec wf`) runs a multi-step workflow.",
    "`awman exec workflow <file> --work-item 0042` fills workflow templates from a work item.",
    "`awman exec workflow <file> --issue 42` runs a workflow against a GitHub issue.",
    "`awman exec workflow --dynamic --work-item 0042` lets a leader agent design the workflow.",
    "`awman exec workflow <file> --worktree` runs in an isolated Git worktree.",
    "`--yolo` and `--auto` on a workflow imply `--worktree`, keeping your checkout untouched.",
    "`awman exec workflow <file> --max-concurrent 2` caps how many steps run in parallel.",
    "Workflow files can be TOML or YAML, with optional setup and teardown phases.",
    "`awman new workflow` builds a workflow step by step; `--interview` lets an agent draft it.",
    "`awman new workflow --global` saves the workflow to `~/.awman/workflows/` for every repo.",
    "Press Ctrl+W during a workflow to open the workflow control board.",
    "Press Ctrl+O to minimize or maximize the workflow overview.",
    "Use Shift+← / Shift+→ to scroll workflow stages that don't fit on screen.",
    // ── Specs and skills ─────────────────────────────────────────────────────
    "`awman new spec` creates a work item from `aspec/work-items/0000-template.md`.",
    "`awman new spec --issue 42` turns a GitHub issue into a work item spec.",
    "`awman specs amend 0042` updates a completed work item to match what was actually built.",
    "`awman new skill` creates a skill; add `--global` to share it with every repo.",
    "`awman new skill --pull github.com/obra/superpowers` pulls a published skills library.",
    "`awman new skill --pull-all` refreshes every skills library you've pulled.",
    // ── Overlays ─────────────────────────────────────────────────────────────
    "`--overlay \"dir(~/notes:/mnt/notes:ro)\"` mounts a host directory into the agent read-only.",
    "The `ssh()` overlay gives the agent your SSH keys so it can push to remotes.",
    "The `env(GITHUB_TOKEN)` overlay passes one host environment variable into the agent.",
    "The `skill(*)` overlay makes your global awman skills available to the agent.",
    "Overlays from config, `AWMAN_OVERLAYS`, `--overlay` flags and workflow steps all combine.",
    // ── Monitoring and cleanup ───────────────────────────────────────────────
    "`awman agents status` prints a one-shot summary of every running code agent.",
    "`awman agents status --watch` keeps the dashboard refreshing. Press Ctrl-C to stop.",
    "`awman agents ls` (or `ps`) lists running agents; add `-a` to include stopped ones.",
    "`awman agents ls -q` prints only agent IDs, handy for scripting.",
    "`awman agents detail <id>` (or `inspect`) shows one agent in depth; an ID prefix works too.",
    "`awman agents attach <id>` connects to an agent's terminal; detaching never stops the agent.",
    "`awman agents rm <id>` (or `kill`) stops and removes an agent; `-y` skips the prompt.",
    "`awman clean` removes stopped agents, finished workflow data and dangling images.",
    "`awman clean --dry-run` shows what would be removed without deleting anything.",
    "`awman clean -y` skips the confirmation prompt, which is handy in cron jobs.",
    "A yellow tab or container bar means the agent has been silent for over 30 seconds.",
    // ── TUI ──────────────────────────────────────────────────────────────────
    "Press Ctrl+T in the TUI to open a new tab with its own working directory.",
    "Use Ctrl+A and Ctrl+D to switch between tabs in the TUI.",
    "Press Ctrl+M to cycle the container window between minimized, hidden and maximized.",
    "Press Ctrl+\\ to detach from a container view without sending anything to the agent.",
    "Press Ctrl+S to move focus to the next running container when several are running.",
    "Press Ctrl+G to toggle the Git sidebar with live staged and unstaged changes.",
    "Press Ctrl+, anywhere in the TUI to open the config editor.",
    "Click and drag to select output, then press Ctrl+Y to copy it.",
    "Press the Up arrow in the command box to focus the execution window.",
    "In the execution window, press `b` / `e` to jump to the beginning / end of output.",
    "In the execution window, press `l` to expand or collapse awman's status log.",
    "Press Tab in the command box to cycle through command and flag suggestions.",
    "Mistyped a command? awman suggests the closest match, subcommands included.",
    "Press `q` in an empty command box to quit, or Ctrl+C to open the quit dialog.",
    "In the New Tab dialog (Ctrl+T), press Ctrl+S to open a squad tab.",
    // ── Configuration ────────────────────────────────────────────────────────
    "Per-repo config lives at `<git-root>/.awman/config.json`.",
    "Global config lives at `~/.awman/config.json`; repo values override it.",
    "`awman config show` lists every setting at global and repo level.",
    "`awman config get <field>` shows a setting's global, repo and effective values.",
    "`awman config set <field> <value> --global` writes to global instead of repo config.",
    // ── Runtimes and security ────────────────────────────────────────────────
    "Agents never run on your host: they run in Docker, Apple Containers, Docker Sandboxes or a microVM.",
    "`awman config set runtime apple-containers --global` switches to Apple Containers.",
    "On Apple Silicon Macs, the `builtin-experimental` runtime runs each agent in a microVM.",
    "The builtin runtime's `network` setting can be `public`, `allowlist` or `none`.",
    "Only the current Git repo root is mounted into agents — never its parent directories.",
    "`--allow-docker` mounts the host Docker socket into the agent; use it only when needed.",
    // ── Squad ────────────────────────────────────────────────────────────────
    "Your squad is a group of agents that work on scheduled tasks for you: try `awman squad`.",
    "`awman squad add --name triage --interval 6h` creates a task your squad evaluates every 6 hours.",
    "`awman squad trigger <task>` evaluates a task now, ignoring its schedule.",
    "`awman squad attach <task>` attaches to the agent running a squad task.",
    "`awman squad pause <task>` and `awman squad resume <task>` toggle a task's schedule.",
    "`awman squad env --push` re-sends env() values to the squad daemon after rotating a token.",
    "`awman squad logs -f` follows the squad daemon log.",
    // ── API and remote ───────────────────────────────────────────────────────
    "`awman api start --background` runs the awman HTTP API as a background service.",
    "`awman api status` and `awman api logs` check on the background API server.",
    "`awman remote exec workflow <file> --follow` runs a workflow on a remote awman host.",
    "Set `remote.defaultAddr` in config to bind new TUI tabs to a remote API session.",
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
    use crate::command::dispatch::catalogue::{CommandCatalogue, CommandSpec};

    /// Longest tip that still fits on one dashboard line in an 80-column
    /// terminal once the `Tip: ` prefix is added.
    const MAX_TIP_CHARS: usize = 100;

    /// Every `` `awman …` `` span in a tip, without the surrounding backticks.
    fn awman_invocations(tip: &str) -> Vec<&str> {
        tip.split('`')
            .skip(1)
            .step_by(2)
            .filter(|span| *span == "awman" || span.starts_with("awman "))
            .collect()
    }

    /// Walk one invocation through the catalogue: each word must be a
    /// subcommand (or alias) while the current command has subcommands, and
    /// every flag must exist on the command it ends at. Placeholders and
    /// positional values (`<file>`, `0042`, `"<text>"`) are skipped.
    fn check_invocation(invocation: &str) -> Result<(), String> {
        let mut spec: &CommandSpec = CommandCatalogue::get().root();
        let mut tokens = invocation.split_whitespace().skip(1).peekable();
        while let Some(word) = tokens.peek() {
            if word.starts_with('-') || spec.subcommands.is_empty() {
                break;
            }
            spec = spec
                .find_subcommand(word)
                .ok_or_else(|| format!("unknown subcommand `{word}` in `{invocation}`"))?;
            tokens.next();
        }
        for token in tokens {
            if let Some(long) = token.strip_prefix("--") {
                let name = long.split('=').next().unwrap_or(long);
                if spec.find_flag(name).is_none() {
                    return Err(format!("unknown flag `--{name}` in `{invocation}`"));
                }
            } else if let Some(short) = token.strip_prefix('-') {
                for c in short.chars() {
                    if !spec.flags.iter().any(|f| f.short == Some(c)) {
                        return Err(format!("unknown short flag `-{c}` in `{invocation}`"));
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn select_random_tip_returns_a_tip_from_the_list() {
        let tip = select_random_tip();
        assert!(TIPS.contains(&tip));
    }

    #[test]
    fn no_tip_is_empty() {
        for t in TIPS {
            assert!(!t.trim().is_empty());
        }
    }

    #[test]
    fn no_tip_is_duplicated() {
        let mut seen = std::collections::HashSet::new();
        for t in TIPS {
            assert!(seen.insert(*t), "duplicate tip: {t}");
        }
    }

    #[test]
    fn every_tip_fits_on_one_line() {
        for t in TIPS {
            assert!(
                t.chars().count() <= MAX_TIP_CHARS,
                "tip is {} chars (max {MAX_TIP_CHARS}): {t}",
                t.chars().count()
            );
        }
    }

    #[test]
    fn every_awman_command_in_a_tip_exists_in_the_catalogue() {
        let errors: Vec<String> = TIPS
            .iter()
            .flat_map(|tip| awman_invocations(tip))
            .filter_map(|invocation| check_invocation(invocation).err())
            .collect();
        assert!(errors.is_empty(), "stale tips:\n{}", errors.join("\n"));
    }

    #[test]
    fn no_tip_names_the_removed_status_command() {
        for tip in TIPS {
            for invocation in awman_invocations(tip) {
                assert!(
                    invocation != "awman status" && !invocation.starts_with("awman status "),
                    "stale `awman status` in tip: {tip}"
                );
            }
            assert!(!tip.contains("awman status"), "stale tip: {tip}");
        }
    }

    #[test]
    fn agents_tips_cover_each_subcommand() {
        for needle in [
            "agents status",
            "agents ls",
            "agents detail",
            "agents attach",
            "agents rm",
        ] {
            assert!(
                TIPS.iter().any(|t| t.contains(needle)),
                "no tip mentions `awman {needle}`"
            );
        }
    }

    #[test]
    fn invocation_check_rejects_unknown_commands_and_flags() {
        assert!(check_invocation("awman exec workflow <file> --worktree").is_ok());
        assert!(check_invocation("awman exec wf <file>").is_ok());
        assert!(check_invocation("awman clean -y").is_ok());
        assert!(check_invocation("awman nonexistent").is_err());
        assert!(check_invocation("awman squad lst").is_err());
        assert!(check_invocation("awman ready --no-such-flag").is_err());
        assert!(check_invocation("awman clean -z").is_err());
    }
}
