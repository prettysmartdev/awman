//! `WorktreeLifecycleFrontend` impl for the CLI.
//!
//! The CLI prompts on stdin (when it is a TTY) for each decision; when
//! stdin is piped the CLI returns the safe non-interactive defaults.

use std::path::Path;

use crate::command::commands::worktree_lifecycle::{
    ExistingWorktreeDecision, PostWorkflowWorktreeAction, PostWorkflowWorktreePrompt,
    PreWorktreeDecision, WorktreeLifecycleFrontend,
};
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;

use crate::frontend::cli::command_frontend::CliFrontend;
use crate::frontend::cli::output::stdin_is_tty;

fn read_line_or_default(default_letter: char) -> char {
    let mut buf = String::new();
    if std::io::stdin().read_line(&mut buf).is_err() {
        return default_letter;
    }
    buf.trim().chars().next().unwrap_or(default_letter)
}

impl WorktreeLifecycleFrontend for CliFrontend {
    fn ask_pre_worktree_uncommitted_files(
        &mut self,
        files: &[String],
        suggested_message: &str,
    ) -> Result<PreWorktreeDecision, CommandError> {
        if !stdin_is_tty() {
            return Ok(PreWorktreeDecision::UseLastCommit);
        }
        eprintln!(
            "awman: {} uncommitted file(s) in working tree:",
            files.len()
        );
        for f in files.iter().take(10) {
            eprintln!("  {f}");
        }
        if files.len() > 10 {
            eprintln!("  ... and {} more", files.len() - 10);
        }
        eprintln!("  [c]ommit / [u]se last commit / [a]bort");
        match read_line_or_default('u') {
            'c' | 'C' => {
                eprintln!("awman: commit message (default \"{suggested_message}\"):");
                let mut buf = String::new();
                let _ = std::io::stdin().read_line(&mut buf);
                let trimmed = buf.trim();
                let message = if trimmed.is_empty() {
                    suggested_message.to_string()
                } else {
                    trimmed.to_string()
                };
                Ok(PreWorktreeDecision::Commit { message })
            }
            'u' | 'U' => Ok(PreWorktreeDecision::UseLastCommit),
            _ => Ok(PreWorktreeDecision::Abort),
        }
    }

    fn ask_existing_worktree(
        &mut self,
        path: &Path,
        branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError> {
        if !stdin_is_tty() {
            return Ok(ExistingWorktreeDecision::Resume);
        }
        eprintln!(
            "awman: worktree {} already exists for branch {branch}. [r]esume / [R]ecreate?",
            path.display()
        );
        let ch = read_line_or_default('r');
        Ok(if ch == 'R' {
            ExistingWorktreeDecision::Recreate
        } else {
            ExistingWorktreeDecision::Resume
        })
    }

    fn report_worktree_created(&mut self, path: &Path, branch: &str) {
        self.messages
            .write_message(crate::engine::message::UserMessage {
                level: crate::engine::message::MessageLevel::Info,
                text: format!("worktree created at {} on branch {branch}", path.display()),
            });
    }

    fn ask_post_workflow_action(
        &mut self,
        prompt: &PostWorkflowWorktreePrompt,
    ) -> Result<PostWorkflowWorktreeAction, CommandError> {
        if !stdin_is_tty() {
            return Ok(PostWorkflowWorktreeAction::Keep);
        }
        eprintln!("awman: {}", prompt.body.replace('\n', " "));
        eprintln!(
            "  [m] {} / [d] {} / [k] {}",
            prompt.merge_label, prompt.discard_label, prompt.keep_label,
        );
        match read_line_or_default('k') {
            'm' | 'M' => Ok(PostWorkflowWorktreeAction::Merge),
            'd' | 'D' => Ok(PostWorkflowWorktreeAction::Discard),
            _ => Ok(PostWorkflowWorktreeAction::Keep),
        }
    }

    fn ask_worktree_commit_before_merge(
        &mut self,
        branch: &str,
        files: &[String],
        suggested_message: &str,
    ) -> Result<Option<String>, CommandError> {
        if !stdin_is_tty() {
            return Ok(None);
        }
        eprintln!(
            "awman: {} uncommitted file(s) in worktree {branch}:",
            files.len()
        );
        for f in files.iter().take(10) {
            eprintln!("  {f}");
        }
        if files.len() > 10 {
            eprintln!("  ... and {} more", files.len() - 10);
        }
        eprintln!("awman: commit message (default \"{suggested_message}\", empty to skip):");
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(None);
        }
        let trimmed = buf.trim();
        Ok(if trimmed.is_empty() {
            Some(suggested_message.to_string())
        } else {
            Some(trimmed.to_string())
        })
    }

    fn confirm_squash_merge(&mut self, branch: &str) -> Result<bool, CommandError> {
        if !stdin_is_tty() {
            return Ok(false);
        }
        eprintln!("awman: squash-merge {branch} into HEAD? [y/n]");
        let ch = read_line_or_default('n');
        Ok(matches!(ch, 'y' | 'Y'))
    }

    fn confirm_worktree_cleanup(
        &mut self,
        branch: &str,
        path: &Path,
    ) -> Result<bool, CommandError> {
        if !stdin_is_tty() {
            return Ok(false);
        }
        eprintln!(
            "awman: delete worktree {} (branch {branch})? [y/n]",
            path.display()
        );
        let ch = read_line_or_default('n');
        Ok(matches!(ch, 'y' | 'Y'))
    }

    fn report_merge_conflict(&mut self, branch: &str, worktree_path: &Path, git_root: &Path) {
        self.messages
            .write_message(crate::engine::message::UserMessage {
                level: crate::engine::message::MessageLevel::Error,
                text: format!(
                    "merge conflict on {branch} (worktree {}, git root {})",
                    worktree_path.display(),
                    git_root.display()
                ),
            });
    }

    fn report_worktree_discarded(&mut self, branch: &str) {
        self.messages
            .write_message(crate::engine::message::UserMessage {
                level: crate::engine::message::MessageLevel::Info,
                text: format!("worktree for {branch} discarded"),
            });
    }

    fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
        self.messages
            .write_message(crate::engine::message::UserMessage {
                level: crate::engine::message::MessageLevel::Info,
                text: format!("worktree for {branch} kept at {}", path.display()),
            });
    }
}

// ─── Safe-default tests (non-TTY stdin path) ──────────────────────────────
//
// `cargo test` runs with stdin piped (not a TTY), so `stdin_is_tty()` returns
// false, and every method returns the §7u safe default without blocking.
// This is the same behavior a `Cursor`-backed stdin would exercise.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::command::commands::worktree_lifecycle::WorktreeLifecycleFrontend;
    use crate::command::dispatch::catalogue::CommandCatalogue;
    use crate::frontend::cli::command_frontend::CliFrontend;

    fn make_frontend() -> CliFrontend {
        let cmd = CommandCatalogue::get().build_clap_command();
        let m = cmd
            .try_get_matches_from(["awman", "exec", "workflow", "deploy.toml"])
            .unwrap();
        CliFrontend::new(m)
    }

    #[test]
    fn confirm_worktree_cleanup_returns_false_when_not_tty() {
        let mut f = make_frontend();
        let result = f
            .confirm_worktree_cleanup("feature/x", &PathBuf::from("/tmp/wt"))
            .unwrap();
        // §7u safe default: false.
        assert!(!result, "expected false in non-TTY env");
    }
}
