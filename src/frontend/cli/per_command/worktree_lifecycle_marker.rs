//! `WorktreeLifecycleFrontend` impl for the CLI.
//!
//! The CLI prompts on stdin (when it is a TTY) for each decision; when stdin
//! is piped it returns the CLI profile's answers from
//! `src/command/headless.rs`.

use std::path::Path;

use crate::command::commands::worktree_lifecycle::{
    dirty_merge_target_body, dirty_target_commit_message, existing_worktree_prompt_line,
    DirtyMergeTargetDecision, ExistingWorktreeDecision, MergeFailure, PostWorkflowWorktreeAction,
    PostWorkflowWorktreePrompt, PreWorktreeDecision, WorktreeLifecycleFrontend, WorktreeMergeMode,
    DIRTY_COMMIT_PROMPT, DIRTY_COMMIT_TITLE, DIRTY_MERGE_TARGET_KEYS, DIRTY_MERGE_TARGET_TITLE,
    MERGE_FIX_PROMPT_OUTPUT_LINES,
};
use crate::command::error::CommandError;
use crate::data::message::UserMessageSink;

use crate::frontend::cli::command_frontend::CliFrontend;

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
        if self.non_interactive {
            return Ok(self
                .headless
                .pre_worktree_uncommitted_files(suggested_message));
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
        if self.non_interactive {
            return Ok(self.headless.existing_worktree());
        }
        eprintln!("{}", existing_worktree_prompt_line(path, branch));
        let ch = read_line_or_default('r');
        Ok(if ch == 'R' {
            ExistingWorktreeDecision::Recreate
        } else {
            ExistingWorktreeDecision::Resume
        })
    }

    fn existing_worktree_after_resume(
        &mut self,
        _path: &Path,
        _branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError> {
        // A confirmed resume is not asked twice; a piped run answers from
        // its profile like every other headless host.
        if self.non_interactive {
            return Ok(self.headless.existing_worktree());
        }
        Ok(ExistingWorktreeDecision::Resume)
    }

    fn report_worktree_created(&mut self, path: &Path, branch: &str) {
        self.messages
            .write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Info,
                text: format!("worktree created at {} on branch {branch}", path.display()),
            });
    }

    fn ask_post_workflow_action(
        &mut self,
        prompt: &PostWorkflowWorktreePrompt,
    ) -> Result<PostWorkflowWorktreeAction, CommandError> {
        if self.non_interactive {
            return Ok(self.headless.post_workflow_action(prompt));
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
        if self.non_interactive {
            return Ok(self
                .headless
                .worktree_commit_before_merge(suggested_message));
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

    fn ask_merge_mode(&mut self, branch: &str) -> Result<WorktreeMergeMode, CommandError> {
        if self.non_interactive {
            return Ok(self.headless.merge_mode());
        }
        eprintln!("awman: merge {branch} into HEAD?");
        eprintln!("  [m] merge (no squash) / [s] squash / [l] leave branch alone");
        Ok(match read_line_or_default('l') {
            'm' | 'M' => WorktreeMergeMode::Merge,
            's' | 'S' => WorktreeMergeMode::Squash,
            _ => WorktreeMergeMode::LeaveBranch,
        })
    }

    fn ask_dirty_merge_target(
        &mut self,
        target_branch: &str,
        files: &[String],
        suggested_message: &str,
    ) -> Result<DirtyMergeTargetDecision, CommandError> {
        if self.non_interactive {
            return Ok(self.headless.dirty_merge_target());
        }
        eprintln!("awman: {DIRTY_MERGE_TARGET_TITLE}");
        eprintln!("{}", dirty_merge_target_body(target_branch, files));
        for (key, label) in DIRTY_MERGE_TARGET_KEYS {
            eprintln!("  [{key}] {label}");
        }
        match read_line_or_default('a') {
            'c' | 'C' => {
                eprintln!(
                    "awman: {DIRTY_COMMIT_TITLE}: {DIRTY_COMMIT_PROMPT} [{suggested_message}]"
                );
                Ok(read_dirty_commit_message(
                    &mut std::io::stdin().lock(),
                    suggested_message,
                ))
            }
            's' | 'S' => Ok(DirtyMergeTargetDecision::Stash),
            _ => Ok(DirtyMergeTargetDecision::Abort),
        }
    }

    fn ask_launch_merge_fix_agent(&mut self, failure: &MergeFailure) -> Result<bool, CommandError> {
        if self.non_interactive {
            return Ok(self.headless.launch_merge_fix_agent());
        }
        eprintln!("{}", failure.prompt_body(MERGE_FIX_PROMPT_OUTPUT_LINES));
        eprintln!("awman: {}", failure.prompt_title());
        let ch = read_line_or_default('n');
        Ok(matches!(ch, 'y' | 'Y'))
    }

    fn confirm_worktree_cleanup(
        &mut self,
        branch: &str,
        path: &Path,
    ) -> Result<bool, CommandError> {
        if self.non_interactive {
            return Ok(self.headless.confirm_worktree_cleanup());
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
            .write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Error,
                text: format!(
                    "merge conflict on {branch} (worktree {}, git root {})",
                    worktree_path.display(),
                    git_root.display()
                ),
            });
    }

    fn report_worktree_discarded(&mut self, branch: &str) {
        self.messages
            .write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Info,
                text: format!("worktree for {branch} discarded"),
            });
    }

    fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
        self.messages
            .write_message(crate::data::message::UserMessage {
                level: crate::data::message::MessageLevel::Info,
                text: format!("worktree for {branch} kept at {}", path.display()),
            });
    }
}

fn read_dirty_commit_message(
    reader: &mut dyn std::io::BufRead,
    suggested: &str,
) -> DirtyMergeTargetDecision {
    let mut text = String::new();
    match reader.read_line(&mut text) {
        Ok(n) if n > 0 => dirty_target_commit_message(Some(&text), suggested),
        _ => dirty_target_commit_message(None, suggested),
    }
}

#[cfg(test)]
mod dirty_target_tests {
    use super::*;
    #[test]
    fn eof_and_read_error_abort_and_enter_accepts_default() {
        assert_eq!(
            read_dirty_commit_message(&mut std::io::Cursor::new(b""), "WIP"),
            DirtyMergeTargetDecision::Abort
        );
        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("closed"))
            }
        }
        impl std::io::BufRead for Broken {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                Err(std::io::Error::other("closed"))
            }
            fn consume(&mut self, _: usize) {}
        }
        assert_eq!(
            read_dirty_commit_message(&mut Broken, "WIP"),
            DirtyMergeTargetDecision::Abort
        );
        assert_eq!(
            read_dirty_commit_message(&mut std::io::Cursor::new(b"\n"), "WIP"),
            DirtyMergeTargetDecision::Commit {
                message: "WIP".into()
            }
        );
    }
}
