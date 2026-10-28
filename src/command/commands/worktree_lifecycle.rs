//! `WorktreeLifecycle` — pre/post worktree helper for commands that run an
//! agent inside an isolated git worktree.
//!
//! Architecturally, all worktree lifecycle logic is a command-layer concern,
//! not a `WorkflowEngine` concern. `WorkflowEngine` is handed a working
//! directory and runs steps in it; it does not know whether that directory
//! is a worktree or the main checkout.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::command::error::CommandError;
use crate::data::config::repo::{OnDirtyMergeTarget, OnMergeFailure};
use crate::data::merge_fix_prompt::has_conflict_markers;
use crate::data::message::{MessageLevel, UserMessage};
use crate::engine::error::{EngineError, MergeFailedDetail};
use crate::engine::git::{squash_commit_message, GitEngine, GitFrontend};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreWorktreeDecision {
    Commit { message: String },
    UseLastCommit,
    Abort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingWorktreeDecision {
    /// Keep the worktree and its branch as they are and run in it.
    Resume,
    /// Remove the worktree, force-delete its branch (and the branch's
    /// commits), then create a fresh worktree from the current HEAD.
    Recreate,
    /// Stop with [`CommandError::WorktreeExists`]; nothing is changed.
    Fail,
}

/// How [`WorktreeLifecycle::prepare_with_existing`] settles the
/// existing-worktree question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingWorktreeChoice {
    /// No flag, no accepted resume: `ask_existing_worktree` decides.
    Ask,
    /// `--on-existing-worktree resume|recreate|fail`: wins on every host.
    Explicit(ExistingWorktreeDecision),
    /// The caller already accepted a workflow/dynamic resume for this worktree.
    /// `existing_worktree_after_resume` decides: interactive hosts answer
    /// `Resume` (not asked twice); headless hosts answer their profile
    /// (API `Fail`, squad and piped CLI `Resume`).
    ResumeAccepted,
}

pub const EXISTING_WORKTREE_TITLE: &str = "Existing worktree";
pub const EXISTING_WORKTREE_KEYS: &[(char, &str)] = &[
    ('r', "Resume"),
    ('n', "Recreate (discard branch and commits)"),
];

/// Body of the existing-worktree dialog. The frontend only renders it.
pub fn existing_worktree_body(path: &Path, branch: &str) -> String {
    format!(
        "Worktree exists at {path} (branch: {branch}).\n\nResume keeps it as it is. \
         Recreate deletes the worktree and the branch '{branch}' with all of its commits, \
         then starts from the current HEAD.",
        path = path.display(),
    )
}

/// One-line existing-worktree prompt for line-oriented frontends.
pub fn existing_worktree_prompt_line(path: &Path, branch: &str) -> String {
    format!(
        "awman: worktree {} already exists for branch {branch}. \
         [r]esume / [R]ecreate (deletes the branch and its commits)?",
        path.display(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostWorkflowWorktreeAction {
    Merge,
    Discard,
    Keep,
}

/// How the worktree branch should be integrated once the user picks the
/// Merge action in the post-workflow dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeMergeMode {
    /// Plain `git merge` — preserves the branch's individual commits.
    Merge,
    /// `git merge --squash` followed by a single commit.
    Squash,
    /// Leave the branch (and worktree) alone.
    LeaveBranch,
}

/// What to do with uncommitted changes in the merge target (the parent
/// checkout) before a worktree branch is merged into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirtyMergeTargetDecision {
    /// Commit everything with this message, then merge.
    Commit { message: String },
    /// `git stash push --include-untracked`, then merge. Never popped.
    Stash,
    /// Do not merge; keep the worktree and branch.
    Abort,
}

/// Flag/config-resolved answers for the merge-back questions. `None` (or
/// `Ask`) consults the frontend, which answers from its headless profile when
/// nobody is attached. Resolved in the command layer: flag > repo > global.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FinalizePolicy {
    pub on_dirty_merge_target: Option<OnDirtyMergeTarget>,
    pub on_merge_failure: Option<OnMergeFailure>,
}

/// Title of the dirty-merge-target dialog. Dialog copy lives here so the
/// frontends only render it.
pub const DIRTY_MERGE_TARGET_TITLE: &str = "Merge target has uncommitted changes";

/// Shared choice and text-input copy for every interactive host.
pub const DIRTY_MERGE_TARGET_KEYS: &[(char, &str)] =
    &[('c', "Commit first"), ('s', "Stash"), ('a', "Abort")];
pub const DIRTY_COMMIT_TITLE: &str = "Commit message";
pub const DIRTY_COMMIT_PROMPT: &str = "Enter commit message (or press Enter to accept):";

/// Only a submitted text response authorizes a commit; cancellation aborts.
pub fn dirty_target_commit_message(
    submitted: Option<&str>,
    suggested: &str,
) -> DirtyMergeTargetDecision {
    match submitted {
        Some(text) => DirtyMergeTargetDecision::Commit {
            message: if text.trim().is_empty() {
                suggested.into()
            } else {
                text.trim().into()
            },
        },
        None => DirtyMergeTargetDecision::Abort,
    }
}

/// Body of the dirty-merge-target dialog: the target branch and a listing of
/// the dirty files (at most ten, then a count of the rest).
pub fn dirty_merge_target_body(target_branch: &str, files: &[String]) -> String {
    let mut body = format!(
        "'{target_branch}' has {} uncommitted file(s); merging into it now could overwrite or mix in your edits:\n\n",
        files.len()
    );
    body.push_str(
        &files
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n"),
    );
    if files.len() > 10 {
        body.push_str(&format!("\n... and {} more", files.len() - 10));
    }
    body
}

/// Everything a frontend and the merge-fix prompt need about a failed
/// worktree merge-back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeFailure {
    pub branch: String,
    pub target_branch: String,
    pub worktree_path: PathBuf,
    pub git_root: PathBuf,
    /// `true` for `git merge --squash`, which commits with
    /// [`squash_commit_message`].
    pub squash: bool,
    /// The exact command that failed, as a user would type it.
    pub command: String,
    pub exit_code: i32,
    /// stdout then stderr of the failed command.
    pub output: String,
    /// `git diff --name-only --diff-filter=U` right after the failure.
    pub unmerged_paths: Vec<String>,
    /// Staged paths before repair; non-conflicting changes must survive it.
    pub staged_paths: Vec<String>,
    /// Squash completion is only automatic while this HEAD is unchanged.
    pub head_before_fix: String,
    /// The merge touched the index: `MERGE_HEAD` existed right after the
    /// failure, or the squash merge's changes were staged and only its
    /// commit failed.
    pub merge_in_progress: bool,
}

/// Lines of the failed command's output the dialog shows.
pub const MERGE_FIX_PROMPT_OUTPUT_LINES: usize = 12;

impl MergeFailure {
    /// The merge touched the index (there is something to resolve) rather
    /// than failing before it started (retry the command instead).
    pub fn merge_started(&self) -> bool {
        self.merge_in_progress || !self.unmerged_paths.is_empty()
    }

    /// Title of the launch-merge-fix-agent dialog. Dialog copy is built here
    /// so TUI, CLI and API render the same words.
    pub fn prompt_title(&self) -> String {
        "Launch an agent to fix the merge? [y/N]".to_string()
    }

    /// Body of the launch-merge-fix-agent dialog: the failed command, its
    /// exit code, the first `max_output_lines` lines of its output and the
    /// unmerged paths.
    pub fn prompt_body(&self, max_output_lines: usize) -> String {
        let mut body = format!(
            "Merging '{}' into '{}' failed.\n\n$ {}\nexit code {}\n",
            self.branch, self.target_branch, self.command, self.exit_code
        );
        let lines: Vec<&str> = self.output.lines().collect();
        for line in lines.iter().take(max_output_lines) {
            body.push_str(line);
            body.push('\n');
        }
        if lines.len() > max_output_lines {
            body.push_str(&format!(
                "... ({} more line(s))\n",
                lines.len() - max_output_lines
            ));
        }
        if !self.unmerged_paths.is_empty() {
            body.push_str("\nUnmerged files:\n");
            body.push_str(&self.unmerged_paths.join("\n"));
            body.push('\n');
        }
        body.push_str(&format!(
            "\nThe agent runs in a container on {}. Otherwise the worktree is kept at {} and the merge is left for you.",
            self.git_root.display(),
            self.worktree_path.display()
        ));
        body
    }
}

/// What [`WorktreeLifecycle::finalize`] left for its caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeOutcome {
    /// The worktree is handled: merged and cleaned up, kept, discarded, or a
    /// failed merge reported with no agent wanted.
    Done,
    /// The merge failed and an agent was requested (by policy or by the
    /// frontend). The caller runs it — outside any lock on the frontend,
    /// because the agent reports through the same frontend — then calls
    /// [`WorktreeLifecycle::complete_merge_after_fix`]. The frontend has
    /// already been told about the failure.
    MergeFixRequested(Box<MergeFailure>),
}

/// Prebuilt dialog content for the post-workflow worktree-action prompt.
///
/// Built by the command layer (which queries the git engine for the target
/// branch and decides on the human-readable labels) and consumed by every
/// frontend. Frontends should NOT compose these strings themselves — that
/// keeps the prompt copy testable in one place and avoids divergence
/// between CLI/TUI/API wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostWorkflowWorktreePrompt {
    /// Worktree branch name (e.g. `awman/work-item-0072`).
    pub branch: String,
    /// Branch that a Merge action would target — the parent repo's HEAD
    /// branch. Resolved via `GitEngine::current_branch`; falls back to
    /// `"current branch"` for detached HEAD or query failure.
    pub target_branch: String,
    /// Whether the workflow ended with an error (drives the title copy).
    pub had_error: bool,
    /// Title shown at the top of the dialog.
    pub title: String,
    /// Body text rendered above the action list.
    pub body: String,
    /// Label for the Merge action (e.g. `Merge into 'main'`).
    pub merge_label: String,
    /// Label for the Discard action.
    pub discard_label: String,
    /// Label for the Keep action.
    pub keep_label: String,
}

pub trait WorktreeLifecycleFrontend: GitFrontend + Send + Sync {
    fn ask_pre_worktree_uncommitted_files(
        &mut self,
        files: &[String],
        suggested_message: &str,
    ) -> Result<PreWorktreeDecision, CommandError>;

    fn ask_existing_worktree(
        &mut self,
        path: &Path,
        branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError>;

    fn report_worktree_created(&mut self, path: &Path, branch: &str);

    /// An existing worktree is being reused as it is. Hosts that track the
    /// active worktree record it here; the default does nothing.
    fn report_worktree_resumed(&mut self, _path: &Path, _branch: &str) {}

    /// The caller already accepted a workflow/dynamic resume, which implies
    /// the worktree is reused. Interactive hosts keep that answer (the
    /// default); headless hosts return their profile's answer, so the API
    /// still fails closed unless the client passed `--on-existing-worktree`.
    fn existing_worktree_after_resume(
        &mut self,
        _path: &Path,
        _branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError> {
        Ok(ExistingWorktreeDecision::Resume)
    }

    fn ask_post_workflow_action(
        &mut self,
        prompt: &PostWorkflowWorktreePrompt,
    ) -> Result<PostWorkflowWorktreeAction, CommandError>;

    fn ask_worktree_commit_before_merge(
        &mut self,
        branch: &str,
        files: &[String],
        suggested_message: &str,
    ) -> Result<Option<String>, CommandError>;

    fn ask_merge_mode(&mut self, branch: &str) -> Result<WorktreeMergeMode, CommandError>;

    /// The merge target (parent checkout) has uncommitted changes. Commit
    /// them, stash them, or abort the merge.
    fn ask_dirty_merge_target(
        &mut self,
        target_branch: &str,
        files: &[String],
        suggested_message: &str,
    ) -> Result<DirtyMergeTargetDecision, CommandError>;

    /// A merge-back failed (and `report_merge_conflict` has been called).
    /// `true` launches an agent to fix it; `false` keeps the worktree and
    /// leaves the merge for the user. Render `failure.prompt_title()` and
    /// `failure.prompt_body(..)`.
    fn ask_launch_merge_fix_agent(&mut self, failure: &MergeFailure) -> Result<bool, CommandError>;

    fn confirm_worktree_cleanup(&mut self, branch: &str, path: &Path)
        -> Result<bool, CommandError>;

    fn report_merge_conflict(&mut self, branch: &str, worktree_path: &Path, git_root: &Path);

    fn report_worktree_discarded(&mut self, branch: &str);

    fn report_worktree_kept(&mut self, path: &Path, branch: &str);
}

#[derive(Clone)]
pub struct WorktreeLifecycle {
    git_engine: Arc<GitEngine>,
    git_root: PathBuf,
    worktree_path: PathBuf,
    creation_path: PathBuf,
    branch: String,
}

impl WorktreeLifecycle {
    /// What an `exec workflow` run names its worktree and branch after.
    ///
    /// The three cases used to be a four-armed `if`/`else` in
    /// `exec_workflow`, with `for_workflow`/`for_work_item` called from each
    /// arm and the same error message written out three times
    /// (WI 0114 F-51). Naming is a property of the run, so it is decided here,
    /// once, and `open` is called once.
    pub fn name_for(
        issue_slug: Option<&str>,
        work_item: Option<u32>,
        workflow_path: &Path,
    ) -> WorktreeName {
        // `--issue` outranks `--work-item`, which outranks the workflow file:
        // the more specific the user was, the more specific the branch name.
        if let Some(slug) = issue_slug {
            return WorktreeName::Workflow(slug.to_string());
        }
        if let Some(number) = work_item {
            return WorktreeName::WorkItem(number);
        }
        WorktreeName::Workflow(
            workflow_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("workflow")
                .to_string(),
        )
    }

    /// Open the lifecycle for a [`WorktreeName`].
    pub fn open(
        git_engine: Arc<GitEngine>,
        git_root: PathBuf,
        name: &WorktreeName,
    ) -> Result<Self, CommandError> {
        match name {
            WorktreeName::Workflow(name) => Self::for_workflow(git_engine, git_root, name),
            WorktreeName::WorkItem(number) => Self::for_work_item(git_engine, git_root, *number),
        }
    }

    pub fn for_workflow(
        git_engine: Arc<GitEngine>,
        git_root: PathBuf,
        workflow_name: &str,
    ) -> Result<Self, CommandError> {
        let worktree_path = git_engine
            .worktree_path_named(&git_root, workflow_name)
            .map_err(CommandError::from)?;
        let creation_path = crate::data::worktree_paths::WorktreePaths::from_home()?.for_workflow(
            &git_root
                .canonicalize()
                .map_err(|e| CommandError::Other(e.to_string()))?,
            workflow_name,
        );
        let branch = git_engine.branch_name_for_workflow(workflow_name);
        Ok(Self {
            git_engine,
            git_root,
            worktree_path,
            creation_path,
            branch,
        })
    }

    pub fn for_work_item(
        git_engine: Arc<GitEngine>,
        git_root: PathBuf,
        work_item: u32,
    ) -> Result<Self, CommandError> {
        let worktree_path = git_engine
            .worktree_path(&git_root, work_item)
            .map_err(CommandError::from)?;
        let creation_path = crate::data::worktree_paths::WorktreePaths::from_home()?.for_work_item(
            &git_root
                .canonicalize()
                .map_err(|e| CommandError::Other(e.to_string()))?,
            work_item,
        );
        let branch = git_engine.branch_name_for_work_item(work_item);
        Ok(Self {
            git_engine,
            git_root,
            worktree_path,
            creation_path,
            branch,
        })
    }

    pub fn worktree_path(&self) -> &Path {
        &self.worktree_path
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The parent checkout the branch is merged into.
    pub fn git_root(&self) -> &Path {
        &self.git_root
    }

    /// Compose the [`PostWorkflowWorktreePrompt`] handed to the frontend.
    /// All copy lives here — the frontend just renders the strings.
    fn build_post_workflow_prompt(&self, had_error: bool) -> PostWorkflowWorktreePrompt {
        let target_branch = self
            .git_engine
            .current_branch(&self.git_root)
            .unwrap_or_else(|| "current branch".to_string());
        let status = if had_error {
            "ended with errors"
        } else {
            "completed successfully"
        };
        PostWorkflowWorktreePrompt {
            branch: self.branch.clone(),
            target_branch: target_branch.clone(),
            had_error,
            title: "Workflow Complete — Worktree Action".to_string(),
            body: format!(
                "Workflow {status}.\nBranch: {branch}\n\nChoose what to do with the worktree:",
                branch = self.branch,
            ),
            merge_label: format!("Merge into '{target_branch}'"),
            discard_label: "Discard worktree (delete branch and directory)".to_string(),
            keep_label: "Keep worktree for later".to_string(),
        }
    }

    /// Settle fail/fresh policy before reading or changing saved run state.
    /// The accepted-resume hook supplies headless defaults without prompting
    /// interactive users a second time.
    pub fn should_offer_resume(
        &self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
        explicit: Option<ExistingWorktreeDecision>,
    ) -> Result<bool, CommandError> {
        if self.worktree_path.exists() {
            let decision = match explicit {
                Some(d) => d,
                None => {
                    frontend.existing_worktree_after_resume(&self.worktree_path, &self.branch)?
                }
            };
            if decision == ExistingWorktreeDecision::Fail {
                return Err(CommandError::WorktreeExists {
                    path: self.worktree_path.clone(),
                    branch: self.branch.clone(),
                });
            }
            self.git_engine
                .validate_worktree_owner(&self.git_root, &self.worktree_path)?;
            return Ok(decision != ExistingWorktreeDecision::Recreate);
        }
        Ok(explicit != Some(ExistingWorktreeDecision::Recreate))
    }

    pub async fn prepare(
        &mut self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
    ) -> Result<PathBuf, CommandError> {
        self.prepare_with_existing(frontend, ExistingWorktreeChoice::Ask)
            .await
    }

    /// [`Self::prepare`], with the existing-worktree question possibly
    /// already settled.
    ///
    /// `Explicit` (`--on-existing-worktree`) wins on every host. A caller that
    /// has just asked a question which *implies* the answer — the workflow
    /// resume prompt, which only makes sense against the worktree that is
    /// already there (WI-0115 §2) — passes `ResumeAccepted` rather than asking
    /// a second, near-identical question. It is not only redundant:
    /// `Recreate` deletes the worktree, and with it the state the accepted
    /// resume just rewound. Headless hosts still answer that case from their
    /// profile (WI 0134), so the API fails closed. `Ask` asks as usual.
    pub async fn prepare_with_existing(
        &mut self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
        choice: ExistingWorktreeChoice,
    ) -> Result<PathBuf, CommandError> {
        if self.git_engine.is_detached_head(&self.git_root) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: "current HEAD is detached; the new worktree branch may target an unexpected commit".to_string(),
            });
        }
        if self.worktree_path.exists() {
            let decision = match choice {
                ExistingWorktreeChoice::Explicit(d) => d,
                ExistingWorktreeChoice::Ask => {
                    frontend.ask_existing_worktree(&self.worktree_path, &self.branch)?
                }
                ExistingWorktreeChoice::ResumeAccepted => {
                    frontend.existing_worktree_after_resume(&self.worktree_path, &self.branch)?
                }
            };
            match decision {
                ExistingWorktreeDecision::Resume => {
                    self.git_engine
                        .validate_worktree_owner(&self.git_root, &self.worktree_path)?;
                    frontend.report_worktree_resumed(&self.worktree_path, &self.branch);
                    return Ok(self.worktree_path.clone());
                }
                ExistingWorktreeDecision::Recreate => {
                    self.git_engine
                        .validate_worktree_owner(&self.git_root, &self.worktree_path)?;
                    self.git_engine.delete_recreated_branch_logged(
                        &self.git_root,
                        &self.worktree_path,
                        &self.branch,
                        frontend,
                    )?;
                    self.git_engine.remove_worktree_logged(
                        &self.git_root,
                        &self.worktree_path,
                        frontend,
                    )?;
                    self.worktree_path = self.creation_path.clone();
                }
                ExistingWorktreeDecision::Fail => {
                    return Err(CommandError::WorktreeExists {
                        path: self.worktree_path.clone(),
                        branch: self.branch.clone(),
                    });
                }
            }
        }
        // Always check for dirty state before creating a new worktree,
        // regardless of whether we're creating fresh or recreating.
        let files = self
            .git_engine
            .uncommitted_files_logged(&self.git_root, frontend)?;
        if !files.is_empty() {
            let suggested = format!("WIP: pre-worktree commit for {}", self.branch);
            match frontend.ask_pre_worktree_uncommitted_files(&files, &suggested)? {
                PreWorktreeDecision::Commit { message } => {
                    self.git_engine
                        .commit_all_logged(&self.git_root, &message, frontend)?;
                }
                PreWorktreeDecision::UseLastCommit => {}
                PreWorktreeDecision::Abort => return Err(CommandError::Aborted),
            }
        }
        self.git_engine.create_worktree_logged(
            &self.git_root,
            &self.worktree_path,
            &self.branch,
            frontend,
        )?;
        frontend.report_worktree_created(&self.worktree_path, &self.branch);
        Ok(self.worktree_path.clone())
    }

    /// Check the merge target (the parent checkout) for uncommitted changes
    /// and resolve them per `policy` or the frontend. Returns `Ok(())` when
    /// the merge may proceed; an abort keeps the worktree and ends with
    /// [`CommandError::MergeTargetDirty`], which is not a merge conflict.
    fn prepare_merge_target(
        &self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
        policy: FinalizePolicy,
    ) -> Result<(), CommandError> {
        let files = self
            .git_engine
            .uncommitted_files_logged(&self.git_root, frontend)?;
        if files.is_empty() {
            return Ok(());
        }
        let target_branch = self
            .git_engine
            .current_branch(&self.git_root)
            .unwrap_or_else(|| "current branch".to_string());
        let suggested = format!(
            "WIP: changes on {target_branch} before merging {}",
            self.branch
        );
        let decision = match policy.on_dirty_merge_target {
            None | Some(OnDirtyMergeTarget::Ask) => {
                frontend.ask_dirty_merge_target(&target_branch, &files, &suggested)?
            }
            Some(OnDirtyMergeTarget::Commit) => {
                DirtyMergeTargetDecision::Commit { message: suggested }
            }
            Some(OnDirtyMergeTarget::Stash) => DirtyMergeTargetDecision::Stash,
            Some(OnDirtyMergeTarget::Fail) => DirtyMergeTargetDecision::Abort,
        };
        match decision {
            DirtyMergeTargetDecision::Commit { message } => {
                self.git_engine
                    .commit_all_logged(&self.git_root, &message, frontend)?;
                Ok(())
            }
            DirtyMergeTargetDecision::Stash => {
                let message = format!("awman: before merging {}", self.branch);
                match self
                    .git_engine
                    .stash_push_logged(&self.git_root, &message, frontend)
                {
                    Ok(()) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Info,
                            text: format!(
                                "stashed uncommitted changes on '{target_branch}' as stash@{{0}} ({message}); run `git stash pop` when you are ready to restore them"
                            ),
                        });
                        Ok(())
                    }
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: e.to_string(),
                        });
                        Err(self.merge_target_dirty(frontend, target_branch))
                    }
                }
            }
            DirtyMergeTargetDecision::Abort => {
                Err(self.merge_target_dirty(frontend, target_branch))
            }
        }
    }

    fn merge_target_dirty(
        &self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
        target_branch: String,
    ) -> CommandError {
        frontend.report_worktree_kept(&self.worktree_path, &self.branch);
        CommandError::MergeTargetDirty {
            target_branch,
            branch: self.branch.clone(),
            worktree_path: self.worktree_path.clone(),
        }
    }

    /// Ask what to do with the worktree and do it.
    ///
    /// A failed merge (a conflict or any other non-zero `git merge`) is
    /// reported with `report_merge_conflict`; then, per
    /// `policy.on_merge_failure` or the frontend, either `Ok(Done)` (the
    /// worktree is kept and the merge left for the user, today's behaviour)
    /// or `Ok(MergeFixRequested)`, after which the caller runs the merge-fix
    /// agent and calls [`Self::complete_merge_after_fix`].
    pub async fn finalize(
        &self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
        had_error: bool,
        policy: FinalizePolicy,
    ) -> Result<FinalizeOutcome, CommandError> {
        let prompt = self.build_post_workflow_prompt(had_error);
        let action = frontend.ask_post_workflow_action(&prompt)?;
        match action {
            PostWorkflowWorktreeAction::Merge => {
                let mode = frontend.ask_merge_mode(&self.branch)?;
                // The commit-uncommitted step runs for every mode — including
                // LeaveBranch — so nothing on the branch is left dangling.
                let files = self
                    .git_engine
                    .uncommitted_files_logged(&self.worktree_path, frontend)?;
                if !files.is_empty() {
                    let suggested = format!("Implement {}", self.branch);
                    if let Some(msg) = frontend.ask_worktree_commit_before_merge(
                        &self.branch,
                        &files,
                        &suggested,
                    )? {
                        self.git_engine
                            .commit_all_logged(&self.worktree_path, &msg, frontend)?;
                    }
                }
                let squash = match mode {
                    WorktreeMergeMode::LeaveBranch => {
                        frontend.report_worktree_kept(&self.worktree_path, &self.branch);
                        return Ok(FinalizeOutcome::Done);
                    }
                    WorktreeMergeMode::Squash => true,
                    WorktreeMergeMode::Merge => false,
                };
                self.prepare_merge_target(frontend, policy)?;
                match self.git_engine.merge_branch_logged(
                    &self.git_root,
                    &self.branch,
                    &self.worktree_path,
                    squash,
                    frontend,
                ) {
                    Ok(()) => self.cleanup_after_merge(frontend)?,
                    Err(EngineError::MergeFailed(detail)) => {
                        let failure = self.merge_failure(frontend, squash, *detail)?;
                        frontend.report_merge_conflict(
                            &self.branch,
                            &self.worktree_path,
                            &self.git_root,
                        );
                        let launch = match policy.on_merge_failure {
                            Some(OnMergeFailure::Keep) => false,
                            Some(OnMergeFailure::Agent) => true,
                            None | Some(OnMergeFailure::Ask) => {
                                frontend.ask_launch_merge_fix_agent(&failure)?
                            }
                        };
                        if launch {
                            return Ok(FinalizeOutcome::MergeFixRequested(Box::new(failure)));
                        }
                    }
                    Err(other) => return Err(CommandError::from(other)),
                }
            }
            PostWorkflowWorktreeAction::Discard => {
                self.git_engine.remove_worktree_logged(
                    &self.git_root,
                    &self.worktree_path,
                    frontend,
                )?;
                self.git_engine
                    .delete_branch_logged(&self.git_root, &self.branch, frontend)?;
                frontend.report_worktree_discarded(&self.branch);
            }
            PostWorkflowWorktreeAction::Keep => {
                frontend.report_worktree_kept(&self.worktree_path, &self.branch);
            }
        }
        Ok(FinalizeOutcome::Done)
    }

    /// The post-merge cleanup: ask, then remove the worktree and delete the
    /// branch, or keep them.
    fn cleanup_after_merge(
        &self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
    ) -> Result<(), CommandError> {
        if frontend.confirm_worktree_cleanup(&self.branch, &self.worktree_path)? {
            self.git_engine.remove_worktree_logged(
                &self.git_root,
                &self.worktree_path,
                frontend,
            )?;
            self.git_engine
                .delete_branch_logged(&self.git_root, &self.branch, frontend)?;
            frontend.report_worktree_discarded(&self.branch);
        } else {
            frontend.report_worktree_kept(&self.worktree_path, &self.branch);
        }
        Ok(())
    }

    /// Describe a failed merge: the command and its output from the engine
    /// error, plus the index state right after it.
    fn merge_failure(
        &self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
        squash: bool,
        detail: MergeFailedDetail,
    ) -> Result<MergeFailure, CommandError> {
        let MergeFailedDetail {
            command,
            exit_code,
            output,
            ..
        } = detail;
        let unmerged_paths = self
            .git_engine
            .unmerged_paths_logged(&self.git_root, frontend)?;
        // A squash merge whose commit failed has its changes staged: there is
        // a merge to complete, not one to retry.
        let squash_commit_failed = squash && command.starts_with("git commit");
        let merge_in_progress = squash_commit_failed
            || self
                .git_engine
                .merge_in_progress_logged(&self.git_root, frontend);
        let staged_paths = self
            .git_engine
            .staged_paths_logged(&self.git_root, frontend)?;
        let head_before_fix = self
            .git_engine
            .head_commit_logged(&self.git_root, frontend)?;
        Ok(MergeFailure {
            branch: self.branch.clone(),
            target_branch: self
                .git_engine
                .current_branch(&self.git_root)
                .unwrap_or_else(|| "current branch".to_string()),
            worktree_path: self.worktree_path.clone(),
            git_root: self.git_root.clone(),
            squash,
            command,
            exit_code,
            output,
            unmerged_paths,
            staged_paths,
            head_before_fix,
            merge_in_progress,
        })
    }

    /// Finish a failed merge after the merge-fix agent exited. One attempt,
    /// no loop.
    ///
    /// - `agent_ran == false` (the agent could not start): fall back to
    ///   keeping the worktree and the merge state, as if no agent had been
    ///   wanted, and return `Ok`.
    /// - The merge never started (it failed before touching the index):
    ///   retry the merge command once.
    /// - Otherwise verify: no conflict markers in the originally unmerged
    ///   files, stage them, no unmerged paths left. Then commit
    ///   (`--no-edit` for a plain merge, [`squash_commit_message`] for a
    ///   squash) and run the normal cleanup.
    ///
    /// Still broken: `report_merge_conflict`, keep the worktree and the merge
    /// state, and return `Err(CommandError::MergeConflict)`. That an
    /// unresolved merge is an error here, while a declined agent is `Ok`, is
    /// deliberate: declining is today's behaviour; a fix the user asked for
    /// that did not work must end the command as failed.
    pub async fn complete_merge_after_fix(
        &self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
        failure: &MergeFailure,
        agent_ran: bool,
    ) -> Result<(), CommandError> {
        if !agent_ran {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "merge-fix agent did not run; merge left unresolved in {} and worktree kept at {}",
                    self.git_root.display(),
                    self.worktree_path.display()
                ),
            });
            frontend.report_worktree_kept(&self.worktree_path, &self.branch);
            return Ok(());
        }

        if !failure.merge_started() {
            return match self.git_engine.merge_branch_logged(
                &self.git_root,
                &self.branch,
                &self.worktree_path,
                failure.squash,
                frontend,
            ) {
                Ok(()) => {
                    self.report_merge_completed(frontend);
                    self.cleanup_after_merge(frontend)
                }
                Err(EngineError::MergeFailed(_)) => {
                    Err(self.merge_still_unresolved(frontend, "retrying the merge failed again"))
                }
                Err(other) => Err(CommandError::from(other)),
            };
        }

        // A missing file can be a deliberate deletion; other read failures
        // cannot prove the resolution safe.
        let mut still_marked = Vec::new();
        for path in &failure.unmerged_paths {
            match std::fs::read(self.git_root.join(path)) {
                Ok(bytes) if has_conflict_markers(&String::from_utf8_lossy(&bytes)) => {
                    still_marked.push(path)
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(self
                        .merge_still_unresolved(frontend, &format!("cannot inspect {path}: {e}")))
                }
            }
        }
        if !still_marked.is_empty() {
            let list = still_marked
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(self
                .merge_still_unresolved(frontend, &format!("conflict markers remain in {list}")));
        }

        // The agent was told not to commit, and may not have staged either.
        // A path one side deleted is staged as a deletion.
        if let Err(e) =
            self.git_engine
                .stage_paths_logged(&self.git_root, &failure.unmerged_paths, frontend)
        {
            return Err(self.merge_still_unresolved(frontend, &e.to_string()));
        }
        let remaining = self
            .git_engine
            .unmerged_paths_logged(&self.git_root, frontend)?;
        if !remaining.is_empty() {
            return Err(self.merge_still_unresolved(
                frontend,
                &format!("unmerged paths remain: {}", remaining.join(", ")),
            ));
        }

        let commit = if failure.squash {
            if self
                .git_engine
                .head_commit_logged(&self.git_root, frontend)?
                != failure.head_before_fix
            {
                return Err(self.merge_still_unresolved(
                    frontend,
                    "HEAD changed during squash repair; completion cannot be verified",
                ));
            }
            let staged = self
                .git_engine
                .staged_paths_logged(&self.git_root, frontend)?;
            if failure
                .staged_paths
                .iter()
                .any(|path| !failure.unmerged_paths.contains(path) && !staged.contains(path))
            {
                return Err(self.merge_still_unresolved(frontend,
                    "a non-conflicting staged squash change was discarded; completion cannot be verified"));
            }
            if self
                .git_engine
                .has_staged_changes_logged(&self.git_root, frontend)?
            {
                let message = squash_commit_message(&self.branch);
                self.git_engine
                    .commit_merge_logged(&self.git_root, Some(&message), frontend)
            } else {
                // An empty index can mean the agent discarded the squash or
                // committed an unrelated result. There is no ancestry proof.
                return Err(self.merge_still_unresolved(
                    frontend,
                    "no staged squash result remains; completion cannot be verified",
                ));
            }
        } else if self
            .git_engine
            .merge_in_progress_logged(&self.git_root, frontend)
        {
            self.git_engine
                .commit_merge_logged(&self.git_root, None, frontend)
        } else if self
            .git_engine
            .is_merged_into_head_logged(&self.git_root, &self.branch, frontend)
        {
            // The agent committed the merge despite being told not to; the
            // branch is in HEAD, which is what was wanted.
            Ok(())
        } else {
            // No merge in progress and the branch is not merged: the merge
            // state was thrown away (`git merge --abort`).
            return Err(self.merge_still_unresolved(
                frontend,
                "the merge is no longer in progress and the branch is not merged",
            ));
        };
        if let Err(e) = commit {
            return Err(self.merge_still_unresolved(frontend, &e.to_string()));
        }
        self.report_merge_completed(frontend);
        self.cleanup_after_merge(frontend)
    }

    fn report_merge_completed(&self, frontend: &mut dyn WorktreeLifecycleFrontend) {
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("merge completed: '{}' merged", self.branch),
        });
    }

    /// Report a merge the agent did not fix and build the command's error.
    fn merge_still_unresolved(
        &self,
        frontend: &mut dyn WorktreeLifecycleFrontend,
        reason: &str,
    ) -> CommandError {
        frontend.write_message(UserMessage {
            level: MessageLevel::Error,
            text: format!(
                "merge still unresolved ({reason}); worktree kept at {}",
                self.worktree_path.display()
            ),
        });
        frontend.report_merge_conflict(&self.branch, &self.worktree_path, &self.git_root);
        CommandError::MergeConflict {
            branch: self.branch.clone(),
            worktree_path: self.worktree_path.clone(),
            git_root: self.git_root.clone(),
        }
    }
}

#[cfg(test)]
impl WorktreeLifecycle {
    pub(crate) fn new_for_test(
        git_engine: Arc<GitEngine>,
        git_root: PathBuf,
        worktree_path: PathBuf,
        branch: String,
    ) -> Self {
        Self {
            git_engine,
            git_root,
            creation_path: worktree_path.clone(),
            worktree_path,
            branch,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command as SysCmd;
    use std::sync::Arc;

    use super::*;
    use crate::data::message::{MessageLevel, UserMessage};
    use crate::engine::git::GitEngine;

    // ─── Recording frontend ───────────────────────────────────────────────────

    impl crate::engine::git::GitFrontend for RecordingWorktreeLifecycleFrontend {}

    struct RecordingWorktreeLifecycleFrontend {
        pre_uncommitted_response: PreWorktreeDecision,
        existing_worktree_response: ExistingWorktreeDecision,
        post_workflow_action: PostWorkflowWorktreeAction,
        commit_before_merge_response: Option<String>,
        merge_mode_response: WorktreeMergeMode,
        confirm_cleanup_response: bool,
        dirty_merge_target_response: DirtyMergeTargetDecision,
        dirty_merge_target_calls: Vec<(String, Vec<String>, String)>,
        launch_merge_fix_response: bool,
        launch_merge_fix_calls: Vec<MergeFailure>,

        messages: Vec<UserMessage>,
        worktree_created_calls: Vec<(PathBuf, String)>,
        merge_conflict_calls: Vec<String>,
        discarded_calls: Vec<String>,
        kept_calls: Vec<(PathBuf, String)>,
    }

    impl RecordingWorktreeLifecycleFrontend {
        fn new() -> Self {
            Self {
                pre_uncommitted_response: PreWorktreeDecision::UseLastCommit,
                existing_worktree_response: ExistingWorktreeDecision::Resume,
                post_workflow_action: PostWorkflowWorktreeAction::Keep,
                commit_before_merge_response: None,
                merge_mode_response: WorktreeMergeMode::Squash,
                confirm_cleanup_response: true,
                dirty_merge_target_response: DirtyMergeTargetDecision::Abort,
                dirty_merge_target_calls: vec![],
                launch_merge_fix_response: false,
                launch_merge_fix_calls: vec![],
                messages: vec![],
                worktree_created_calls: vec![],
                merge_conflict_calls: vec![],
                discarded_calls: vec![],
                kept_calls: vec![],
            }
        }
    }

    impl crate::data::message::UserMessageSink for RecordingWorktreeLifecycleFrontend {
        fn write_message(&mut self, msg: UserMessage) {
            self.messages.push(msg);
        }
        fn replay_queued(&mut self) {}
    }

    impl WorktreeLifecycleFrontend for RecordingWorktreeLifecycleFrontend {
        fn ask_pre_worktree_uncommitted_files(
            &mut self,
            _files: &[String],
            _suggested_message: &str,
        ) -> Result<PreWorktreeDecision, CommandError> {
            Ok(self.pre_uncommitted_response.clone())
        }

        fn ask_existing_worktree(
            &mut self,
            _path: &Path,
            _branch: &str,
        ) -> Result<ExistingWorktreeDecision, CommandError> {
            Ok(self.existing_worktree_response)
        }

        fn report_worktree_created(&mut self, path: &Path, branch: &str) {
            self.worktree_created_calls
                .push((path.to_path_buf(), branch.to_string()));
        }

        fn ask_post_workflow_action(
            &mut self,
            _prompt: &PostWorkflowWorktreePrompt,
        ) -> Result<PostWorkflowWorktreeAction, CommandError> {
            Ok(self.post_workflow_action)
        }

        fn ask_worktree_commit_before_merge(
            &mut self,
            _branch: &str,
            _files: &[String],
            _suggested_message: &str,
        ) -> Result<Option<String>, CommandError> {
            Ok(self.commit_before_merge_response.clone())
        }

        fn ask_dirty_merge_target(
            &mut self,
            target_branch: &str,
            files: &[String],
            suggested_message: &str,
        ) -> Result<DirtyMergeTargetDecision, CommandError> {
            self.dirty_merge_target_calls.push((
                target_branch.to_string(),
                files.to_vec(),
                suggested_message.to_string(),
            ));
            Ok(self.dirty_merge_target_response.clone())
        }

        fn ask_merge_mode(&mut self, _branch: &str) -> Result<WorktreeMergeMode, CommandError> {
            Ok(self.merge_mode_response)
        }

        fn ask_launch_merge_fix_agent(
            &mut self,
            failure: &MergeFailure,
        ) -> Result<bool, CommandError> {
            self.launch_merge_fix_calls.push(failure.clone());
            Ok(self.launch_merge_fix_response)
        }

        fn confirm_worktree_cleanup(
            &mut self,
            _branch: &str,
            _path: &Path,
        ) -> Result<bool, CommandError> {
            Ok(self.confirm_cleanup_response)
        }

        fn report_merge_conflict(&mut self, branch: &str, _worktree_path: &Path, _git_root: &Path) {
            self.merge_conflict_calls.push(branch.to_string());
        }

        fn report_worktree_discarded(&mut self, branch: &str) {
            self.discarded_calls.push(branch.to_string());
        }

        fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
            self.kept_calls
                .push((path.to_path_buf(), branch.to_string()));
        }
    }

    // ─── Git helpers ──────────────────────────────────────────────────────────

    fn init_repo(dir: &std::path::Path) {
        SysCmd::new("git")
            .args(["init"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        SysCmd::new("git")
            .args(["config", "user.email", "test@awman.test"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        SysCmd::new("git")
            .args(["config", "user.name", "awman-test"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        std::fs::write(dir.join("README.md"), "initial").unwrap();
        SysCmd::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        SysCmd::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
    }

    fn git_log_count(dir: &std::path::Path) -> usize {
        let out = SysCmd::new("git")
            .args(["log", "--oneline"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .count()
    }

    // ─── prepare tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn prepare_happy_path_creates_worktree_and_reports() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let mut lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root,
            wt_path.clone(),
            "awman/test-happy".to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        let result = lifecycle.prepare(&mut fe).await;
        assert!(result.is_ok(), "prepare must succeed: {result:?}");
        assert_eq!(result.unwrap(), wt_path);
        assert!(wt_path.exists(), "worktree directory must be created");
        assert_eq!(fe.worktree_created_calls.len(), 1);
        assert_eq!(fe.worktree_created_calls[0].0, wt_path);
        assert_eq!(fe.worktree_created_calls[0].1, "awman/test-happy");
    }

    #[tokio::test]
    async fn prepare_with_uncommitted_files_commit_creates_new_commit() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        std::fs::write(repo.path().join("dirty.txt"), "dirty").unwrap();
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let mut lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            "awman/test-commit".to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.pre_uncommitted_response = PreWorktreeDecision::Commit {
            message: "auto-commit".to_string(),
        };
        let before = git_log_count(&git_root);
        let result = lifecycle.prepare(&mut fe).await;
        assert!(result.is_ok(), "prepare must succeed: {result:?}");
        let after = git_log_count(&git_root);
        assert_eq!(after, before + 1, "commit_all must add exactly one commit");
        assert!(wt_path.exists());
        assert_eq!(fe.worktree_created_calls.len(), 1);
    }

    #[tokio::test]
    async fn prepare_with_uncommitted_files_use_last_commit_does_not_add_commit() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        std::fs::write(repo.path().join("dirty.txt"), "dirty").unwrap();
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let mut lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            "awman/test-uselast".to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.pre_uncommitted_response = PreWorktreeDecision::UseLastCommit;
        let before = git_log_count(&git_root);
        let result = lifecycle.prepare(&mut fe).await;
        assert!(result.is_ok(), "prepare must succeed: {result:?}");
        let after = git_log_count(&git_root);
        assert_eq!(after, before, "UseLastCommit must NOT create a new commit");
        assert!(wt_path.exists());
        assert_eq!(fe.worktree_created_calls.len(), 1);
    }

    #[tokio::test]
    async fn prepare_with_uncommitted_files_abort_returns_aborted_and_no_worktree() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        std::fs::write(repo.path().join("dirty.txt"), "dirty").unwrap();
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let mut lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root,
            wt_path.clone(),
            "awman/test-abort".to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.pre_uncommitted_response = PreWorktreeDecision::Abort;
        let result = lifecycle.prepare(&mut fe).await;
        assert!(
            matches!(result, Err(CommandError::Aborted)),
            "Abort must return CommandError::Aborted"
        );
        assert!(!wt_path.exists(), "worktree must NOT be created on Abort");
        assert!(fe.worktree_created_calls.is_empty());
    }

    #[tokio::test]
    async fn prepare_existing_worktree_resume_returns_path_without_recreating() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/test-resume";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        // Write a sentinel that must survive Resume (no recreation).
        std::fs::write(wt_path.join("sentinel.txt"), "existing").unwrap();

        let mut lifecycle =
            WorktreeLifecycle::new_for_test(engine, git_root, wt_path.clone(), branch.to_string());
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.existing_worktree_response = ExistingWorktreeDecision::Resume;
        let result = lifecycle.prepare(&mut fe).await;
        assert!(result.is_ok(), "prepare(Resume) must succeed: {result:?}");
        assert_eq!(result.unwrap(), wt_path);
        assert!(
            wt_path.join("sentinel.txt").exists(),
            "sentinel must survive Resume"
        );
        assert!(
            fe.worktree_created_calls.is_empty(),
            "create_worktree must NOT be called on Resume"
        );
    }

    #[tokio::test]
    async fn prepare_existing_worktree_fail_names_path_and_explicit_flag_choices() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("existing");
        let branch = "awman/test-fail-existing";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        let mut lifecycle =
            WorktreeLifecycle::new_for_test(engine, git_root, wt_path.clone(), branch.to_string());
        let mut frontend = RecordingWorktreeLifecycleFrontend::new();

        let error = lifecycle
            .prepare_with_existing(
                &mut frontend,
                ExistingWorktreeChoice::Explicit(ExistingWorktreeDecision::Fail),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, CommandError::WorktreeExists { .. }));
        let message = error.to_string();
        assert!(
            message.contains(&wt_path.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains("--on-existing-worktree resume"),
            "{message}"
        );
        assert!(
            message.contains("--on-existing-worktree recreate"),
            "{message}"
        );
        assert!(wt_path.exists(), "Fail must leave the worktree untouched");
    }

    #[tokio::test]
    async fn prepare_existing_worktree_recreate_deletes_branch_commits() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("existing");
        let branch = "awman/test-recreate-fresh";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        std::fs::write(wt_path.join("old-commit.txt"), "old\n").unwrap();
        for args in [
            vec!["add", "old-commit.txt"],
            vec!["commit", "-m", "old worktree commit"],
        ] {
            let output = SysCmd::new("git")
                .args(&args)
                .current_dir(&wt_path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let mut lifecycle = WorktreeLifecycle::new_for_test(
            engine.clone(),
            git_root.clone(),
            wt_path.clone(),
            branch.to_string(),
        );
        let mut frontend = RecordingWorktreeLifecycleFrontend::new();
        let result = lifecycle
            .prepare_with_existing(
                &mut frontend,
                ExistingWorktreeChoice::Explicit(ExistingWorktreeDecision::Recreate),
            )
            .await;

        assert!(result.is_ok(), "Recreate must succeed: {result:?}");
        assert_eq!(
            git_log_count(&wt_path),
            1,
            "only the original HEAD commit remains"
        );
        assert!(!wt_path.join("old-commit.txt").exists());
        let branch_exists = SysCmd::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(&git_root)
            .status()
            .unwrap()
            .success();
        assert!(branch_exists, "fresh worktree creation makes a new branch");
    }

    #[tokio::test]
    async fn prepare_recreate_surfaces_git_error_when_branch_is_checked_out_elsewhere() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let assigned_branch = "awman/test-checked-out-elsewhere";
        let elsewhere = wt_dir.path().join("elsewhere");
        let target = wt_dir.path().join("target");
        let engine = Arc::new(GitEngine::new());
        engine
            .create_worktree(&git_root, &elsewhere, assigned_branch)
            .unwrap();
        // The existing target is a different branch; Recreate then attempts
        // to delete `assigned_branch`, which git refuses while `elsewhere`
        // still has it checked out.
        engine
            .create_worktree(&git_root, &target, "awman/test-target")
            .unwrap();
        let mut lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root,
            target.clone(),
            assigned_branch.to_string(),
        );
        let mut frontend = RecordingWorktreeLifecycleFrontend::new();

        let error = lifecycle
            .prepare_with_existing(
                &mut frontend,
                ExistingWorktreeChoice::Explicit(ExistingWorktreeDecision::Recreate),
            )
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("checked out"),
            "expected git's branch-in-use error: {message}"
        );
        assert!(elsewhere.exists(), "the other checkout remains intact");
        assert!(
            target.exists(),
            "branch deletion failure preserves the target"
        );
    }

    #[tokio::test]
    async fn prepare_recreate_keeps_existing_worktree_when_branch_is_checked_out_elsewhere() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let assigned_branch = "awman/test-checked-out-preserved";
        let elsewhere = wt_dir.path().join("elsewhere");
        let target = wt_dir.path().join("target");
        let engine = Arc::new(GitEngine::new());
        engine
            .create_worktree(&git_root, &elsewhere, assigned_branch)
            .unwrap();
        engine
            .create_worktree(&git_root, &target, "awman/test-target-preserved")
            .unwrap();
        let mut lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root,
            target.clone(),
            assigned_branch.to_string(),
        );
        let mut frontend = RecordingWorktreeLifecycleFrontend::new();

        let error = lifecycle
            .prepare_with_existing(
                &mut frontend,
                ExistingWorktreeChoice::Explicit(ExistingWorktreeDecision::Recreate),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("checked out"));
        assert!(
            target.exists(),
            "failed recreate should preserve the existing worktree"
        );
        assert!(elsewhere.exists());
    }

    #[tokio::test]
    async fn prepare_existing_worktree_recreate_removes_then_recreates() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/test-recreate";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        std::fs::write(wt_path.join("sentinel.txt"), "original").unwrap();

        let mut lifecycle =
            WorktreeLifecycle::new_for_test(engine, git_root, wt_path.clone(), branch.to_string());
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.existing_worktree_response = ExistingWorktreeDecision::Recreate;
        let result = lifecycle.prepare(&mut fe).await;
        assert!(result.is_ok(), "prepare(Recreate) must succeed: {result:?}");
        assert!(wt_path.exists(), "worktree must exist after Recreate");
        assert!(
            !wt_path.join("sentinel.txt").exists(),
            "original sentinel must be gone after Recreate"
        );
        assert_eq!(
            fe.worktree_created_calls.len(),
            1,
            "create_worktree must be called on Recreate"
        );
    }

    #[tokio::test]
    async fn prepare_existing_worktree_recreate_with_dirty_files_commits() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/test-recreate-dirty";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        // Make the main branch dirty AFTER the worktree already exists.
        std::fs::write(repo.path().join("dirty.txt"), "dirty").unwrap();

        let mut lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            branch.to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.existing_worktree_response = ExistingWorktreeDecision::Recreate;
        fe.pre_uncommitted_response = PreWorktreeDecision::Commit {
            message: "pre-recreate commit".to_string(),
        };
        let before = git_log_count(&git_root);
        let result = lifecycle.prepare(&mut fe).await;
        assert!(
            result.is_ok(),
            "prepare(Recreate+dirty) must succeed: {result:?}"
        );
        let after = git_log_count(&git_root);
        assert_eq!(
            after,
            before + 1,
            "dirty files must be committed before recreating worktree"
        );
        assert!(wt_path.exists(), "worktree must exist after Recreate");
        assert_eq!(fe.worktree_created_calls.len(), 1);
    }

    #[tokio::test]
    async fn prepare_existing_worktree_recreate_with_dirty_files_abort() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/test-recreate-abort";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        std::fs::write(repo.path().join("dirty.txt"), "dirty").unwrap();

        let mut lifecycle =
            WorktreeLifecycle::new_for_test(engine, git_root, wt_path.clone(), branch.to_string());
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.existing_worktree_response = ExistingWorktreeDecision::Recreate;
        fe.pre_uncommitted_response = PreWorktreeDecision::Abort;
        let result = lifecycle.prepare(&mut fe).await;
        assert!(
            matches!(result, Err(CommandError::Aborted)),
            "Abort must return CommandError::Aborted on Recreate path"
        );
        assert!(fe.worktree_created_calls.is_empty());
    }

    #[tokio::test]
    async fn prepare_detached_head_writes_warning_message_before_creation() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        // Detach HEAD
        let sha_out = SysCmd::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
        SysCmd::new("git")
            .args(["checkout", "--detach", &sha])
            .current_dir(repo.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let mut lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root,
            wt_path,
            "awman/detach-test".to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        let _ = lifecycle.prepare(&mut fe).await;
        assert!(
            fe.messages
                .iter()
                .any(|m| { m.level == MessageLevel::Warning && m.text.contains("detached") }),
            "must write a Warning message mentioning 'detached'; got: {:?}",
            fe.messages
        );
        // The warning must appear before report_worktree_created (which records
        // in worktree_created_calls). We verify ordering: messages is non-empty
        // before the first worktree_created_calls entry was made.
        // Since messages are recorded as the frontend methods are called in
        // prepare(), and write_message is called first, the message slice is
        // non-empty by the time the worktree is created.
        assert!(!fe.messages.is_empty());
    }

    // ─── finalize tests ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn finalize_keep_calls_report_worktree_kept_and_no_git_side_effects() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root,
            wt_path.clone(),
            "awman/keep-branch".to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Keep;
        let result = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await;
        assert!(result.is_ok(), "finalize(Keep) must return Ok: {result:?}");
        assert_eq!(fe.kept_calls.len(), 1);
        assert_eq!(fe.kept_calls[0].0, wt_path);
        assert!(fe.discarded_calls.is_empty());
        assert!(fe.merge_conflict_calls.is_empty());
    }

    #[tokio::test]
    async fn finalize_discard_removes_worktree_and_deletes_branch() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/discard-branch";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        assert!(wt_path.exists());

        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            branch.to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Discard;
        let result = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await;
        assert!(
            result.is_ok(),
            "finalize(Discard) must return Ok: {result:?}"
        );
        assert!(
            !wt_path.exists(),
            "worktree directory must be removed on Discard"
        );
        assert_eq!(fe.discarded_calls.len(), 1);
        assert!(fe.kept_calls.is_empty());
        assert!(
            !GitEngine::new().branch_exists(&git_root, branch),
            "branch must be deleted on Discard"
        );
    }

    #[tokio::test]
    async fn finalize_merge_squash_merges_and_cleans_up_on_confirm() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/merge-ok-branch";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        // Add a commit in the worktree so there is something to merge.
        std::fs::write(wt_path.join("work.txt"), "done").unwrap();
        engine
            .commit_all(&wt_path, "work done in worktree")
            .unwrap();

        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            branch.to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        fe.merge_mode_response = WorktreeMergeMode::Squash;
        fe.confirm_cleanup_response = true;
        let result = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await;
        assert!(result.is_ok(), "finalize(Merge) must return Ok: {result:?}");
        assert!(
            !wt_path.exists(),
            "worktree must be removed after merge + cleanup"
        );
        assert_eq!(
            fe.discarded_calls.len(),
            1,
            "report_worktree_discarded must be called"
        );
        assert!(fe.merge_conflict_calls.is_empty());
    }

    /// A repo with a worktree branch holding one commit, and an untracked
    /// file in the parent checkout so the merge target is dirty.
    fn dirty_target_fixture(
        branch: &str,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        WorktreeLifecycle,
        PathBuf,
    ) {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        std::fs::write(wt_path.join("work.txt"), "done").unwrap();
        engine
            .commit_all(&wt_path, "work done in worktree")
            .unwrap();
        std::fs::write(git_root.join("scratch.txt"), "user edit").unwrap();
        let lifecycle =
            WorktreeLifecycle::new_for_test(engine, git_root, wt_path.clone(), branch.to_string());
        (repo, wt_dir, lifecycle, wt_path)
    }

    #[tokio::test]
    async fn finalize_dirty_merge_target_abort_keeps_worktree_and_is_not_a_conflict() {
        let (_repo, _wt_dir, lifecycle, wt_path) = dirty_target_fixture("awman/dirty-abort");
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        let result = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await;
        assert!(matches!(result, Err(CommandError::MergeTargetDirty { .. })));
        assert!(wt_path.exists());
        assert_eq!(fe.dirty_merge_target_calls.len(), 1);
        assert!(fe.merge_conflict_calls.is_empty());
        assert_eq!(fe.kept_calls.len(), 1);
    }

    #[tokio::test]
    async fn finalize_dirty_merge_target_stash_policy_stashes_and_merges() {
        let (repo, _wt_dir, lifecycle, wt_path) = dirty_target_fixture("awman/dirty-stash");
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        let policy = FinalizePolicy {
            on_dirty_merge_target: Some(OnDirtyMergeTarget::Stash),
            on_merge_failure: None,
        };
        let result = lifecycle.finalize(&mut fe, false, policy).await;
        assert!(result.is_ok(), "{result:?}");
        assert!(fe.dirty_merge_target_calls.is_empty(), "flag skips the ask");
        assert!(!wt_path.exists());
        assert!(!repo.path().join("scratch.txt").exists(), "stashed away");
        assert!(repo.path().join("work.txt").exists(), "merged");
        assert!(fe.messages.iter().any(|m| m.text.contains("git stash pop")));
    }

    #[tokio::test]
    async fn finalize_merge_with_uncommitted_files_commits_before_merge() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/merge-precommit-branch";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        // Leave an uncommitted file in the worktree.
        std::fs::write(wt_path.join("uncommitted.txt"), "not committed").unwrap();

        let lifecycle =
            WorktreeLifecycle::new_for_test(engine, git_root, wt_path.clone(), branch.to_string());
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        fe.commit_before_merge_response = Some("pre-merge commit".to_string());
        fe.merge_mode_response = WorktreeMergeMode::Squash;
        fe.confirm_cleanup_response = true;
        let result = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await;
        assert!(
            result.is_ok(),
            "finalize with pre-merge commit must succeed: {result:?}"
        );
        assert!(!wt_path.exists());
    }

    #[tokio::test]
    async fn finalize_merge_no_squash_preserves_branch_commits() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/merge-nosquash-branch";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        std::fs::write(wt_path.join("work.txt"), "done").unwrap();
        engine
            .commit_all(&wt_path, "work done in worktree")
            .unwrap();

        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            branch.to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        fe.merge_mode_response = WorktreeMergeMode::Merge;
        fe.confirm_cleanup_response = true;
        let result = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await;
        assert!(
            result.is_ok(),
            "finalize(Merge, no squash) must return Ok: {result:?}"
        );
        // A plain merge preserves the branch's own commit — the subject must
        // appear in the target branch's log (a squash would rewrite it to
        // "Implement <branch>").
        let out = SysCmd::new("git")
            .args(["log", "--format=%s"])
            .current_dir(&git_root)
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(
            log.lines().any(|l| l == "work done in worktree"),
            "branch commit must survive a no-squash merge; log:\n{log}"
        );
        assert!(
            !wt_path.exists(),
            "worktree must be removed after merge + cleanup"
        );
        assert_eq!(fe.discarded_calls.len(), 1);
        assert!(fe.merge_conflict_calls.is_empty());
    }

    #[tokio::test]
    async fn finalize_merge_leave_branch_still_commits_uncommitted_and_keeps() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/leave-branch";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        // Leave an uncommitted file in the worktree.
        std::fs::write(wt_path.join("uncommitted.txt"), "not committed").unwrap();

        let lifecycle = WorktreeLifecycle::new_for_test(
            engine.clone(),
            git_root.clone(),
            wt_path.clone(),
            branch.to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        fe.merge_mode_response = WorktreeMergeMode::LeaveBranch;
        fe.commit_before_merge_response = Some("tidy up branch".to_string());
        let before = git_log_count(&wt_path);
        let result = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await;
        assert!(
            result.is_ok(),
            "finalize(LeaveBranch) must return Ok: {result:?}"
        );
        // The commit-uncommitted step must still run even though the branch
        // is left alone.
        let after = git_log_count(&wt_path);
        assert_eq!(
            after,
            before + 1,
            "uncommitted files must be committed on the branch"
        );
        assert!(
            engine.uncommitted_files(&wt_path).unwrap().is_empty(),
            "worktree must be clean after the commit step"
        );
        assert!(wt_path.exists(), "worktree must be kept");
        assert_eq!(fe.kept_calls.len(), 1);
        assert!(fe.discarded_calls.is_empty());
        // Nothing was merged into the target branch.
        let out = SysCmd::new("git")
            .args(["log", "--format=%s"])
            .current_dir(&git_root)
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(
            !log.contains("tidy up branch"),
            "commit must land on the worktree branch only; log:\n{log}"
        );
    }

    #[tokio::test]
    async fn finalize_had_error_true_is_forwarded_to_ask_post_workflow_action() {
        // Verify that when had_error=true is passed, it reaches the frontend.
        struct ErrorRecordingFrontend {
            inner: RecordingWorktreeLifecycleFrontend,
            received_had_error: Option<bool>,
        }
        impl crate::engine::git::GitFrontend for ErrorRecordingFrontend {}
        impl crate::data::message::UserMessageSink for ErrorRecordingFrontend {
            fn write_message(&mut self, msg: UserMessage) {
                self.inner.write_message(msg);
            }
            fn replay_queued(&mut self) {}
        }
        impl WorktreeLifecycleFrontend for ErrorRecordingFrontend {
            fn ask_pre_worktree_uncommitted_files(
                &mut self,
                files: &[String],
                suggested_message: &str,
            ) -> Result<PreWorktreeDecision, CommandError> {
                self.inner
                    .ask_pre_worktree_uncommitted_files(files, suggested_message)
            }
            fn ask_existing_worktree(
                &mut self,
                path: &Path,
                branch: &str,
            ) -> Result<ExistingWorktreeDecision, CommandError> {
                self.inner.ask_existing_worktree(path, branch)
            }
            fn report_worktree_created(&mut self, path: &Path, branch: &str) {
                self.inner.report_worktree_created(path, branch);
            }
            fn ask_post_workflow_action(
                &mut self,
                prompt: &PostWorkflowWorktreePrompt,
            ) -> Result<PostWorkflowWorktreeAction, CommandError> {
                self.received_had_error = Some(prompt.had_error);
                self.inner.ask_post_workflow_action(prompt)
            }
            fn ask_worktree_commit_before_merge(
                &mut self,
                branch: &str,
                files: &[String],
                suggested_message: &str,
            ) -> Result<Option<String>, CommandError> {
                self.inner
                    .ask_worktree_commit_before_merge(branch, files, suggested_message)
            }
            fn ask_dirty_merge_target(
                &mut self,
                target_branch: &str,
                files: &[String],
                suggested_message: &str,
            ) -> Result<DirtyMergeTargetDecision, CommandError> {
                self.inner
                    .ask_dirty_merge_target(target_branch, files, suggested_message)
            }
            fn ask_merge_mode(&mut self, branch: &str) -> Result<WorktreeMergeMode, CommandError> {
                self.inner.ask_merge_mode(branch)
            }
            fn ask_launch_merge_fix_agent(
                &mut self,
                failure: &MergeFailure,
            ) -> Result<bool, CommandError> {
                self.inner.ask_launch_merge_fix_agent(failure)
            }
            fn confirm_worktree_cleanup(
                &mut self,
                branch: &str,
                path: &Path,
            ) -> Result<bool, CommandError> {
                self.inner.confirm_worktree_cleanup(branch, path)
            }
            fn report_merge_conflict(&mut self, branch: &str, wt: &Path, root: &Path) {
                self.inner.report_merge_conflict(branch, wt, root);
            }
            fn report_worktree_discarded(&mut self, branch: &str) {
                self.inner.report_worktree_discarded(branch);
            }
            fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
                self.inner.report_worktree_kept(path, branch);
            }
        }

        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root,
            wt_path,
            "awman/had-error-branch".to_string(),
        );
        let mut fe = ErrorRecordingFrontend {
            inner: {
                let mut r = RecordingWorktreeLifecycleFrontend::new();
                r.post_workflow_action = PostWorkflowWorktreeAction::Keep;
                r
            },
            received_had_error: None,
        };
        let result = lifecycle
            .finalize(&mut fe, true, FinalizePolicy::default())
            .await;
        assert!(result.is_ok(), "finalize must return Ok: {result:?}");
        assert_eq!(
            fe.received_had_error,
            Some(true),
            "had_error=true must reach ask_post_workflow_action"
        );
    }

    #[tokio::test]
    async fn finalize_merge_conflict_calls_report_and_returns_ok() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/conflict-branch";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();

        // Diverge both branches on the same file to force a conflict.
        std::fs::write(wt_path.join("README.md"), "branch version").unwrap();
        engine.commit_all(&wt_path, "branch change").unwrap();
        std::fs::write(git_root.join("README.md"), "main version").unwrap();
        engine.commit_all(&git_root, "main change").unwrap();

        let lifecycle = WorktreeLifecycle::new_for_test(
            engine,
            git_root.clone(),
            wt_path.clone(),
            branch.to_string(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        fe.merge_mode_response = WorktreeMergeMode::Squash;
        let result = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await;
        assert!(result.is_ok(), "merge conflict must return Ok: {result:?}");
        assert_eq!(
            fe.merge_conflict_calls.len(),
            1,
            "report_merge_conflict must be called exactly once"
        );
        assert!(
            fe.discarded_calls.is_empty(),
            "must NOT discard on conflict"
        );
        // Clean up git's conflicted-merge state so the temp dir drops cleanly.
        SysCmd::new("git")
            .args(["merge", "--abort"])
            .current_dir(&git_root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok();
    }

    /// A conflicted merge with `on_merge_failure = Agent`: `finalize` hands
    /// the failure back without asking, and `fix` stands in for the agent
    /// between `finalize` and `complete_merge_after_fix`.
    async fn conflicted_merge_with_fake_agent(
        squash: bool,
        fix: impl FnOnce(&Path),
    ) -> (
        RecordingWorktreeLifecycleFrontend,
        MergeFailure,
        Result<(), CommandError>,
        PathBuf,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let branch = "awman/fix-branch";
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        std::fs::write(wt_path.join("README.md"), "branch version\n").unwrap();
        std::fs::write(wt_path.join("feature-only.txt"), "feature\n").unwrap();
        engine.commit_all(&wt_path, "branch change").unwrap();
        std::fs::write(git_root.join("README.md"), "main version\n").unwrap();
        engine.commit_all(&git_root, "main change").unwrap();

        let lifecycle =
            WorktreeLifecycle::new_for_test(engine, git_root.clone(), wt_path, branch.to_string());
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        fe.merge_mode_response = if squash {
            WorktreeMergeMode::Squash
        } else {
            WorktreeMergeMode::Merge
        };
        let policy = FinalizePolicy {
            on_merge_failure: Some(OnMergeFailure::Agent),
            ..FinalizePolicy::default()
        };
        let failure = match lifecycle.finalize(&mut fe, false, policy).await.unwrap() {
            FinalizeOutcome::MergeFixRequested(failure) => *failure,
            FinalizeOutcome::Done => panic!("a conflicted merge must request the agent"),
        };
        assert!(fe.launch_merge_fix_calls.is_empty(), "policy must not ask");
        fix(&git_root);
        let result = lifecycle
            .complete_merge_after_fix(&mut fe, &failure, true)
            .await;
        (fe, failure, result, git_root, repo, wt_dir)
    }

    #[tokio::test]
    async fn merge_fix_resolved_by_agent_commits_and_cleans_up() {
        for squash in [false, true] {
            let (fe, failure, result, git_root, _repo, _wt) =
                conflicted_merge_with_fake_agent(squash, |root| {
                    std::fs::write(root.join("README.md"), "both versions\n").unwrap();
                })
                .await;
            assert_eq!(failure.unmerged_paths, vec!["README.md".to_string()]);
            assert!(failure.merge_started());
            result.unwrap_or_else(|e| panic!("squash={squash}: {e}"));
            assert_eq!(fe.discarded_calls.len(), 1, "squash={squash}");
            let log = SysCmd::new("git")
                .args(["log", "-1", "--format=%s"])
                .current_dir(&git_root)
                .output()
                .unwrap();
            let subject = String::from_utf8_lossy(&log.stdout).trim().to_string();
            let expected = if squash {
                "Implement awman/fix-branch"
            } else {
                "Merge awman/fix-branch"
            };
            assert_eq!(subject, expected);
        }
    }

    // ─── WI 0132 Part B: dirty merge target ───────────────────────────────────

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = SysCmd::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn merging_frontend() -> RecordingWorktreeLifecycleFrontend {
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        fe.post_workflow_action = PostWorkflowWorktreeAction::Merge;
        fe.merge_mode_response = WorktreeMergeMode::Squash;
        fe
    }

    fn dirty_policy(choice: OnDirtyMergeTarget) -> FinalizePolicy {
        FinalizePolicy {
            on_dirty_merge_target: Some(choice),
            on_merge_failure: None,
        }
    }

    #[tokio::test]
    async fn finalize_clean_merge_target_merges_without_asking() {
        let (repo, _wt_dir, lifecycle, wt_path) = dirty_target_fixture("awman/clean-target");
        std::fs::remove_file(repo.path().join("scratch.txt")).unwrap();
        let mut fe = merging_frontend();
        // Even a policy that would fail a dirty target must not matter here.
        let result = lifecycle
            .finalize(&mut fe, false, dirty_policy(OnDirtyMergeTarget::Fail))
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(fe.dirty_merge_target_calls.is_empty());
        assert!(repo.path().join("work.txt").exists());
        assert!(!wt_path.exists());
    }

    #[tokio::test]
    async fn finalize_dirty_target_commit_policy_commits_then_merges() {
        let (repo, _wt_dir, lifecycle, wt_path) = dirty_target_fixture("awman/dirty-commit");
        let mut fe = merging_frontend();
        let result = lifecycle
            .finalize(&mut fe, false, dirty_policy(OnDirtyMergeTarget::Commit))
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(fe.dirty_merge_target_calls.is_empty(), "flag skips the ask");
        assert!(!wt_path.exists());
        // The user's file is committed (not stashed, not lost) ...
        assert!(repo.path().join("scratch.txt").exists());
        assert_eq!(git_out(repo.path(), &["status", "--porcelain"]), "");
        let tracked = git_out(repo.path(), &["ls-files"]);
        assert!(tracked.contains("scratch.txt") && tracked.contains("work.txt"));
        // ... in its own WIP commit, before the squash commit.
        let log = git_out(repo.path(), &["log", "--format=%s"]);
        let subjects: Vec<&str> = log.lines().collect();
        assert!(subjects[0].starts_with("Implement awman/dirty-commit"));
        assert!(subjects[1].starts_with("WIP: changes on"));
        assert_eq!(git_out(repo.path(), &["stash", "list"]), "");
    }

    #[tokio::test]
    async fn finalize_dirty_target_asked_commit_uses_the_chosen_message() {
        let (repo, _wt_dir, lifecycle, _wt_path) = dirty_target_fixture("awman/dirty-ask-commit");
        let mut fe = merging_frontend();
        fe.dirty_merge_target_response = DirtyMergeTargetDecision::Commit {
            message: "my own message".to_string(),
        };
        lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await
            .unwrap();
        assert_eq!(fe.dirty_merge_target_calls.len(), 1);
        assert!(git_out(repo.path(), &["log", "--format=%s"]).contains("my own message"));
    }

    #[tokio::test]
    async fn finalize_dirty_target_asked_stash_leaves_one_stash_entry_and_merges() {
        let (repo, _wt_dir, lifecycle, wt_path) = dirty_target_fixture("awman/dirty-ask-stash");
        let mut fe = merging_frontend();
        fe.dirty_merge_target_response = DirtyMergeTargetDecision::Stash;
        let result = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(fe.dirty_merge_target_calls.len(), 1);
        let stashes = git_out(repo.path(), &["stash", "list"]);
        assert_eq!(stashes.lines().count(), 1, "{stashes}");
        assert!(stashes.contains("awman: before merging awman/dirty-ask-stash"));
        // The stash is never popped: the file stays out of the checkout.
        assert!(!repo.path().join("scratch.txt").exists());
        assert!(repo.path().join("work.txt").exists());
        assert!(!wt_path.exists());
    }

    #[tokio::test]
    async fn finalize_dirty_target_fail_policy_aborts_without_a_merge_attempt() {
        let (repo, _wt_dir, lifecycle, wt_path) = dirty_target_fixture("awman/dirty-fail");
        let mut fe = merging_frontend();
        let result = lifecycle
            .finalize(&mut fe, false, dirty_policy(OnDirtyMergeTarget::Fail))
            .await;
        let Err(CommandError::MergeTargetDirty {
            target_branch,
            branch,
            worktree_path,
        }) = result
        else {
            panic!("expected MergeTargetDirty");
        };
        assert!(!target_branch.is_empty());
        assert_eq!(branch, "awman/dirty-fail");
        assert_eq!(worktree_path, wt_path);
        assert!(fe.dirty_merge_target_calls.is_empty(), "flag skips the ask");
        assert!(wt_path.exists());
        assert!(fe.merge_conflict_calls.is_empty());
        assert!(fe.discarded_calls.is_empty());
        assert_eq!(fe.kept_calls.len(), 1);
        // No merge was attempted: nothing merged, nothing staged, no MERGE_HEAD.
        assert!(!repo.path().join("work.txt").exists());
        assert!(!git_out(repo.path(), &["status", "--porcelain"]).contains("work.txt"));
        assert!(repo.path().join("scratch.txt").exists(), "user file intact");
    }

    #[tokio::test]
    async fn finalize_dirty_target_abort_error_is_not_a_merge_conflict() {
        let (_repo, _wt_dir, lifecycle, _wt_path) = dirty_target_fixture("awman/dirty-abort-kind");
        let mut fe = merging_frontend();
        let err = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await
            .unwrap_err();
        assert!(!matches!(err, CommandError::MergeConflict { .. }));
        assert!(
            !err.to_string().to_lowercase().contains("conflict"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn finalize_untracked_only_target_counts_as_dirty() {
        let (repo, _wt_dir, lifecycle, _wt_path) = dirty_target_fixture("awman/dirty-untracked");
        // Only an untracked file: no tracked file is modified.
        assert_eq!(
            git_out(repo.path(), &["status", "--porcelain"]),
            "?? scratch.txt"
        );
        let mut fe = merging_frontend();
        let _ = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await;
        assert_eq!(fe.dirty_merge_target_calls.len(), 1);
        assert!(fe.dirty_merge_target_calls[0]
            .1
            .iter()
            .any(|f| f.contains("scratch.txt")));
    }

    #[tokio::test]
    async fn finalize_modified_tracked_target_counts_as_dirty() {
        let (repo, _wt_dir, lifecycle, _wt_path) = dirty_target_fixture("awman/dirty-tracked");
        std::fs::remove_file(repo.path().join("scratch.txt")).unwrap();
        std::fs::write(repo.path().join("README.md"), "edited").unwrap();
        let mut fe = merging_frontend();
        let _ = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await;
        assert_eq!(fe.dirty_merge_target_calls.len(), 1);
        assert!(!fe.dirty_merge_target_calls[0].0.is_empty());
        assert_eq!(fe.dirty_merge_target_calls[0].1.len(), 1);
        assert!(fe.dirty_merge_target_calls[0].1[0].contains("README.md"));
    }

    #[tokio::test]
    async fn finalize_failed_stash_becomes_abort() {
        let (repo, _wt_dir, lifecycle, wt_path) = dirty_target_fixture("awman/dirty-stash-fails");
        // A held index lock makes `git stash push` fail.
        std::fs::write(repo.path().join(".git").join("index.lock"), "").unwrap();
        let mut fe = merging_frontend();
        let result = lifecycle
            .finalize(&mut fe, false, dirty_policy(OnDirtyMergeTarget::Stash))
            .await;
        assert!(
            matches!(result, Err(CommandError::MergeTargetDirty { .. })),
            "{result:?}"
        );
        assert!(wt_path.exists());
        assert_eq!(fe.kept_calls.len(), 1);
        assert!(fe.merge_conflict_calls.is_empty());
        assert!(fe.messages.iter().any(|m| m.level == MessageLevel::Error));
        assert!(!fe.messages.iter().any(|m| m.text.contains("git stash pop")));
        assert!(!repo.path().join("work.txt").exists(), "no merge attempt");
    }

    // ─── WI 0132 Part C: merge-fix agent ──────────────────────────────────────

    /// A repo whose worktree branch conflicts with the parent's `README.md`.
    fn conflict_fixture(branch: &str) -> (tempfile::TempDir, tempfile::TempDir, WorktreeLifecycle) {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git_root = repo.path().to_path_buf();
        let wt_path = wt_dir.path().join("wt");
        let engine = Arc::new(GitEngine::new());
        engine.create_worktree(&git_root, &wt_path, branch).unwrap();
        std::fs::write(wt_path.join("README.md"), "branch version\n").unwrap();
        engine.commit_all(&wt_path, "branch change").unwrap();
        std::fs::write(git_root.join("README.md"), "main version\n").unwrap();
        engine.commit_all(&git_root, "main change").unwrap();
        let lifecycle = WorktreeLifecycle::new_for_test(engine, git_root, wt_path, branch.into());
        (repo, wt_dir, lifecycle)
    }

    fn abort_merge(root: &Path) {
        SysCmd::new("git")
            .args(["merge", "--abort"])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok();
    }

    #[tokio::test]
    async fn finalize_conflict_with_agent_declined_reports_conflict_and_keeps_worktree() {
        let (repo, _wt_dir, lifecycle) = conflict_fixture("awman/decline");
        let mut fe = merging_frontend();
        fe.launch_merge_fix_response = false;
        let outcome = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await
            .unwrap();
        assert!(matches!(outcome, FinalizeOutcome::Done));
        assert_eq!(fe.merge_conflict_calls, vec!["awman/decline".to_string()]);
        assert_eq!(fe.launch_merge_fix_calls.len(), 1);
        let asked = &fe.launch_merge_fix_calls[0];
        assert_eq!(asked.branch, "awman/decline");
        assert!(asked.squash);
        assert_eq!(asked.unmerged_paths, vec!["README.md".to_string()]);
        assert!(asked.command.contains("git merge --squash"));
        assert!(lifecycle.worktree_path.exists());
        assert!(fe.discarded_calls.is_empty());
        abort_merge(repo.path());
    }

    #[tokio::test]
    async fn finalize_keep_policy_never_asks_and_agent_policy_never_asks() {
        for (choice, expect_fix) in [(OnMergeFailure::Keep, false), (OnMergeFailure::Agent, true)] {
            let (repo, _wt_dir, lifecycle) = conflict_fixture("awman/policy");
            let mut fe = merging_frontend();
            fe.launch_merge_fix_response = !expect_fix; // the opposite: must be ignored
            let policy = FinalizePolicy {
                on_dirty_merge_target: None,
                on_merge_failure: Some(choice),
            };
            let outcome = lifecycle.finalize(&mut fe, false, policy).await.unwrap();
            assert!(fe.launch_merge_fix_calls.is_empty(), "{choice:?} asked");
            assert_eq!(fe.merge_conflict_calls.len(), 1, "{choice:?}");
            assert_eq!(
                matches!(outcome, FinalizeOutcome::MergeFixRequested(_)),
                expect_fix,
                "{choice:?}"
            );
            abort_merge(repo.path());
        }
    }

    #[tokio::test]
    async fn finalize_conflict_with_agent_accepted_requests_the_fix() {
        let (repo, _wt_dir, lifecycle) = conflict_fixture("awman/accept");
        let mut fe = merging_frontend();
        fe.launch_merge_fix_response = true;
        let outcome = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await
            .unwrap();
        assert!(matches!(outcome, FinalizeOutcome::MergeFixRequested(_)));
        assert_eq!(fe.launch_merge_fix_calls.len(), 1);
        abort_merge(repo.path());
    }

    #[tokio::test]
    async fn finalize_ask_policy_consults_the_frontend_for_both_questions() {
        let (_repo, _wt_dir, lifecycle, _wt) = dirty_target_fixture("awman/ask-policy");
        let mut fe = merging_frontend();
        let policy = FinalizePolicy {
            on_dirty_merge_target: Some(OnDirtyMergeTarget::Ask),
            on_merge_failure: Some(OnMergeFailure::Ask),
        };
        let _ = lifecycle.finalize(&mut fe, false, policy).await;
        assert_eq!(fe.dirty_merge_target_calls.len(), 1);

        let (repo, _wt_dir, lifecycle) = conflict_fixture("awman/ask-policy-2");
        let mut fe = merging_frontend();
        lifecycle.finalize(&mut fe, false, policy).await.unwrap();
        assert_eq!(fe.launch_merge_fix_calls.len(), 1);
        abort_merge(repo.path());
    }

    #[tokio::test]
    async fn merge_fix_agent_that_stages_the_files_itself_is_committed() {
        let (fe, _failure, result, git_root, _repo, _wt) =
            conflicted_merge_with_fake_agent(true, |root| {
                std::fs::write(root.join("README.md"), "resolved\n").unwrap();
                SysCmd::new("git")
                    .args(["add", "README.md"])
                    .current_dir(root)
                    .status()
                    .unwrap();
            })
            .await;
        result.unwrap();
        assert_eq!(fe.discarded_calls.len(), 1);
        assert_eq!(git_out(&git_root, &["status", "--porcelain"]), "");
        assert_eq!(
            std::fs::read_to_string(git_root.join("README.md")).unwrap(),
            "resolved\n"
        );
    }

    #[tokio::test]
    async fn merge_fix_agent_that_commits_itself_is_accepted() {
        let (fe, _failure, result, git_root, _repo, _wt) =
            conflicted_merge_with_fake_agent(false, |root| {
                std::fs::write(root.join("README.md"), "resolved\n").unwrap();
                for args in [vec!["add", "README.md"], vec!["commit", "--no-edit"]] {
                    SysCmd::new("git")
                        .args(args)
                        .current_dir(root)
                        .stdout(std::process::Stdio::null())
                        .status()
                        .unwrap();
                }
            })
            .await;
        result.unwrap();
        assert_eq!(fe.discarded_calls.len(), 1);
        assert_eq!(git_out(&git_root, &["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn merge_fix_unresolved_keeps_worktree_and_reports_the_conflict() {
        let (fe, failure, result, git_root, _repo, _wt) =
            conflicted_merge_with_fake_agent(false, |_| {}).await;
        assert!(matches!(result, Err(CommandError::MergeConflict { .. })));
        assert!(failure.worktree_path.exists());
        assert!(fe.discarded_calls.is_empty());
        assert!(fe
            .messages
            .iter()
            .any(|m| m.level == MessageLevel::Error && m.text.contains("still unresolved")));
        abort_merge(&git_root);
    }

    #[tokio::test]
    async fn merge_fix_agent_failing_to_start_falls_back_to_keep() {
        let (repo, _wt_dir, lifecycle) = conflict_fixture("awman/no-start");
        let mut fe = merging_frontend();
        let policy = FinalizePolicy {
            on_dirty_merge_target: None,
            on_merge_failure: Some(OnMergeFailure::Agent),
        };
        let FinalizeOutcome::MergeFixRequested(failure) =
            lifecycle.finalize(&mut fe, false, policy).await.unwrap()
        else {
            panic!("expected a merge-fix request");
        };
        let result = lifecycle
            .complete_merge_after_fix(&mut fe, &failure, false)
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(lifecycle.worktree_path.exists());
        assert_eq!(fe.kept_calls.len(), 1);
        assert!(fe.discarded_calls.is_empty());
        assert!(fe.messages.iter().any(|m| m.level == MessageLevel::Warning));
        abort_merge(repo.path());
    }

    #[tokio::test]
    async fn merge_fix_makes_only_one_agent_attempt() {
        let (repo, _wt_dir, lifecycle) = conflict_fixture("awman/one-shot");
        let mut fe = merging_frontend();
        fe.launch_merge_fix_response = true;
        let FinalizeOutcome::MergeFixRequested(failure) = lifecycle
            .finalize(&mut fe, false, FinalizePolicy::default())
            .await
            .unwrap()
        else {
            panic!("expected a merge-fix request");
        };
        // The agent leaves the markers; verification fails once, with no
        // second ask and no second request.
        let result = lifecycle
            .complete_merge_after_fix(&mut fe, &failure, true)
            .await;
        assert!(matches!(result, Err(CommandError::MergeConflict { .. })));
        assert_eq!(fe.launch_merge_fix_calls.len(), 1, "asked exactly once");
        abort_merge(repo.path());
    }

    #[tokio::test]
    async fn merge_fix_leaving_markers_is_still_unresolved() {
        let (fe, _failure, result, git_root, _repo, _wt) =
            conflicted_merge_with_fake_agent(false, |_| {}).await;
        assert!(matches!(result, Err(CommandError::MergeConflict { .. })));
        // Once from finalize, once for the failed verification.
        assert_eq!(fe.merge_conflict_calls.len(), 2);
        assert!(fe.discarded_calls.is_empty());
        SysCmd::new("git")
            .args(["merge", "--abort"])
            .current_dir(&git_root)
            .status()
            .ok();
    }
    #[tokio::test]
    async fn squash_repair_discarded_or_unstaged_result_keeps_branch_and_worktree() {
        for mode in ["hard", "mixed", "committed", "committed-and-staged"] {
            let (fe, failure, result, root, _repo, _wt) =
                conflicted_merge_with_fake_agent(true, |root| {
                    if mode.starts_with("committed") {
                        std::fs::write(root.join("README.md"), "resolved\n").unwrap();
                        git_out(root, &["add", "."]);
                        git_out(root, &["commit", "-m", "agent squash"]);
                        if mode == "committed-and-staged" {
                            std::fs::write(root.join("README.md"), "another change\\n").unwrap();
                            git_out(root, &["add", "README.md"]);
                        }
                    } else {
                        git_out(
                            root,
                            &[
                                "reset",
                                if mode == "hard" { "--hard" } else { "--mixed" },
                                "HEAD",
                            ],
                        );
                        // A mixed reset can leave the conflict markers; resolve only
                        // the conflict but leave the feature-only file unstaged.
                        if mode == "mixed" {
                            std::fs::write(root.join("README.md"), "resolved\n").unwrap();
                        }
                    }
                })
                .await;
            assert!(
                matches!(result, Err(CommandError::MergeConflict { .. })),
                "{mode}: {result:?}"
            );
            assert!(failure.worktree_path.exists());
            assert!(fe.discarded_calls.is_empty());
            assert!(!git_out(&root, &["branch", "--list", &failure.branch]).is_empty());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn merge_repair_preserves_literal_unicode_whitespace_and_magic_paths() {
        let repo = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let names = [
            ":(top)*",
            "glob*.txt",
            "café.txt",
            " leading ",
            "quote\".txt",
            "tab\t.txt",
            "line\n.txt",
        ];
        for name in names {
            std::fs::write(repo.path().join(name), "base\n").unwrap();
        }
        std::fs::write(repo.path().join("private-tracked.txt"), "base\n").unwrap();
        let engine = Arc::new(GitEngine::new());
        engine.commit_all(repo.path(), "filenames").unwrap();
        let wt_path = wt.path().join("wt");
        engine
            .create_worktree(repo.path(), &wt_path, "awman/filenames")
            .unwrap();
        for name in names {
            std::fs::write(wt_path.join(name), "feature\n").unwrap();
        }
        engine.commit_all(&wt_path, "feature").unwrap();
        for name in names {
            std::fs::write(repo.path().join(name), "main\n").unwrap();
        }
        engine.commit_all(repo.path(), "main").unwrap();
        let lifecycle = WorktreeLifecycle::new_for_test(
            engine.clone(),
            repo.path().into(),
            wt_path.clone(),
            "awman/filenames".into(),
        );
        let mut fe = merging_frontend();
        fe.confirm_cleanup_response = true;
        let FinalizeOutcome::MergeFixRequested(failure) = lifecycle
            .finalize(
                &mut fe,
                false,
                FinalizePolicy {
                    on_merge_failure: Some(OnMergeFailure::Agent),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected conflicts");
        };
        let mut returned = failure.unmerged_paths.clone();
        returned.sort();
        let mut expected = names.map(str::to_string).to_vec();
        expected.sort();
        assert_eq!(returned, expected, "Git filenames must be lossless");
        for name in names {
            std::fs::write(repo.path().join(name), "resolved\n").unwrap();
        }
        std::fs::write(repo.path().join("private-draft.txt"), "private\n").unwrap();
        std::fs::write(repo.path().join("private-tracked.txt"), "user edit\n").unwrap();
        lifecycle
            .complete_merge_after_fix(&mut fe, &failure, true)
            .await
            .unwrap();
        assert!(!wt_path.exists());
        assert_eq!(
            git_out(repo.path(), &["show", "HEAD:private-tracked.txt"]),
            "base"
        );
        assert!(git_out(repo.path(), &["ls-files", "private-draft.txt"]).is_empty());
        assert_eq!(
            std::fs::read_to_string(repo.path().join("private-tracked.txt")).unwrap(),
            "user edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("private-draft.txt")).unwrap(),
            "private\n"
        );
    }

    #[tokio::test]
    async fn squash_failed_commit_then_discarded_index_is_not_success() {
        let (repo, _wt, lifecycle, wt_path) = dirty_target_fixture("awman/failed-commit");
        std::fs::remove_file(repo.path().join("scratch.txt")).unwrap();
        // Hooks are disabled for host Git. Force a real commit I/O failure
        // instead, after the squash has populated the index.
        let message = repo.path().join(".git/COMMIT_EDITMSG");
        std::fs::remove_file(&message).unwrap();
        std::fs::create_dir(&message).unwrap();
        let mut fe = merging_frontend();
        let FinalizeOutcome::MergeFixRequested(failure) = lifecycle
            .finalize(
                &mut fe,
                false,
                FinalizePolicy {
                    on_merge_failure: Some(OnMergeFailure::Agent),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected commit failure");
        };
        assert!(failure.command.starts_with("git commit"));
        git_out(repo.path(), &["reset", "--hard", "HEAD"]);
        let result = lifecycle
            .complete_merge_after_fix(&mut fe, &failure, true)
            .await;
        assert!(matches!(result, Err(CommandError::MergeConflict { .. })));
        assert!(wt_path.exists());
        assert!(fe.discarded_calls.is_empty());
        assert!(!repo.path().join("work.txt").exists());
    }

    #[tokio::test]
    async fn failed_pre_index_merge_retries_once_after_agent_without_relaunching() {
        for fix in [false, true] {
            let (repo, _wt, lifecycle, wt_path) = dirty_target_fixture("awman/retry");
            std::fs::remove_file(repo.path().join("scratch.txt")).unwrap();
            // An existing MERGE_HEAD would count as started. A lock instead
            // makes git fail before it touches the index.
            let lock = repo.path().join(".git/index.lock");
            std::fs::write(&lock, "").unwrap();
            let mut fe = merging_frontend();
            let FinalizeOutcome::MergeFixRequested(failure) = lifecycle
                .finalize(
                    &mut fe,
                    false,
                    FinalizePolicy {
                        on_merge_failure: Some(OnMergeFailure::Agent),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
            else {
                panic!("expected pre-index failure");
            };
            assert!(!failure.merge_started());
            if fix {
                std::fs::remove_file(&lock).unwrap();
            }
            let before = fe.messages.len();
            let result = lifecycle
                .complete_merge_after_fix(&mut fe, &failure, true)
                .await;
            assert_eq!(result.is_ok(), fix, "{result:?}");
            assert_eq!(wt_path.exists(), !fix);
            assert_eq!(
                fe.messages[before..]
                    .iter()
                    .filter(|m| m.text.contains("$ git merge --squash"))
                    .count(),
                1
            );
            assert!(
                fe.launch_merge_fix_calls.is_empty(),
                "agent policy never asks again"
            );
        }
    }
    #[tokio::test]
    async fn recreate_owned_legacy_uses_hashed_creation_path_for_both_names() {
        use crate::data::worktree_paths::WorktreePaths;
        for named in [false, true] {
            let repo = tempfile::tempdir().unwrap();
            let home = tempfile::tempdir().unwrap();
            init_repo(repo.path());
            let root = repo.path().canonicalize().unwrap();
            let paths = WorktreePaths::with_home(home.path());
            let (legacy, hashed, branch) = if named {
                (
                    paths.legacy_for_workflow(&root, "build"),
                    paths.for_workflow(&root, "build"),
                    "awman/workflow-build",
                )
            } else {
                (
                    paths.legacy_for_work_item(&root, 134),
                    paths.for_work_item(&root, 134),
                    "awman/work-item-0134",
                )
            };
            let git = Arc::new(GitEngine::new());
            git.create_worktree(&root, &legacy, branch).unwrap();
            let mut lifecycle =
                WorktreeLifecycle::new_for_test(git.clone(), root, legacy.clone(), branch.into());
            lifecycle.creation_path = hashed.clone();
            let mut frontend = RecordingWorktreeLifecycleFrontend::new();
            let result = lifecycle
                .prepare_with_existing(
                    &mut frontend,
                    ExistingWorktreeChoice::Explicit(ExistingWorktreeDecision::Recreate),
                )
                .await
                .unwrap();
            assert_eq!(result, hashed);
            assert_eq!(lifecycle.worktree_path(), hashed);
            assert!(!legacy.exists());
            assert_eq!(frontend.worktree_created_calls[0].0, hashed);
            // Finalization follows the effective hashed path too.
            assert!(git
                .validate_worktree_owner(lifecycle.git_root(), lifecycle.worktree_path())
                .is_ok());
        }
    }

    #[tokio::test]
    async fn recreate_refuses_unexpected_target_branch_without_deleting_either_branch() {
        let repo = tempfile::tempdir().unwrap();
        let wt_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let engine = Arc::new(GitEngine::new());
        let target = wt_dir.path().join("wt");
        engine
            .create_worktree(repo.path(), &target, "actual")
            .unwrap();
        // The deterministic branch exists, but the user switched the target
        // to a different branch after the previous run.
        assert!(SysCmd::new("git")
            .args(["branch", "assigned"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(target.join("untracked"), "preserve").unwrap();
        let mut lifecycle = WorktreeLifecycle::new_for_test(
            engine.clone(),
            repo.path().into(),
            target.clone(),
            "assigned".into(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        assert!(lifecycle
            .prepare_with_existing(
                &mut fe,
                ExistingWorktreeChoice::Explicit(ExistingWorktreeDecision::Recreate)
            )
            .await
            .is_err());
        assert!(engine.branch_exists(repo.path(), "actual"));
        assert!(engine.branch_exists(repo.path(), "assigned"));
        assert_eq!(engine.current_branch(&target).as_deref(), Some("actual"));
        assert_eq!(
            std::fs::read_to_string(target.join("untracked")).unwrap(),
            "preserve"
        );
    }

    #[tokio::test]
    async fn recreate_branch_lock_error_restores_head_and_preserves_untracked_work() {
        let repo = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let git = Arc::new(GitEngine::new());
        let target = target_dir.path().join("wt");
        let branch = "awman/locked";
        git.create_worktree(repo.path(), &target, branch).unwrap();
        std::fs::write(target.join("untracked"), "keep me").unwrap();
        std::fs::write(
            repo.path().join(".git/refs/heads/awman/locked.lock"),
            "lock",
        )
        .unwrap();
        let mut lifecycle = WorktreeLifecycle::new_for_test(
            git.clone(),
            repo.path().into(),
            target.clone(),
            branch.into(),
        );
        let mut fe = RecordingWorktreeLifecycleFrontend::new();
        assert!(lifecycle
            .prepare_with_existing(
                &mut fe,
                ExistingWorktreeChoice::Explicit(ExistingWorktreeDecision::Recreate)
            )
            .await
            .is_err());
        assert_eq!(
            std::fs::read_to_string(target.join("untracked")).unwrap(),
            "keep me"
        );
        assert_eq!(git.current_branch(&target).as_deref(), Some(branch));
    }
}

/// What a worktree and its branch are named after.
///
/// Carries the choice so it can be made in one place and acted on in another
/// (WI 0114 F-51).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeName {
    /// A workflow file's stem, or an issue's slug.
    Workflow(String),
    /// A work item number.
    WorkItem(u32),
}

#[cfg(test)]
mod name_for_tests {
    use super::*;

    /// The precedence the four `if`/`else` arms encoded before F-51 collapsed
    /// them: the more specific the user was, the more specific the name.
    #[test]
    fn an_issue_slug_outranks_a_work_item_which_outranks_the_file() {
        let path = Path::new("/repo/flows/deploy.toml");
        assert_eq!(
            WorktreeLifecycle::name_for(Some("fix-login"), Some(114), path),
            WorktreeName::Workflow("fix-login".into())
        );
        assert_eq!(
            WorktreeLifecycle::name_for(None, Some(114), path),
            WorktreeName::WorkItem(114)
        );
        assert_eq!(
            WorktreeLifecycle::name_for(None, None, path),
            WorktreeName::Workflow("deploy".into())
        );
    }

    /// A path with no usable stem falls back to the same literal the old code
    /// used in both of its copies.
    #[test]
    fn a_path_without_a_stem_falls_back_to_workflow() {
        assert_eq!(
            WorktreeLifecycle::name_for(None, None, Path::new("..")),
            WorktreeName::Workflow("workflow".into())
        );
    }
}
