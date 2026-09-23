//! Headless answer policy — the one place awman decides what a question gets
//! answered with when there is nobody to ask.
//!
//! Three hosts run commands with no operator attached: the API server, the
//! squad daemon, and the CLI with a piped stdin (or `--non-interactive`).
//! Before WI 0114 F-13 each of them carried its own answers, inline in its
//! Layer 3 frontend, and they had drifted: the same `exec workflow` squash-
//! merged under the API, left a branch alone under squad, and asked for
//! credentials differently again under a piped CLI.
//!
//! Decision Q5 records that the differences are **deliberate per-host policy**,
//! not drift, so the fix is not one table but named profiles in Layer 2. Each
//! frontend's `ask_*` body becomes a one-line delegation to the profile it was
//! constructed with, and this module doc is the single place the policies are
//! compared.
//!
//! # The answer table
//!
//! `d` = `default_available`, `e` = `prompt.had_error`, `m` =
//! `suggested_message`, `sp` = [`WorkflowResumePrompt::resume_from_stop_point`].
//!
//! | Question | [`api`](HeadlessDefaults::api) | [`squad`](HeadlessDefaults::squad) | [`cli`](HeadlessDefaults::cli) |
//! |---|---|---|---|
//! | [`mount_scope`](HeadlessDefaults::mount_scope) | `MountGitRoot` | *the task's captured scope* | `MountGitRoot` |
//! | [`agent_setup`](HeadlessDefaults::agent_setup) | `d ? Setup : Abort` | `Setup` | `Setup` |
//! | [`agent_auth_consent`](HeadlessDefaults::agent_auth_consent) | `Accept` | `Accept` | `DeclineOnce` |
//! | [`confirm_resume`](HeadlessDefaults::confirm_resume) | `true` | `false` | `false` |
//! | [`workflow_next_action`](HeadlessDefaults::workflow_next_action) | `can_launch_next ? LaunchNext : Abort` | `LaunchNext` | `LaunchNext` |
//! | [`yolo_tick`](HeadlessDefaults::yolo_tick) | `Continue` | `Continue` | `Continue` |
//! | [`pre_worktree_uncommitted_files`](HeadlessDefaults::pre_worktree_uncommitted_files) | `Commit { message: m }` | `UseLastCommit` | `UseLastCommit` |
//! | [`existing_worktree`](HeadlessDefaults::existing_worktree) | `Resume` | `Resume` | `Resume` |
//! | [`post_workflow_action`](HeadlessDefaults::post_workflow_action) | `e ? Keep : Merge` | `Keep` | `Keep` |
//! | [`worktree_commit_before_merge`](HeadlessDefaults::worktree_commit_before_merge) | `Some(m)` | `None` | `None` |
//! | [`merge_mode`](HeadlessDefaults::merge_mode) | `Squash` | `LeaveBranch` | `LeaveBranch` |
//! | [`confirm_worktree_cleanup`](HeadlessDefaults::confirm_worktree_cleanup) | `true` | `false` | `false` |
//! | [`workflow_resume`](HeadlessDefaults::workflow_resume) | `sp` | `Fresh` | `sp` |
//! | [`acp_permission`](HeadlessDefaults::acp_permission) | `Cancelled` | *(unused)* | `approve(options)` |
//! | [`replace_aspec`](HeadlessDefaults::replace_aspec) | `false` | *(unused)* | `false` |
//! | [`run_audit`](HeadlessDefaults::run_audit) | `false` | *(unused)* | `false` |
//! | [`work_items_setup`](HeadlessDefaults::work_items_setup) | `None` | *(unused)* | `None` |
//! | [`dockerfile_setup`](HeadlessDefaults::dockerfile_setup) | `CreateNew` | *(unused)* | `CreateNew` |
//! | [`create_dockerfile`](HeadlessDefaults::create_dockerfile) | `true` | *(unused)* | `true` |
//! | [`run_audit_on_template`](HeadlessDefaults::run_audit_on_template) | `false` | *(unused)* | `false` |
//! | [`confirm_deletion`](HeadlessDefaults::confirm_deletion) | `false` | *(unused)* | *(not delegated — see below)* |
//! | [`delete_task_dir`](HeadlessDefaults::delete_task_dir) | `false` | `false` | `false` |
//! | [`spec_kind`](HeadlessDefaults::spec_kind) | `Task` | `Task` | `Task` |
//!
//! Every row reproduces what the three frontends answered before F-13. The
//! table-driven test at the bottom of this file is the contract: it asserts
//! each cell, so changing a policy means changing the test, which means the
//! change is deliberate and reviewable.
//!
//! ## Where the profiles genuinely differ
//!
//! Nine rows are not uniform, and each is a per-host policy worth stating:
//!
//! - **`agent_setup`.** Only the API refuses to proceed when the default agent
//!   is unavailable. An HTTP caller named an agent and gets an error rather
//!   than a surprise substitution; squad and the CLI set the agent up.
//! - **`agent_auth_consent`.** The CLI is the only host with a *human user's*
//!   host credentials at stake and no way to ask, so it declines. The API and
//!   squad daemon run with credentials provisioned for exactly that purpose.
//! - **`confirm_resume`, `workflow_resume`.** The API preserves a saved run
//!   (an HTTP retry should not silently re-run completed steps). Squad starts
//!   fresh, because each scheduled evaluation is its own run and picking up a
//!   stale one would skip steps this run is meant to perform. The CLI splits
//!   the two: it will not resume across a *changed workflow file*
//!   (`confirm_resume`) but will resume an unchanged one (`workflow_resume`).
//! - **The four worktree rows.** The API is the only host that commits, merges
//!   and cleans up on the user's behalf — it is driving a detached run whose
//!   result is fetched over HTTP, so leaving branches around strands the work.
//!   Squad and the CLI never touch a user's working tree or branches
//!   unattended.
//! - **`mount_scope`.** Squad answers with the scope captured when the task was
//!   created rather than a constant; that is why [`HeadlessDefaults::squad`]
//!   takes it as an argument.
//! - **`acp_permission`.** The API fails closed (it cannot ask a human, so it
//!   must not approve a tool call). A piped CLI approves, because reaching this
//!   path at all means the operator ran without `--yolo`/`--auto` on a
//!   non-TTY. This is the sharpest divergence in the table and the one most
//!   worth a developer's attention.
//!
//! ## Three profiles, not two
//!
//! Decision Q5 named two (`api` and `squad`). Tabulating the CLI's non-TTY
//! answers first — as `00-plan.md` requires before adding one — shows it
//! matches neither: it agrees with `squad` everywhere except `mount_scope`,
//! `agent_auth_consent` and `workflow_resume`. Folding it into `squad` would
//! silently change CLI behaviour on all three, so it is a third profile. See
//! `assumptions.md` (F-13) for the developer-facing note.
//!
//! ## What is deliberately not here
//!
//! - **`confirm_deletion`** (`awman clean`). The API answers `false` from an
//!   impl that never runs (`clean` is `api_allowed: false` at the catalogue).
//!   The CLI does *not* default: with no TTY and no `--yes` it raises
//!   `CommandError::InteractiveInputUnavailable`, refusing to guess about
//!   deletion. That is a gate, not an answer, so only the API delegates.
//! - **`supports_interactive_recovery`** and the PTY/IO gates. They report a
//!   host *capability*, not a decision, and stay with the frontend.

use crate::command::commands::agent_auth::AgentAuthDecision;
use crate::command::commands::agent_setup::AgentSetupDecision;
use crate::command::commands::exec_workflow::{WorkflowResumeDecision, WorkflowResumePrompt};
use crate::command::commands::mount_scope::MountScopeDecision;
use crate::command::commands::specs::WorkItemKind;
use crate::command::commands::worktree_lifecycle::{
    ExistingWorktreeDecision, PostWorkflowWorktreeAction, PostWorkflowWorktreePrompt,
    PreWorktreeDecision, WorktreeMergeMode,
};
use crate::data::config::repo::WorkItemsConfig;
use crate::engine::acp::protocol::{PermissionDecision, PermissionOption};
use crate::engine::init::frontend::DockerfileSetupDecision;
use crate::engine::workflow::actions::{AvailableActions, NextAction, YoloTickOutcome};

/// Which host's policy a [`HeadlessDefaults`] carries. Private: a profile is
/// chosen by calling the constructor named after the host, never by passing a
/// tag around.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    Api,
    Squad,
    Cli,
}

/// The headless answer policy for one host. Built by [`HeadlessDefaults::api`],
/// [`HeadlessDefaults::squad`] or [`HeadlessDefaults::cli`]; see the module doc
/// for the full answer table.
#[derive(Debug, Clone)]
pub struct HeadlessDefaults {
    profile: Profile,
    /// Squad's `mount_scope` answer. The other two profiles answer with a
    /// constant, so this is set to that constant for them and the field stays
    /// a single code path.
    mount_scope: MountScopeDecision,
}

impl HeadlessDefaults {
    /// The API server's policy: preserve saved work, and finish the job —
    /// commit, squash-merge and clean up — because nobody will come back to a
    /// branch left behind by a detached HTTP run.
    pub fn api() -> Self {
        Self {
            profile: Profile::Api,
            mount_scope: MountScopeDecision::MountGitRoot,
        }
    }

    /// The squad daemon's policy: never touch the user's working tree or
    /// branches, and start each scheduled evaluation fresh.
    ///
    /// `mount_scope` is the scope captured when the task was created, which
    /// this profile returns verbatim — squad is the one host whose answer is
    /// task data rather than a constant.
    pub fn squad(mount_scope: MountScopeDecision) -> Self {
        Self {
            profile: Profile::Squad,
            mount_scope,
        }
    }

    /// The CLI's policy when stdin is not a TTY or `--non-interactive` is set:
    /// squad's caution about the working tree, but the user's own host
    /// credentials are never injected without them saying so.
    pub fn cli() -> Self {
        Self {
            profile: Profile::Cli,
            mount_scope: MountScopeDecision::MountGitRoot,
        }
    }

    // ─── Mount scope ────────────────────────────────────────────────────────

    /// Answer for `MountScopeFrontend::ask_mount_scope`.
    pub fn mount_scope(&self) -> MountScopeDecision {
        self.mount_scope
    }

    // ─── Agent setup and auth ───────────────────────────────────────────────

    /// Answer for `AgentSetupFrontend::ask_agent_setup`.
    ///
    /// `default_available` is consulted by the API profile alone: an HTTP
    /// caller that named an agent gets an error rather than a silent
    /// substitution when the default is not there to fall back to.
    pub fn agent_setup(&self, default_available: bool) -> AgentSetupDecision {
        match self.profile {
            Profile::Api if !default_available => AgentSetupDecision::Abort,
            _ => AgentSetupDecision::Setup,
        }
    }

    /// Answer for `AgentAuthFrontend::ask_agent_auth_consent`.
    pub fn agent_auth_consent(&self) -> AgentAuthDecision {
        match self.profile {
            Profile::Api | Profile::Squad => AgentAuthDecision::Accept,
            Profile::Cli => AgentAuthDecision::DeclineOnce,
        }
    }

    // ─── Workflow ───────────────────────────────────────────────────────────

    /// Answer for `WorkflowFrontend::confirm_resume` — whether to resume a
    /// saved run whose workflow file changed underneath it.
    pub fn confirm_resume(&self) -> bool {
        matches!(self.profile, Profile::Api)
    }

    /// Answer for `WorkflowFrontend::show_workflow_control_board`.
    pub fn workflow_next_action(&self, available: &AvailableActions) -> NextAction {
        match self.profile {
            Profile::Api if !available.can_launch_next => NextAction::Abort,
            _ => NextAction::LaunchNext,
        }
    }

    /// Answer for `WorkflowFrontend::yolo_countdown_tick`. No headless host has
    /// a key to interrupt the countdown with, so every profile lets it run.
    pub fn yolo_tick(&self) -> YoloTickOutcome {
        YoloTickOutcome::Continue
    }

    /// Answer for `ExecWorkflowCommandFrontend::ask_workflow_resume`.
    pub fn workflow_resume(&self, prompt: &WorkflowResumePrompt) -> WorkflowResumeDecision {
        match self.profile {
            Profile::Squad => WorkflowResumeDecision::Fresh,
            Profile::Api | Profile::Cli => prompt.resume_from_stop_point(),
        }
    }

    // ─── Worktree lifecycle ─────────────────────────────────────────────────

    /// Answer for `WorktreeLifecycleFrontend::ask_pre_worktree_uncommitted_files`.
    pub fn pre_worktree_uncommitted_files(&self, suggested_message: &str) -> PreWorktreeDecision {
        match self.profile {
            Profile::Api => PreWorktreeDecision::Commit {
                message: suggested_message.to_string(),
            },
            Profile::Squad | Profile::Cli => PreWorktreeDecision::UseLastCommit,
        }
    }

    /// Answer for `WorktreeLifecycleFrontend::ask_existing_worktree`.
    pub fn existing_worktree(&self) -> ExistingWorktreeDecision {
        ExistingWorktreeDecision::Resume
    }

    /// Answer for `WorktreeLifecycleFrontend::ask_post_workflow_action`.
    ///
    /// The API merges a clean run's worktree back and keeps a failed one for
    /// inspection; the other two hosts always keep it.
    pub fn post_workflow_action(
        &self,
        prompt: &PostWorkflowWorktreePrompt,
    ) -> PostWorkflowWorktreeAction {
        match self.profile {
            Profile::Api if !prompt.had_error => PostWorkflowWorktreeAction::Merge,
            _ => PostWorkflowWorktreeAction::Keep,
        }
    }

    /// Answer for `WorktreeLifecycleFrontend::ask_worktree_commit_before_merge`.
    pub fn worktree_commit_before_merge(&self, suggested_message: &str) -> Option<String> {
        match self.profile {
            Profile::Api => Some(suggested_message.to_string()),
            Profile::Squad | Profile::Cli => None,
        }
    }

    /// Answer for `WorktreeLifecycleFrontend::ask_merge_mode`.
    pub fn merge_mode(&self) -> WorktreeMergeMode {
        match self.profile {
            Profile::Api => WorktreeMergeMode::Squash,
            Profile::Squad | Profile::Cli => WorktreeMergeMode::LeaveBranch,
        }
    }

    /// Answer for `WorktreeLifecycleFrontend::confirm_worktree_cleanup`.
    pub fn confirm_worktree_cleanup(&self) -> bool {
        matches!(self.profile, Profile::Api)
    }

    // ─── ACP ────────────────────────────────────────────────────────────────

    /// Answer for `AcpFrontend::request_permission`.
    ///
    /// The API fails closed: it has no way to ask a human, so it must not
    /// approve a tool call. A piped CLI approves, because `AcpSession` only
    /// consults the frontend when neither `--yolo` nor `--auto` is set, so the
    /// operator has already chosen to run this way.
    /// [`PermissionDecision::approve`] still refuses to pick anything the agent
    /// did not mark as an allow outcome.
    pub fn acp_permission(&self, options: &[PermissionOption]) -> PermissionDecision {
        match self.profile {
            Profile::Api | Profile::Squad => PermissionDecision::Cancelled,
            Profile::Cli => PermissionDecision::approve(options),
        }
    }

    // ─── Init and ready ─────────────────────────────────────────────────────
    //
    // Uniform across every profile: a headless host never replaces a user's
    // aspec folder, never spends a container run on an audit nobody asked for,
    // and takes the built-in Dockerfile template when there is no Dockerfile.

    /// Answer for `InitFrontend::ask_replace_aspec`.
    pub fn replace_aspec(&self) -> bool {
        false
    }

    /// Answer for `InitFrontend::ask_run_audit`.
    pub fn run_audit(&self) -> bool {
        false
    }

    /// Answer for `InitFrontend::ask_work_items_setup`.
    pub fn work_items_setup(&self) -> Option<WorkItemsConfig> {
        None
    }

    /// Answer for `InitFrontend::ask_dockerfile_setup`.
    pub fn dockerfile_setup(&self) -> DockerfileSetupDecision {
        DockerfileSetupDecision::CreateNew
    }

    /// Answer for `ReadyFrontend::ask_create_dockerfile`.
    pub fn create_dockerfile(&self) -> bool {
        true
    }

    /// Answer for `ReadyFrontend::ask_run_audit_on_template`.
    pub fn run_audit_on_template(&self) -> bool {
        false
    }

    // ─── Clean ──────────────────────────────────────────────────────────────

    /// Answer for `CleanCommandFrontend::confirm_deletion`.
    ///
    /// The CLI does not use this: with no TTY and no `--yes` it raises
    /// `CommandError::InteractiveInputUnavailable` rather than guessing about
    /// deletion. See the module doc.
    pub fn confirm_deletion(&self) -> bool {
        false
    }

    // ─── Squad and specs ────────────────────────────────────────────────────

    /// Answer for `SquadCommandFrontend::ask_delete_task_dir`.
    ///
    /// Uniform: `squad remove` drops the task, and a headless host never also
    /// deletes the persistent directory a later run might still want. An
    /// operator who means to delete it passes `--yes`.
    pub fn delete_task_dir(&self) -> bool {
        false
    }

    /// Answer for `SpecsCommandFrontend::ask_spec_kind`.
    ///
    /// Uniform: `Task` is the neutral kind, and nothing in a headless
    /// invocation distinguishes a feature from a bug.
    pub fn spec_kind(&self) -> WorkItemKind {
        WorkItemKind::Task
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::exec_workflow::WorkflowResumeStep;

    /// One row of the module doc's answer table, in the same order.
    ///
    /// Anything that varies across profiles gets a column here. Rows that are
    /// uniform by construction (`existing_worktree`, `yolo_tick`, the init and
    /// ready answers, `confirm_deletion`) are asserted once in
    /// [`uniform_answers_are_the_same_for_every_profile`].
    struct Row {
        /// The question, as the module doc names it.
        question: &'static str,
        api: &'static str,
        squad: &'static str,
        cli: &'static str,
    }

    /// The recorded contract. Changing a cell here is the only way to change a
    /// headless policy, which is the point: it cannot happen as a side effect
    /// of editing a frontend.
    const TABLE: &[Row] = &[
        Row {
            question: "mount_scope",
            api: "MountGitRoot",
            squad: "MountCurrentDirOnly",
            cli: "MountGitRoot",
        },
        Row {
            question: "agent_setup(default_available = true)",
            api: "Setup",
            squad: "Setup",
            cli: "Setup",
        },
        Row {
            question: "agent_setup(default_available = false)",
            api: "Abort",
            squad: "Setup",
            cli: "Setup",
        },
        Row {
            question: "agent_auth_consent",
            api: "Accept",
            squad: "Accept",
            cli: "DeclineOnce",
        },
        Row {
            question: "confirm_resume",
            api: "true",
            squad: "false",
            cli: "false",
        },
        Row {
            question: "workflow_next_action(can_launch_next = true)",
            api: "LaunchNext",
            squad: "LaunchNext",
            cli: "LaunchNext",
        },
        Row {
            question: "workflow_next_action(can_launch_next = false)",
            api: "Abort",
            squad: "LaunchNext",
            cli: "LaunchNext",
        },
        Row {
            question: "pre_worktree_uncommitted_files",
            api: "Commit(suggested)",
            squad: "UseLastCommit",
            cli: "UseLastCommit",
        },
        Row {
            question: "post_workflow_action(had_error = false)",
            api: "Merge",
            squad: "Keep",
            cli: "Keep",
        },
        Row {
            question: "post_workflow_action(had_error = true)",
            api: "Keep",
            squad: "Keep",
            cli: "Keep",
        },
        Row {
            question: "worktree_commit_before_merge",
            api: "Some(suggested)",
            squad: "None",
            cli: "None",
        },
        Row {
            question: "merge_mode",
            api: "Squash",
            squad: "LeaveBranch",
            cli: "LeaveBranch",
        },
        Row {
            question: "confirm_worktree_cleanup",
            api: "true",
            squad: "false",
            cli: "false",
        },
        Row {
            question: "workflow_resume(has a stop point)",
            api: "ResumeFrom(build)",
            squad: "Fresh",
            cli: "ResumeFrom(build)",
        },
        Row {
            question: "workflow_resume(no stop point)",
            api: "Fresh",
            squad: "Fresh",
            cli: "Fresh",
        },
        Row {
            question: "acp_permission",
            api: "Cancelled",
            squad: "Cancelled",
            cli: "Selected(ok)",
        },
    ];

    /// The squad profile's `mount_scope` is task data, so the table records
    /// what it does with a *non-default* value: return it verbatim.
    const SQUAD_TASK_SCOPE: MountScopeDecision = MountScopeDecision::MountCurrentDirOnly;

    fn available(can_launch_next: bool) -> AvailableActions {
        AvailableActions {
            can_launch_next,
            ..Default::default()
        }
    }

    fn post_workflow_prompt(had_error: bool) -> PostWorkflowWorktreePrompt {
        PostWorkflowWorktreePrompt {
            branch: "awman/work-item-0114".to_string(),
            target_branch: "main".to_string(),
            had_error,
            title: "title".to_string(),
            body: "body".to_string(),
            merge_label: "merge".to_string(),
            discard_label: "discard".to_string(),
            keep_label: "keep".to_string(),
        }
    }

    fn resume_prompt(start_points: Vec<&str>) -> WorkflowResumePrompt {
        WorkflowResumePrompt::new(
            "ci".to_string(),
            None,
            None,
            false,
            1,
            3,
            start_points
                .into_iter()
                .map(|name| WorkflowResumeStep {
                    name: name.to_string(),
                    role: "the step that failed".to_string(),
                })
                .collect(),
        )
    }

    fn permission_option(option_id: &str, kind: &str) -> PermissionOption {
        PermissionOption {
            option_id: option_id.to_string(),
            name: option_id.to_string(),
            kind: kind.to_string(),
        }
    }

    fn permission_options() -> Vec<PermissionOption> {
        vec![permission_option("ok", "allow_once")]
    }

    /// Render every answer of one profile as the table spells it.
    fn answers(d: &HeadlessDefaults) -> Vec<(&'static str, String)> {
        let opts = permission_options();
        vec![
            ("mount_scope", format!("{:?}", d.mount_scope())),
            (
                "agent_setup(default_available = true)",
                format!("{:?}", d.agent_setup(true)),
            ),
            (
                "agent_setup(default_available = false)",
                format!("{:?}", d.agent_setup(false)),
            ),
            (
                "agent_auth_consent",
                format!("{:?}", d.agent_auth_consent()),
            ),
            ("confirm_resume", d.confirm_resume().to_string()),
            (
                "workflow_next_action(can_launch_next = true)",
                format!("{:?}", d.workflow_next_action(&available(true))),
            ),
            (
                "workflow_next_action(can_launch_next = false)",
                format!("{:?}", d.workflow_next_action(&available(false))),
            ),
            (
                "pre_worktree_uncommitted_files",
                match d.pre_worktree_uncommitted_files("suggested") {
                    PreWorktreeDecision::Commit { message } => format!("Commit({message})"),
                    other => format!("{other:?}"),
                },
            ),
            (
                "post_workflow_action(had_error = false)",
                format!("{:?}", d.post_workflow_action(&post_workflow_prompt(false))),
            ),
            (
                "post_workflow_action(had_error = true)",
                format!("{:?}", d.post_workflow_action(&post_workflow_prompt(true))),
            ),
            (
                "worktree_commit_before_merge",
                match d.worktree_commit_before_merge("suggested") {
                    Some(m) => format!("Some({m})"),
                    None => "None".to_string(),
                },
            ),
            ("merge_mode", format!("{:?}", d.merge_mode())),
            (
                "confirm_worktree_cleanup",
                d.confirm_worktree_cleanup().to_string(),
            ),
            (
                "workflow_resume(has a stop point)",
                match d.workflow_resume(&resume_prompt(vec!["build"])) {
                    WorkflowResumeDecision::ResumeFrom(name) => format!("ResumeFrom({name})"),
                    other => format!("{other:?}"),
                },
            ),
            (
                "workflow_resume(no stop point)",
                match d.workflow_resume(&resume_prompt(vec![])) {
                    WorkflowResumeDecision::ResumeFrom(name) => format!("ResumeFrom({name})"),
                    other => format!("{other:?}"),
                },
            ),
            (
                "acp_permission",
                match d.acp_permission(&opts) {
                    PermissionDecision::Selected { option_id } => format!("Selected({option_id})"),
                    PermissionDecision::Cancelled => "Cancelled".to_string(),
                },
            ),
        ]
    }

    /// The regression guard F-13 exists for: each profile answers exactly what
    /// the table records, cell by cell.
    /// One profile under test: its name, the profile itself, and the accessor
    /// for the column of [`TABLE`] it must match.
    type ProfileUnderTest = (&'static str, HeadlessDefaults, fn(&Row) -> &'static str);

    #[test]
    fn every_profile_answers_its_recorded_table() {
        let profiles: [ProfileUnderTest; 3] = [
            ("api", HeadlessDefaults::api(), |r| r.api),
            ("squad", HeadlessDefaults::squad(SQUAD_TASK_SCOPE), |r| {
                r.squad
            }),
            ("cli", HeadlessDefaults::cli(), |r| r.cli),
        ];

        for (name, defaults, expected_cell) in profiles {
            let actual = answers(&defaults);
            assert_eq!(
                actual.len(),
                TABLE.len(),
                "{name}: the table and the answer list have drifted apart"
            );
            for (row, (question, got)) in TABLE.iter().zip(actual) {
                assert_eq!(
                    row.question, question,
                    "{name}: table and answer list are out of order"
                );
                assert_eq!(
                    expected_cell(row),
                    got,
                    "{name} profile answered {question} with {got}, table says {}",
                    expected_cell(row)
                );
            }
        }
    }

    /// The rows the module doc marks as the same for every host. Asserted here
    /// rather than as three identical table columns.
    #[test]
    fn uniform_answers_are_the_same_for_every_profile() {
        for defaults in [
            HeadlessDefaults::api(),
            HeadlessDefaults::squad(SQUAD_TASK_SCOPE),
            HeadlessDefaults::cli(),
        ] {
            assert_eq!(
                defaults.existing_worktree(),
                ExistingWorktreeDecision::Resume
            );
            assert!(matches!(defaults.yolo_tick(), YoloTickOutcome::Continue));
            assert!(!defaults.replace_aspec());
            assert!(!defaults.run_audit());
            assert!(defaults.work_items_setup().is_none());
            assert_eq!(
                defaults.dockerfile_setup(),
                DockerfileSetupDecision::CreateNew
            );
            assert!(defaults.create_dockerfile());
            assert!(!defaults.run_audit_on_template());
            assert!(!defaults.confirm_deletion());
            assert!(!defaults.delete_task_dir());
            assert_eq!(defaults.spec_kind(), WorkItemKind::Task);
        }
    }

    /// Squad's `mount_scope` is the scope the task was created with, returned
    /// verbatim — not a constant like the other two profiles.
    #[test]
    fn squad_returns_the_captured_mount_scope() {
        for scope in [
            MountScopeDecision::MountGitRoot,
            MountScopeDecision::MountCurrentDirOnly,
            MountScopeDecision::Abort,
        ] {
            assert_eq!(HeadlessDefaults::squad(scope).mount_scope(), scope);
        }
    }

    /// `approve` refuses to treat a non-allow option as approval, so a CLI run
    /// whose agent offered only "reject" still cancels.
    #[test]
    fn cli_acp_permission_does_not_approve_a_reject_only_request() {
        let reject_only = vec![permission_option("no", "reject_once")];
        assert!(matches!(
            HeadlessDefaults::cli().acp_permission(&reject_only),
            PermissionDecision::Cancelled
        ));
    }
}
