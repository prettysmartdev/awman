//! `WorktreeLifecycleFrontend` impl for the TUI.

use std::path::Path;

use crate::command::commands::worktree_lifecycle::{
    ExistingWorktreeDecision, PostWorkflowWorktreeAction, PostWorkflowWorktreePrompt,
    PreWorktreeDecision, WorktreeLifecycleFrontend,
};
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

fn format_file_list(files: &[String]) -> String {
    let shown: Vec<&str> = files.iter().take(10).map(|s| s.as_str()).collect();
    let mut body = shown.join("\n");
    if files.len() > 10 {
        body.push_str(&format!("\n... and {} more", files.len() - 10));
    }
    body
}

impl WorktreeLifecycleFrontend for TuiCommandFrontend {
    fn ask_pre_worktree_uncommitted_files(
        &mut self,
        files: &[String],
        suggested_message: &str,
    ) -> Result<PreWorktreeDecision, CommandError> {
        let file_list = format_file_list(files);
        let body = format!("{} uncommitted file(s):\n\n{}", files.len(), file_list);
        let response = self.ask_dialog(DialogRequest::Custom {
            title: "Uncommitted files".into(),
            body,
            keys: vec![
                ('c', "Commit first".into()),
                ('u', "Use last commit".into()),
                ('a', "Abort".into()),
            ],
        })?;
        Ok(match response {
            DialogResponse::Char('c') => {
                let msg_response = self.ask_dialog(DialogRequest::TextInput {
                    title: "Commit message".into(),
                    prompt: "Enter commit message (or press Enter to accept):".into(),
                    default_text: Some(suggested_message.to_string()),
                })?;
                match msg_response {
                    DialogResponse::Text(msg) if !msg.is_empty() => {
                        PreWorktreeDecision::Commit { message: msg }
                    }
                    _ => PreWorktreeDecision::Commit {
                        message: suggested_message.to_string(),
                    },
                }
            }
            DialogResponse::Char('u') => PreWorktreeDecision::UseLastCommit,
            _ => PreWorktreeDecision::Abort,
        })
    }

    fn ask_existing_worktree(
        &mut self,
        path: &Path,
        branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError> {
        let response = self.ask_dialog(DialogRequest::Custom {
            title: "Existing worktree".into(),
            body: format!(
                "Worktree exists at {} (branch: {}).\n\nResume or recreate?",
                path.display(),
                branch
            ),
            keys: vec![('r', "Resume".into()), ('n', "Recreate".into())],
        })?;
        let decision = match response {
            DialogResponse::Char('n') => ExistingWorktreeDecision::Recreate,
            _ => ExistingWorktreeDecision::Resume,
        };
        if matches!(decision, ExistingWorktreeDecision::Resume) {
            // The lifecycle returns early on Resume without calling
            // report_worktree_created, so publish the path here so the
            // bottom-bar context line shows "Using worktree" immediately.
            if let Ok(mut guard) = self.active_worktree_path.lock() {
                *guard = Some(path.to_path_buf());
            }
        }
        Ok(decision)
    }

    fn report_worktree_created(&mut self, path: &Path, branch: &str) {
        if let Ok(mut guard) = self.active_worktree_path.lock() {
            *guard = Some(path.to_path_buf());
        }
        self.messages.info(format!(
            "Created worktree at {} on branch {}",
            path.display(),
            branch
        ));
    }

    fn ask_post_workflow_action(
        &mut self,
        prompt: &PostWorkflowWorktreePrompt,
    ) -> Result<PostWorkflowWorktreeAction, CommandError> {
        let response = self.ask_dialog(DialogRequest::Custom {
            title: prompt.title.clone(),
            body: prompt.body.clone(),
            keys: vec![
                ('m', prompt.merge_label.clone()),
                ('d', prompt.discard_label.clone()),
                ('k', prompt.keep_label.clone()),
            ],
        })?;
        Ok(match response {
            DialogResponse::Char('m') => PostWorkflowWorktreeAction::Merge,
            DialogResponse::Char('d') => PostWorkflowWorktreeAction::Discard,
            _ => PostWorkflowWorktreeAction::Keep,
        })
    }

    fn ask_worktree_commit_before_merge(
        &mut self,
        _branch: &str,
        files: &[String],
        suggested_message: &str,
    ) -> Result<Option<String>, CommandError> {
        let file_list = format_file_list(files);
        let body = format!(
            "{} uncommitted file(s) will be committed before merge:\n{}",
            files.len(),
            file_list
        );
        self.messages.info(body);
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Commit before merge?".into(),
            body: "Commit uncommitted files before merging?".into(),
        })?;
        if matches!(response, DialogResponse::Yes | DialogResponse::Char('y')) {
            let msg_response = self.ask_dialog(DialogRequest::TextInput {
                title: "Commit message".into(),
                prompt: "Enter commit message (or press Enter to accept):".into(),
                default_text: Some(suggested_message.to_string()),
            })?;
            match msg_response {
                DialogResponse::Text(msg) if !msg.is_empty() => Ok(Some(msg)),
                _ => Ok(Some(suggested_message.to_string())),
            }
        } else {
            Ok(None)
        }
    }

    fn confirm_squash_merge(&mut self, branch: &str) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Squash merge?".into(),
            body: format!("Squash-merge branch '{branch}' into main branch?"),
        })?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn confirm_worktree_cleanup(
        &mut self,
        branch: &str,
        path: &Path,
    ) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Clean up worktree?".into(),
            body: format!(
                "Delete worktree at {} and branch '{branch}'?",
                path.display()
            ),
        })?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn report_merge_conflict(&mut self, branch: &str, worktree_path: &Path, _git_root: &Path) {
        self.messages.error_msg(format!(
            "Merge conflict on branch '{}'. Resolve manually in {}",
            branch,
            worktree_path.display()
        ));
    }

    fn report_worktree_discarded(&mut self, branch: &str) {
        if let Ok(mut guard) = self.active_worktree_path.lock() {
            *guard = None;
        }
        self.messages
            .info(format!("Worktree for branch '{branch}' discarded"));
    }

    fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
        if let Ok(mut guard) = self.active_worktree_path.lock() {
            *guard = None;
        }
        self.messages.info(format!(
            "Worktree kept at {} (branch: {branch})",
            path.display()
        ));
    }
}
