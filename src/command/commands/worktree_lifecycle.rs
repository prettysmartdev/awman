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
use crate::data::config::worktree::{OnDirtyMergeTarget, OnMergeFailure};
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
        let creation_path = crate::data::worktree_paths::WorktreePaths::from_env(git_engine.env())?
            .for_workflow(
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
        let creation_path = crate::data::worktree_paths::WorktreePaths::from_env(git_engine.env())?
            .for_work_item(
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
