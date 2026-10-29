//! The copy for every prompt awman puts to a user.
//!
//! Layer 0 holds the *shape* (`Prompt<D>`, `TextPrompt`); this module holds
//! the words. Before WI 0114 F-19 each frontend wrote its own: the CLI
//! printed `"awman: work item kind?"` with a numbered list, the TUI built a
//! `KindSelect` dialog with its own labels, and the two could — and did —
//! drift. A frontend now renders what it is handed and maps a keypress back
//! to the decision; it authors nothing.
//!
//! Defaults come from wherever they are already the truth. The squad
//! interview's evaluation interval is the catalogue's `--interval` default,
//! not a `"6h"` literal written a third time.

use crate::command::commands::agent_trust::{AgentTrustReview, TrustDecision};
use crate::command::commands::agents::{AgentSnapshot, AgentSource, KillChoice};
use crate::command::commands::config::MigrateChoice;
use crate::command::commands::specs::WorkItemKind;
use crate::command::commands::squad::commands::{SquadConfirmDecision, TaskWorkspaceChoice};
use crate::command::commands::workflow_prerequisites::{
    GapChoice, IdentityChoice, IdentityPersist, PrerequisiteReportNext, SshChoice, TokenConsent,
    TokenSourceChoice, ToolChoice,
};
use crate::command::dispatch::catalogue::{CommandCatalogue, FlagDefault};
use crate::command::dispatch::frontend_action::FrontendAction;
use crate::data::fs::task_store::MountScope;
use crate::data::prompt::{Choice, Prompt, TextPrompt};
use crate::data::workflow_prerequisites::{PrerequisiteCheck, Remediation};
use crate::data::workflow_state::StepState;
use crate::engine::auth::host_secret_command::HostSecretCommandReview;
use crate::engine::init::frontend::DockerfileSetupChoice;
use crate::engine::workflow::actions::{
    GroupExit, GroupMember, ParallelGroupDecision, ParallelGroupPrompts,
};
use std::path::{Path, PathBuf};

/// Which kind of work item `awman new spec` / `awman specs` is creating.
///
/// `default_on_dismiss` is `None`: abandoning the interview must not silently
/// file a Task. The CLI's old `_ =>` fall-through did exactly that for any
/// unrecognised input, which is why it is stated here rather than left to a
/// match arm.
pub fn work_item_kind() -> Prompt<WorkItemKind> {
    Prompt::new(
        "Work item kind",
        "",
        vec![
            Choice::new('1', "Feature", WorkItemKind::Feature),
            Choice::new('2', "Bug", WorkItemKind::Bug),
            Choice::new('3', "Task", WorkItemKind::Task),
            Choice::new('4', "Enhancement", WorkItemKind::Enhancement),
        ],
        None,
    )
}

/// How often the squad daemon should evaluate a task.
///
/// The default is the catalogue's `squad add --interval` default, so the
/// interview and the flag cannot disagree.
pub fn squad_task_interval() -> TextPrompt {
    TextPrompt::new(
        "Evaluation Interval",
        "How often should this task be evaluated? (e.g. 30m, 6h, 1d)",
        Some(squad_flag_default("interval").to_string()),
    )
}

/// A `squad add` flag's catalogue default, as a string.
///
/// Panics if the flag is gone or is no longer a string — both are catalogue
/// edits that must not leave an interview asking for something the command
/// cannot accept.
fn squad_flag_default(flag: &str) -> &'static str {
    let spec = CommandCatalogue::get()
        .lookup(&["squad", "add"])
        .expect("`squad add` is in the catalogue");
    match spec
        .find_flag(flag)
        .unwrap_or_else(|| panic!("`squad add --{flag}` is in the catalogue"))
        .default
    {
        FlagDefault::Str(value) => value,
        other => panic!("`squad add --{flag}` default is {other:?}, not a string"),
    }
}

/// Which workspace a new squad task is bound to.
///
/// `default_on_dismiss` is `None`: this is an interview step, and abandoning
/// it must not silently bind the task to a workspace nobody picked. The CLI
/// used to answer `DefaultTaskWorkspace` for any unparseable line — including
/// the empty line a piped stdin gives — which is the fall-through F-19 is
/// about.
pub fn squad_task_workspace() -> Prompt<TaskWorkspaceChoice> {
    Prompt::new(
        "Task Workspace",
        "",
        vec![
            Choice::new(
                '1',
                "Default Task Workspace",
                TaskWorkspaceChoice::DefaultTaskWorkspace,
            ),
            Choice::new(
                '2',
                "Custom Folder / Repo",
                TaskWorkspaceChoice::CustomFolderOrRepo,
            ),
        ],
        None,
    )
}

/// How much of a custom workspace a squad task mounts.
///
/// `default_on_dismiss` is `None`: the scope is captured once, at creation,
/// and every run afterwards is bound by it, so it is not a question a
/// dismissal may answer. The CLI used to read `"cwd"` and map *every other
/// string* — including one the user mistyped — to `GitRoot`, constructing a
/// decision from a string in Layer 3, which `src/data/prompt.rs` forbids.
pub fn squad_task_mount_scope() -> Prompt<MountScope> {
    Prompt::new(
        "Mount scope",
        "How much of the workspace should each run mount?",
        vec![
            Choice::new('g', "the entire git root", MountScope::GitRoot),
            Choice::new('c', "the current directory only", MountScope::Cwd),
        ],
        None,
    )
}

/// How `awman init` should proceed when it finds no project Dockerfile.
///
/// `default_on_dismiss` is `Some(CreateNew)`: the bundled template is what
/// every headless profile answers here
/// ([`HeadlessDefaults::dockerfile_setup`](crate::command::headless::HeadlessDefaults::dockerfile_setup)),
/// it is the only choice that leaves init able to finish, and it writes a
/// file the user can replace or delete. Walking away from the question is not
/// a reason to leave a repository half-initialised.
///
/// The title names no path: the resolved Dockerfile path is a fact the engine
/// hands the frontend separately, and a frontend shows it beside the
/// question.
pub fn dockerfile_setup() -> Prompt<DockerfileSetupChoice> {
    Prompt::new(
        "No project Dockerfile found",
        "How would you like to proceed?",
        vec![
            Choice::new(
                '1',
                "Create Dockerfile.dev from the built-in template (recommended)",
                DockerfileSetupChoice::CreateNew,
            ),
            Choice::new(
                '2',
                "Use an existing Dockerfile in this repo",
                DockerfileSetupChoice::UseExisting,
            ),
            Choice::new(
                '3',
                "Skip for now (configure manually in .awman/config.json)",
                DockerfileSetupChoice::Skip,
            ),
        ],
        Some(DockerfileSetupChoice::CreateNew),
    )
}

/// The confirmation a frontend puts before a squad task action.
///
/// `None` for an action that acts at once: `squad resume` is reversible with
/// the key next to it and has never asked. Which actions ask is a Layer 2
/// decision — before WI 0114 F-55 the TUI made it, by routing `resume` past
/// the confirmation helper and the other three through it.
///
/// `default_on_dismiss` is `Some(Dismiss)`: walking away from a confirmation
/// is a refusal, so Esc must not trigger, cancel or pause anything. This is a
/// confirmation rather than an interview step: refusing to choose *is* an
/// answer here, which is what `Some` records.
pub fn squad_task_confirm(
    action: FrontendAction,
    name: &str,
) -> Option<Prompt<SquadConfirmDecision>> {
    let (title, question, verb) = match action {
        FrontendAction::TriggerSquadTask => (
            "Trigger task",
            format!("Evaluate task \"{name}\" on the next tick?"),
            "trigger",
        ),
        FrontendAction::CancelSquadRun => (
            "Cancel run",
            format!("Cancel the in-progress run of task \"{name}\" and stop its agents?"),
            "cancel run",
        ),
        FrontendAction::PauseSquadTask => {
            ("Pause task", format!("Pause task \"{name}\"?"), "pause")
        }
        _ => return None,
    };
    Some(Prompt::new(
        title,
        question,
        vec![
            Choice::new('y', verb, SquadConfirmDecision::Dispatch),
            Choice::new('n', "back", SquadConfirmDecision::Dismiss),
        ],
        Some(SquadConfirmDecision::Dismiss),
    ))
}

/// The approval asked before awman builds or launches a custom agent
/// definition that needs one (any repo definition; a global one with
/// `[ping]`).
///
/// The body is the whole review summary, host command first.
/// `default_on_dismiss` is `Some(Decline)`: walking away from an approval is
/// a refusal, never a grant.
pub fn agent_trust_confirm(review: &AgentTrustReview) -> Prompt<TrustDecision> {
    Prompt::new(
        format!("Approve agent definition '{}'?", review.name),
        review.render(),
        vec![
            Choice::new('y', "Approve this definition", TrustDecision::Approve),
            Choice::new('n', "Do not approve", TrustDecision::Decline),
        ],
        Some(TrustDecision::Decline),
    )
}

/// `awman agents kill` / `rm`: stop and remove one agent.
///
/// When the agent belongs to a squad task or a TUI tab the body says so,
/// because killing it fails that run. `default_on_dismiss` is `Some(Keep)`:
/// walking away from a confirmation is a refusal.
pub fn agents_kill_confirm(agent: &AgentSnapshot) -> Prompt<KillChoice> {
    let mut body = format!("Stop and remove agent {} ({})?", agent.name, agent.short_id);
    if let AgentSource::Squad(slug) = &agent.source {
        body.push_str(&format!(
            "\nThis agent belongs to squad task `{slug}`; the squad run will fail."
        ));
    } else if let Some(tab) = agent.tab_number {
        body.push_str(&format!("\nThis agent belongs to TUI tab {tab}."));
    }
    Prompt::new(
        "Stop and remove agent",
        body,
        vec![
            Choice::new('y', "stop and remove", KillChoice::Kill),
            Choice::new('n', "keep running", KillChoice::Keep),
        ],
        Some(KillChoice::Keep),
    )
}

/// `awman config migrate`: move renamed settings to their current keys.
///
/// The body is a warning about teammates on older awman versions, who stop
/// seeing moved settings, then the migration preview (file path, then the
/// before/after JSON of every affected key). The warning comes first so a
/// long preview cannot push it out of sight. `default_on_dismiss` is
/// `Some(Cancel)`: walking away from a confirmation is a refusal; Esc must
/// not rewrite a file.
pub fn config_migrate_confirm(preview: &str) -> Prompt<MigrateChoice> {
    Prompt::new(
        "Move `dynamicWorkflows` to `workflows.dynamic`?",
        format!(
            "Teammates on awman versions before v{} will stop seeing these settings once \
             this change is committed.\n\n{preview}",
            env!("CARGO_PKG_VERSION")
        ),
        vec![
            Choice::new('y', "Move it", MigrateChoice::Move),
            Choice::new('n', "Cancel", MigrateChoice::Cancel),
        ],
        Some(MigrateChoice::Cancel),
    )
}

/// The questions the workflow engine asks after a Workflow Control Board
/// choice that affects a whole running parallel group. The engine authors no
/// copy, so it is handed these builders and calls them with the facts.
///
/// Every one has `default_on_dismiss` = `KeepRunning`: walking away from any
/// of them must leave the group exactly as it was.
pub fn parallel_group_prompts() -> ParallelGroupPrompts {
    ParallelGroupPrompts {
        restart_scope: parallel_restart_scope,
        restart_which: parallel_restart_which,
        cancel_group: cancel_parallel_group,
    }
}

fn parallel_restart_scope(group: &[String]) -> Prompt<ParallelGroupDecision> {
    Prompt::new(
        "Restart",
        format!(
            "Restart the whole parallel group ({}), or a single agent in it?",
            group.join(", ")
        ),
        vec![
            Choice::new('g', "Whole group", ParallelGroupDecision::RestartWholeGroup),
            Choice::new('s', "Single agent", ParallelGroupDecision::RestartOneAgent),
        ],
        Some(ParallelGroupDecision::KeepRunning),
    )
}

/// Hotkeys for the agent picker, in order. A group larger than this lists
/// only its first members; no real workflow comes close.
const AGENT_PICKER_KEYS: &str = "123456789abcdefghijklmnopqrstuvwxyz";

fn parallel_restart_which(members: &[GroupMember]) -> Prompt<ParallelGroupDecision> {
    let choices = members
        .iter()
        .zip(AGENT_PICKER_KEYS.chars())
        .map(|((name, state), key)| {
            let status = match state {
                Some(StepState::Running { .. }) => "running",
                Some(StepState::Succeeded) => "done",
                Some(StepState::Failed { .. }) => "failed",
                Some(StepState::Cancelled) => "cancelled",
                Some(StepState::Skipped) => "skipped",
                Some(StepState::Pending) | None => "queued",
            };
            Choice::new(
                key,
                format!("{name} ({status})"),
                ParallelGroupDecision::RestartStep(name.clone()),
            )
        })
        .collect();
    Prompt::new(
        "Restart which agent?",
        "It restarts in a fresh container; the rest of the group keeps running.",
        choices,
        Some(ParallelGroupDecision::KeepRunning),
    )
}

fn cancel_parallel_group(group: &[String], exit: &GroupExit) -> Prompt<ParallelGroupDecision> {
    let quoted = |names: &[String]| {
        names
            .iter()
            .map(|n| format!("'{n}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let then = match exit {
        GroupExit::Back(steps) => format!("go back to {}", quoted(steps)),
        GroupExit::Next(steps) if steps.is_empty() => "finish the workflow".to_string(),
        GroupExit::Next(steps) => format!("move on to {}", quoted(steps)),
    };
    Prompt::new(
        "Cancel parallel group?",
        format!(
            "Cancel the entire parallel group ({}), stopping any agents still running, \
             and {then}?",
            group.join(", ")
        ),
        vec![
            Choice::new('y', "Cancel the group", ParallelGroupDecision::CancelGroup),
            Choice::new('n', "Keep it running", ParallelGroupDecision::KeepRunning),
        ],
        Some(ParallelGroupDecision::KeepRunning),
    )
}

// ─── Workflow prerequisites (WI 0149) ───────────────────────────────────────
//
// Every question the workflow prerequisite flow asks. Each one that grants a
// credential has `default_on_dismiss` = the refusal (`Continue` without it,
// `Decline`), so Esc or Ctrl-C applies nothing for that prerequisite. The
// tool and gap questions dismiss to `Abort`: walking away from "this step
// will fail" should not start a long run.

/// "push_branch", "push_branch, create_pull_request" or "any teardown step"
/// (a dynamic run, whose steps do not exist yet).
fn prerequisite_steps(check: &PrerequisiteCheck) -> String {
    check.needed_by_label()
}

/// The git identity question for `commit_changes`, built from the check's
/// remediations so only the answers that apply are offered. The persistent
/// `.git/config` write is listed last.
pub fn prerequisite_identity(check: &PrerequisiteCheck) -> Prompt<IdentityChoice> {
    let mut choices = Vec::new();
    let mut set_local = None;
    for remediation in &check.remediations {
        match remediation {
            Remediation::UseHostIdentity { name, email } => choices.push(Choice::new(
                'h',
                format!("Use host identity \"{name} <{email}>\""),
                IdentityChoice::UseHost {
                    name: name.clone(),
                    email: email.clone(),
                },
            )),
            Remediation::EnterIdentity => choices.push(Choice::new(
                'e',
                "Enter a name and email",
                IdentityChoice::Enter,
            )),
            Remediation::SetLocalIdentity { name, email, .. } => {
                set_local = Some(Choice::new(
                    'g',
                    "Set it in this repo's .git/config",
                    IdentityChoice::SetLocalFromHost {
                        name: name.clone(),
                        email: email.clone(),
                    },
                ))
            }
            _ => {}
        }
    }
    if !choices
        .iter()
        .any(|c| matches!(c.value, IdentityChoice::Enter))
    {
        choices.push(Choice::new(
            'e',
            "Enter a name and email",
            IdentityChoice::Enter,
        ));
    }
    choices.extend(set_local);
    choices.push(Choice::new(
        'c',
        "Continue without (commit_changes will fail)",
        IdentityChoice::Continue,
    ));
    Prompt::new(
        format!(
            "{} needs a git identity inside its container",
            prerequisite_steps(check)
        ),
        format!(
            "{}\n\nA global ~/.gitconfig on this host is not visible inside the step container. \
             A chosen identity is injected as GIT_AUTHOR_*/GIT_COMMITTER_* only into \
             commit_changes containers.",
            check.explanation
        ),
        choices,
        Some(IdentityChoice::Continue),
    )
}

/// The name asked after "Enter a name and email".
pub fn prerequisite_identity_name() -> TextPrompt {
    TextPrompt::new(
        "Git author name",
        "Used for commit_changes in this run only. Must not be empty or contain a newline.",
        None,
    )
}

/// The email asked after the name.
pub fn prerequisite_identity_email() -> TextPrompt {
    TextPrompt::new(
        "Git author email",
        "Used for commit_changes in this run only. Must contain '@' and no newline.",
        None,
    )
}

/// Confirmation before awman writes `user.name`/`user.email` into the repo's
/// `.git/config` on the host. `command` is the exact command that will run.
/// Dismiss means "don't write it", which still uses the identity for this run.
pub fn prerequisite_identity_persist(command: &str) -> Prompt<IdentityPersist> {
    Prompt::new(
        "Write the identity into this repo's .git/config?",
        format!(
            "awman will run this ON YOUR HOST:\n\n  {command}\n\n\
             The setting persists after this run. In a worktree it is written to the \
             repository's shared config, so every worktree of this repository sees it."
        ),
        vec![
            Choice::new(
                'w',
                "Run it and write .git/config",
                IdentityPersist::WriteLocal,
            ),
            Choice::new(
                'r',
                "Don't write it; use the identity for this run only",
                IdentityPersist::RunOnly,
            ),
        ],
        Some(IdentityPersist::RunOnly),
    )
}

/// The SSH key question for `push_branch` (and `pull_branch`/`clone_repo`
/// over SSH). One "this run" choice per detected key, each followed by its
/// "remember" variant, then the whole `~/.ssh` (offered last because it also
/// exposes `config`, other keys and agent sockets named in config), then
/// "Don't use SSH keys".
///
/// `remote` is the credential-stripped remote URL.
pub fn prerequisite_ssh(check: &PrerequisiteCheck, remote: &str) -> Prompt<SshChoice> {
    const RUN_KEYS: &[char] = &['1', '2', '3', '4', '5', '6', '7', '8'];
    const REMEMBER_KEYS: &[char] = &['a', 'b', 'd', 'e', 'f', 'g', 'h', 'i'];
    let steps = prerequisite_steps(check);
    let keys: Vec<(&String, &Option<bool>, bool, bool)> = check
        .remediations
        .iter()
        .filter_map(|r| match r {
            Remediation::MountSshKey {
                path,
                passphrase_protected,
                remembered,
                suggested,
                ..
            } => Some((path, passphrase_protected, *remembered, *suggested)),
            _ => None,
        })
        .take(RUN_KEYS.len())
        .collect();
    let mut choices = Vec::new();
    for (i, (path, passphrase, remembered, suggested)) in keys.iter().enumerate() {
        let display = tilde_path(path);
        let mut note = String::new();
        if **passphrase == Some(true) {
            note.push_str(" (passphrase-protected: will fail unattended)");
        }
        if *suggested {
            note.push_str(" (suggested by repo config)");
        }
        if *remembered {
            note.push_str(" (remembered)");
        }
        choices.push(Choice::new(
            RUN_KEYS[i],
            format!("Mount {display} read-only for this run{note}"),
            SshChoice::Key {
                path: PathBuf::from(path.as_str()),
                remember: false,
            },
        ));
        choices.push(Choice::new(
            REMEMBER_KEYS[i],
            format!("Mount {display} read-only and remember for this repo"),
            SshChoice::Key {
                path: PathBuf::from(path.as_str()),
                remember: true,
            },
        ));
    }
    if check
        .remediations
        .iter()
        .any(|r| matches!(r, Remediation::MountSshDir))
    {
        choices.push(Choice::new(
            'w',
            "Mount the whole ~/.ssh read-only",
            SshChoice::WholeDir,
        ));
    }
    choices.push(Choice::new(
        'n',
        "Don't use SSH keys (push_branch will fail)",
        SshChoice::Decline,
    ));
    let mut body = format!(
        "The chosen key is mounted read-only, as a single file, at \
         ~/.ssh/{GRANT_SSH_KEY} inside the {steps} container only, with a writable \
         copy of known_hosts. No other step, agent or remediation agent sees it.\n\n{}",
        check.explanation
    );
    let all_protected = !keys.is_empty() && keys.iter().all(|(_, p, _, _)| **p == Some(true));
    if all_protected {
        body.push_str(
            "\n\nEvery detected key is passphrase-protected, and a step container cannot \
             prompt for a passphrase. Consider a deploy key without a passphrase, or an \
             HTTPS remote with a GitHub token.",
        );
    }
    Prompt::new(
        format!(
            "{steps} pushes to {remote} over SSH. Allow awman to mount an SSH key into the \
             {steps} container?"
        ),
        body,
        choices,
        Some(SshChoice::Decline),
    )
}

const GRANT_SSH_KEY: &str =
    crate::engine::overlay::prerequisite_grants::GRANT_SSH_KEY_CONTAINER_NAME;

/// `$HOME/.ssh/id_x` → `~/.ssh/id_x` for display. Paths outside `$HOME` are
/// shown as they are.
fn tilde_path(path: &str) -> String {
    match crate::data::config::env::host_var("HOME") {
        Some(home) if !home.is_empty() => match Path::new(path).strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.to_string(),
        },
        _ => path.to_string(),
    }
}

/// "Use the token found in `var`?", asked when a token is set in the
/// environment. The token goes only to the containers that need it.
pub fn prerequisite_token_consent(check: &PrerequisiteCheck, var: &str) -> Prompt<TokenConsent> {
    let steps = prerequisite_steps(check);
    Prompt::new(
        format!("{steps} need a GitHub token. Use the token in {var} for teardown steps?"),
        format!(
            "The token is passed by name only to the containers that need it \
             (create_pull_request as GH_TOKEN, an HTTPS push_branch as AWMAN_GIT_TOKEN) and \
             to poll_ci on the host. It is redacted from step output and never written to \
             disk. \"Remember\" stores only the variable name, {var}."
        ),
        vec![
            Choice::new('y', "Yes, this run", TokenConsent::ThisRun),
            Choice::new(
                'r',
                "Yes, and remember for this repo",
                TokenConsent::Remember,
            ),
            Choice::new('n', "No", TokenConsent::Decline),
        ],
        Some(TokenConsent::Decline),
    )
}

/// Where a GitHub token should come from, asked when none is set or consent
/// to the one found was declined. Only the sources the check offers are
/// listed, in the work item's order.
pub fn prerequisite_token_source(check: &PrerequisiteCheck) -> Prompt<TokenSourceChoice> {
    let steps = prerequisite_steps(check);
    let mut choices = Vec::new();
    let mut body = Vec::new();
    for remediation in &check.remediations {
        match remediation {
            Remediation::PasteToken => {
                choices.push(Choice::new('1', "Paste a token", TokenSourceChoice::Paste))
            }
            Remediation::RunGhAuthToken {
                argv,
                resolved_path,
                ..
            } => {
                choices.push(Choice::new(
                    '2',
                    "Run \"gh auth token\" on this host",
                    TokenSourceChoice::GhAuthToken,
                ));
                body.push(format!(
                    "\"gh auth token\" runs ON YOUR HOST: {} (resolved to {resolved_path})",
                    render_argv(argv)
                ));
            }
            Remediation::RunGhAuthLogin {
                argv,
                resolved_path,
            } => {
                choices.push(Choice::new(
                    '3',
                    "Log in with \"gh auth login\" on this host, then use its token",
                    TokenSourceChoice::GhAuthLogin,
                ));
                body.push(format!(
                    "\"gh auth login\" runs ON YOUR HOST in the foreground: {} (resolved to \
                     {resolved_path})",
                    render_argv(argv)
                ));
            }
            Remediation::RunAcquireCommand {
                label, argv, scope, ..
            } => {
                choices.push(Choice::new(
                    '4',
                    format!("Run the configured token command ({})", render_argv(argv)),
                    TokenSourceChoice::Acquire,
                ));
                let from = match scope {
                    crate::data::workflow_prerequisites::AcquireScope::Global => {
                        "your global config"
                    }
                    crate::data::workflow_prerequisites::AcquireScope::Repo => {
                        "this repository's config; it needs your approval first"
                    }
                };
                body.push(format!(
                    "{label} from {from} runs ON YOUR HOST: {}",
                    render_argv(argv)
                ));
            }
            _ => {}
        }
    }
    choices.push(Choice::new(
        'c',
        "Continue without a token",
        TokenSourceChoice::Decline,
    ));
    body.push(
        "Continuing without a token: create_pull_request will fail; poll_ci will fall back \
         to unauthenticated, rate-limited requests."
            .to_string(),
    );
    Prompt::new(
        format!("{steps} need a GitHub token. Where should it come from?"),
        body.join("\n"),
        choices,
        Some(TokenSourceChoice::Decline),
    )
}

/// The masked prompt for a pasted token.
pub fn prerequisite_token_paste() -> TextPrompt {
    TextPrompt::masked(
        "Paste a GitHub token",
        "The token is held in memory for this run only and never written anywhere.",
    )
}

/// A tool missing from the phase image. awman never edits a Dockerfile
/// itself, so the body shows the exact line to add.
pub fn prerequisite_missing_tool(check: &PrerequisiteCheck) -> Prompt<ToolChoice> {
    let mut body = check.explanation.clone();
    for remediation in &check.remediations {
        if let Remediation::AddDockerfileLine { file, line } = remediation {
            body.push_str(&format!(
                "\n\nAdd this line to {file} (or your configured base image) and rebuild:\n  {line}"
            ));
        }
    }
    Prompt::new(
        format!(
            "{} is missing from the image {} runs in",
            check.prerequisite.label(),
            prerequisite_steps(check)
        ),
        body,
        vec![
            Choice::new('c', "Continue (the step will fail)", ToolChoice::Continue),
            Choice::new('a', "Abort the run", ToolChoice::Abort),
        ],
        Some(ToolChoice::Abort),
    )
}

/// A gap no grant can fix: a missing remote, a detached HEAD, a pull request
/// with no push before it.
pub fn prerequisite_gap(check: &PrerequisiteCheck) -> Prompt<GapChoice> {
    Prompt::new(
        format!(
            "{}: {} will fail",
            check.prerequisite.label(),
            prerequisite_steps(check)
        ),
        check.explanation.clone(),
        vec![
            Choice::new('c', "Continue", GapChoice::Continue),
            Choice::new('a', "Abort the run", GapChoice::Abort),
        ],
        Some(GapChoice::Abort),
    )
}

/// Approval of a repository-defined token command
/// (`workflows.prerequisites.githubToken.acquire` in `.awman/config.json`)
/// for its exact content hash. `default_on_dismiss` is `Some(Decline)`:
/// walking away from an approval is a refusal.
pub fn prerequisite_acquire_approval(review: &HostSecretCommandReview) -> Prompt<TrustDecision> {
    Prompt::new(
        "Approve this repository's GitHub token command?",
        format!(
            "⚠ awman will run this command ON YOUR HOST (not in a container):\n\n  {}\n\n\
             Resolved program: {}\nTimeout: {}s\nSHA-256: {}\n\n\
             It comes from this repository's .awman/config.json. The approval is \
             remembered for this exact command; any change asks again.",
            review.rendered,
            review.resolved_path.display(),
            review.timeout_secs,
            review.sha256
        ),
        vec![
            Choice::new('y', "Approve this command", TrustDecision::Approve),
            Choice::new('n', "Do not approve", TrustDecision::Decline),
        ],
        Some(TrustDecision::Decline),
    )
}

/// argv joined for display, shell-quoted where needed.
fn render_argv(argv: &[String]) -> String {
    shell_words::join(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every parallel-group question leaves the group running when walked
    /// away from.
    #[test]
    fn parallel_group_prompts_dismiss_to_keep_running() {
        let p = parallel_group_prompts();
        let group = vec!["a".to_string(), "b".to_string()];
        let members = vec![("a".to_string(), None)];
        for prompt in [
            (p.restart_scope)(&group),
            (p.restart_which)(&members),
            (p.cancel_group)(&group, &GroupExit::Next(vec![])),
        ] {
            assert_eq!(
                prompt.default_on_dismiss,
                Some(ParallelGroupDecision::KeepRunning),
                "{}",
                prompt.title
            );
        }
    }

    #[test]
    fn parallel_restart_scope_offers_group_or_single_agent() {
        let prompt = (parallel_group_prompts().restart_scope)(&["a".into(), "b".into()]);
        assert_eq!(
            prompt.answer_for_key('g'),
            Some(ParallelGroupDecision::RestartWholeGroup)
        );
        assert_eq!(
            prompt.answer_for_key('s'),
            Some(ParallelGroupDecision::RestartOneAgent)
        );
    }

    #[test]
    fn parallel_restart_which_lists_every_member_with_its_state() {
        let members = vec![
            ("a".to_string(), Some(StepState::Succeeded)),
            (
                "b".to_string(),
                Some(StepState::Running { container_id: None }),
            ),
            (
                "c".to_string(),
                Some(StepState::Failed {
                    exit_code: 1,
                    error_message: None,
                }),
            ),
            ("d".to_string(), Some(StepState::Pending)),
        ];
        let prompt = (parallel_group_prompts().restart_which)(&members);
        let labels: Vec<&str> = prompt.choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            ["a (done)", "b (running)", "c (failed)", "d (queued)"]
        );
        assert_eq!(
            prompt.answer_for_key('3'),
            Some(ParallelGroupDecision::RestartStep("c".into()))
        );
    }

    #[test]
    fn cancel_parallel_group_says_where_the_workflow_goes() {
        let p = parallel_group_prompts();
        let group = vec!["a".to_string(), "b".to_string()];
        let back = (p.cancel_group)(&group, &GroupExit::Back(vec!["root".into()]));
        assert!(back.body.contains("(a, b)") && back.body.contains("go back to 'root'"));
        let next = (p.cancel_group)(&group, &GroupExit::Next(vec!["c".into()]));
        assert!(next.body.contains("move on to 'c'"), "{}", next.body);
        let last = (p.cancel_group)(&group, &GroupExit::Next(vec![]));
        assert!(last.body.contains("finish the workflow"), "{}", last.body);
        assert_eq!(
            back.answer_for_key('y'),
            Some(ParallelGroupDecision::CancelGroup)
        );
    }

    /// The interview's default *is* the flag's default. Before F-19 the string
    /// `"6h"` appeared in the catalogue, in `cli/command_frontend.rs` and twice
    /// in `tui/per_command/squad.rs`, and only the catalogue's copy was tested.
    #[test]
    fn the_interval_default_is_the_catalogue_flag_default() {
        let prompt = squad_task_interval();
        assert_eq!(
            prompt.default.as_deref(),
            Some(squad_flag_default("interval"))
        );
        // Nothing typed means the flag default, whatever it is.
        assert_eq!(
            prompt.resolve("   ").as_deref(),
            Some(squad_flag_default("interval"))
        );
    }

    /// No squad interview step answers itself on a dismissal.
    ///
    /// The workspace and the mount scope are captured once, at creation, and
    /// bind every run afterwards. Before F-19 the CLI answered
    /// `DefaultTaskWorkspace` for any unparsed line and `GitRoot` for any
    /// string that was not `"cwd"`, so a piped interview silently chose both.
    #[test]
    fn no_squad_interview_prompt_answers_a_dismissal() {
        assert!(squad_task_workspace().default_on_dismiss.is_none());
        assert!(squad_task_mount_scope().default_on_dismiss.is_none());
    }

    /// Every mount scope is reachable, and no frontend maps a string to one.
    #[test]
    fn every_mount_scope_is_offered_under_its_own_key() {
        let prompt = squad_task_mount_scope();
        assert_eq!(prompt.keys(), vec!['g', 'c']);
        assert_eq!(prompt.answer_for_key('g'), Some(MountScope::GitRoot));
        assert_eq!(prompt.answer_for_key('c'), Some(MountScope::Cwd));
    }

    /// The workspace choices keep the digits the CLI and the TUI both used.
    #[test]
    fn every_task_workspace_choice_is_offered_under_its_documented_key() {
        let prompt = squad_task_workspace();
        assert_eq!(prompt.keys(), vec!['1', '2']);
        assert_eq!(
            prompt.answer_for_key('1'),
            Some(TaskWorkspaceChoice::DefaultTaskWorkspace)
        );
        assert_eq!(
            prompt.answer_for_key('2'),
            Some(TaskWorkspaceChoice::CustomFolderOrRepo)
        );
    }

    /// Abandoning the work-item interview must not file a Task by default.
    #[test]
    fn the_work_item_kind_prompt_has_no_dismissal_default() {
        assert!(work_item_kind().default_on_dismiss.is_none());
    }

    /// Every kind is reachable, and by the digit the CLI has always printed.
    #[test]
    fn every_work_item_kind_is_offered_under_its_documented_key() {
        let prompt = work_item_kind();
        assert_eq!(prompt.answer_for_key('1'), Some(WorkItemKind::Feature));
        assert_eq!(prompt.answer_for_key('2'), Some(WorkItemKind::Bug));
        assert_eq!(prompt.answer_for_key('3'), Some(WorkItemKind::Task));
        assert_eq!(prompt.answer_for_key('4'), Some(WorkItemKind::Enhancement));
    }
}

#[cfg(test)]
mod parity_tests {
    //! The regression guard F-19 asks for: both frontends ask the *same*
    //! question, because both are handed the same `Prompt`.
    //!
    //! Asserting the rendered output would pin two different renderings; what
    //! matters is that neither frontend can introduce a word, a hotkey or a
    //! default of its own. These tests pin the prompt each command builds, so
    //! changing the copy means changing it here — where both read it.

    use super::*;

    /// Every hotkey the work-item prompt offers, and the label beside it.
    /// A frontend that rendered its own list (the CLI printed `[1] Feature …`
    /// and additionally accepted `feature`, `bug`, `enhancement`, which the
    /// TUI did not) can no longer diverge.
    #[test]
    fn the_work_item_kind_prompt_is_the_only_source_of_its_copy() {
        let prompt = work_item_kind();
        assert_eq!(prompt.title, "Work item kind");
        assert_eq!(prompt.keys(), vec!['1', '2', '3', '4']);
        assert_eq!(
            prompt.labels(),
            vec!["Feature", "Bug", "Task", "Enhancement"]
        );
    }

    /// The interval prompt's title and body, and a default that is not a
    /// literal.
    #[test]
    fn the_interval_prompt_is_the_only_source_of_its_copy() {
        let prompt = squad_task_interval();
        assert_eq!(prompt.title, "Evaluation Interval");
        assert!(
            prompt.body.contains("How often"),
            "the body asks the question: {:?}",
            prompt.body
        );
        assert!(prompt.default.is_some());
    }

    /// Both frontends ask the mount-scope question with the same words.
    ///
    /// They did not before F-19: the CLI printed `mount scope: [gitroot]/cwd?`
    /// and read a string, while the TUI asked `Mount the entire git root?
    /// (No = current directory only)` as a yes/no. Two wordings, two shapes,
    /// one decision — and the CLI's `_ =>` arm turned a typo into `GitRoot`.
    #[test]
    fn the_mount_scope_prompt_is_the_only_source_of_its_copy() {
        let prompt = squad_task_mount_scope();
        assert_eq!(prompt.title, "Mount scope");
        assert_eq!(
            prompt.labels(),
            vec!["the entire git root", "the current directory only"]
        );
    }

    /// The same for the workspace step, whose labels the CLI numbered itself.
    #[test]
    fn the_task_workspace_prompt_is_the_only_source_of_its_copy() {
        let prompt = squad_task_workspace();
        assert_eq!(prompt.title, "Task Workspace");
        assert_eq!(
            prompt.labels(),
            vec!["Default Task Workspace", "Custom Folder / Repo"]
        );
    }

    /// Neither frontend still authors the copy for these two steps.
    ///
    /// The CLI numbered its own workspace options and read the literal
    /// `"cwd"` into a `MountScope`, mapping everything else — a typo
    /// included — to `GitRoot`. The TUI asked the same scope as a yes/no,
    /// with its own wording. Both now render what
    /// `squad_task_workspace`/`squad_task_mount_scope` hand them.
    ///
    /// Phrase-based rather than label-based: `ask_task_repo`'s dialog title
    /// happens to read "Custom Folder / Repo" too, and that step is still one
    /// of F-19's deferred `TextPrompt` conversions — its copy is legitimately
    /// still in Layer 3, and a label scan would flag it for the coincidence.
    #[test]
    fn no_frontend_still_authors_the_squad_interview_copy() {
        for (path, source, retired) in [
            (
                "cli/command_frontend.rs",
                include_str!("../frontend/cli/command_frontend.rs"),
                &["Default Task Workspace", "mount scope: [gitroot]/cwd?"][..],
            ),
            (
                "tui/per_command/squad.rs",
                include_str!("../frontend/tui/per_command/squad.rs"),
                &["Task Workspace", "Mount the entire git root?"][..],
            ),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            for phrase in retired {
                assert!(
                    !production.contains(&format!("\"{phrase}\"")),
                    "{path} still authors {phrase:?}; the prompt is Layer 2's"
                );
            }
        }
    }

    /// No frontend may spell the interval default. Before F-19 `"6h"` was in
    /// the catalogue, in `cli/command_frontend.rs`, and twice in
    /// `tui/per_command/squad.rs`.
    #[test]
    fn no_frontend_spells_the_interval_default() {
        let default = squad_flag_default("interval");
        for (path, source) in [
            (
                "cli/command_frontend.rs",
                include_str!("../frontend/cli/command_frontend.rs"),
            ),
            (
                "tui/per_command/squad.rs",
                include_str!("../frontend/tui/per_command/squad.rs"),
            ),
        ] {
            // The test module below the frontend asserts against the prompt's
            // own default, so only the production half is scanned.
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            assert!(
                !production.contains(&format!("\"{default}\"")),
                "{path} spells the interval default {default:?} itself"
            );
        }
    }
}

#[cfg(test)]
mod wi0149_tests {
    use super::*;
    fn check(remediations: Vec<Remediation>) -> PrerequisiteCheck {
        PrerequisiteCheck {
            prerequisite: crate::data::workflow_prerequisites::WorkflowPrerequisite::GitIdentity,
            status: crate::data::workflow_prerequisites::PrerequisiteStatus::NeedsConsent,
            explanation: "fixture".into(),
            needed_by: vec![],
            remediations,
        }
    }
    #[test]
    fn wi0149_all_prerequisite_prompts_have_safe_dismiss_defaults_and_unique_keys() {
        fn unique<D>(p: &Prompt<D>) {
            let keys = p.keys();
            assert_eq!(
                keys.iter().collect::<std::collections::BTreeSet<_>>().len(),
                keys.len()
            );
        }
        let c = check(vec![
            Remediation::UseHostIdentity {
                name: "Ada".into(),
                email: "ada@x".into(),
            },
            Remediation::SetLocalIdentity {
                name: "Ada".into(),
                email: "ada@x".into(),
                command: "git config --local".into(),
            },
        ]);
        let identity = prerequisite_identity(&c);
        unique(&identity);
        assert_eq!(identity.keys(), vec!['h', 'e', 'g', 'c']);
        assert_eq!(identity.default_on_dismiss, Some(IdentityChoice::Continue));
        assert_eq!(identity.labels()[0], "Use host identity \"Ada <ada@x>\"");
        let persist = prerequisite_identity_persist("git config --local user.name Ada");
        unique(&persist);
        assert_eq!(persist.default_on_dismiss, Some(IdentityPersist::RunOnly));
        assert!(persist.body.contains("git config --local user.name Ada"));
        let c = check(vec![
            Remediation::MountSshKey {
                path: "/home/test/.ssh/id_ed25519".into(),
                passphrase_protected: Some(true),
                mode_ok: true,
                remembered: false,
                suggested: false,
            },
            Remediation::MountSshDir,
        ]);
        let ssh = prerequisite_ssh(&c, "github.com:org/repo.git");
        unique(&ssh);
        assert_eq!(ssh.keys(), vec!['1', 'a', 'w', 'n']);
        assert!(ssh.labels()[0].contains("passphrase-protected: will fail unattended"));
        assert!(ssh.body.contains("read-only"));
        assert!(ssh.body.contains("deploy key"));
        assert_eq!(ssh.default_on_dismiss, Some(SshChoice::Decline));
        let token = prerequisite_token_consent(&c, "GH_TOKEN");
        unique(&token);
        assert_eq!(
            token.labels(),
            vec!["Yes, this run", "Yes, and remember for this repo", "No"]
        );
        assert_eq!(token.default_on_dismiss, Some(TokenConsent::Decline));
        let source = prerequisite_token_source(&c);
        unique(&source);
        assert_eq!(source.default_on_dismiss, Some(TokenSourceChoice::Decline));
        assert_eq!(source.labels().last(), Some(&"Continue without a token"));
        let tool = prerequisite_missing_tool(&c);
        unique(&tool);
        assert_eq!(tool.default_on_dismiss, Some(ToolChoice::Abort));
        let gap = prerequisite_gap(&c);
        unique(&gap);
        assert_eq!(gap.default_on_dismiss, Some(GapChoice::Abort));
        let review = HostSecretCommandReview {
            label: "op".into(),
            argv: vec!["op".into()],
            rendered: "op read vault".into(),
            resolved_path: "/usr/bin/op".into(),
            timeout_secs: 30,
            sha256: "exact-hash".into(),
        };
        let approval = prerequisite_acquire_approval(&review);
        unique(&approval);
        assert_eq!(approval.default_on_dismiss, Some(TrustDecision::Decline));
        for detail in ["ON YOUR HOST", "/usr/bin/op", "op read vault", "exact-hash"] {
            assert!(approval.body.contains(detail));
        }
        assert!(prerequisite_token_paste().masked);
        assert!(!prerequisite_identity_name().masked);
        assert!(!prerequisite_identity_email().masked);
    }
}

/// The prerequisite summary's choices, and what each leads to.
///
/// The body is not the table (each frontend draws the report its own way)
/// but what happens next, so "Continue" is never a leap in the dark: who is
/// asked what, and what an item left unresolved does to the run. Esc
/// continues; Ctrl-C is an explicit abort, handled by the frontend's input
/// transport.
pub fn prerequisite_report(
    report: &crate::data::workflow_prerequisites::WorkflowPrerequisiteReport,
    next: PrerequisiteReportNext,
) -> Prompt<bool> {
    use crate::data::config::workflows::UnresolvedMode;
    use crate::data::workflow_prerequisites::{PrerequisiteOrigin, PrerequisiteStatus};

    let attention = report.needs_attention();
    let dynamic = report.origin == PrerequisiteOrigin::AssumedAll;
    let mut body: Vec<String> = Vec::new();
    if dynamic && next != PrerequisiteReportNext::CheckOnly {
        body.push(
            "This is a dynamic workflow: its teardown steps are not known until the leader \
             writes them, so every kind of teardown step is checked."
                .into(),
        );
    }
    let choices = match next {
        PrerequisiteReportNext::CheckOnly => {
            body.push(
                "Check only (--check-prerequisites): nothing was granted and no workflow ran."
                    .into(),
            );
            vec![Choice::new('c', "Close", true)]
        }
        PrerequisiteReportNext::Decided => vec![Choice::new('c', "Continue", true)],
        PrerequisiteReportNext::Questions { .. } if attention.is_empty() => {
            body.push("Everything this workflow needs is available.".into());
            vec![
                Choice::new('c', "Start the workflow", true),
                Choice::new('a', "Abort the run", false),
            ]
        }
        PrerequisiteReportNext::Questions { unresolved } => {
            let count = attention.len();
            body.push(format!(
                "Next: awman asks about {} one at a time and offers a fix where it can. \
                 Nothing is mounted, passed to a container or written until you choose it.",
                if count == 1 {
                    "the item that needs attention".to_string()
                } else {
                    format!("each of the {count} items that need attention")
                }
            ));
            let mut steps: Vec<&str> = Vec::new();
            let mut any_step = false;
            for check in &attention {
                any_step |= check.needed_by.is_empty();
                for step in &check.needed_by {
                    if !steps.contains(&step.kind.label()) {
                        steps.push(step.kind.label());
                    }
                }
            }
            let steps = if any_step || steps.is_empty() {
                "the steps that need them".to_string()
            } else {
                steps.join(", ")
            };
            body.push(match unresolved {
                UnresolvedMode::Warn if dynamic => format!(
                    "Anything you leave unresolved does not stop the run: the workflow starts, \
                     you get a warning for each item, the leader is told what is unavailable, \
                     and {steps} will fail if they run."
                ),
                UnresolvedMode::Warn => format!(
                    "Anything you leave unresolved does not stop the run: the workflow starts, \
                     you get a warning for each item, and {steps} will fail when they run."
                ),
                UnresolvedMode::Fail => "Anything you leave unresolved stops the run before any \
                     agent starts (workflows.prerequisites.unresolved is \"fail\")."
                    .to_string(),
            });
            vec![
                Choice::new('c', "Continue to the questions", true),
                Choice::new('a', "Abort the run", false),
            ]
        }
    };
    let unknown = report
        .checks
        .iter()
        .filter(|c| c.status == PrerequisiteStatus::Unknown)
        .count();
    if unknown > 0 {
        body.push(format!(
            "{unknown} {} could not be checked; awman does not ask about {}, and the steps \
             that need {} may still fail.",
            if unknown == 1 { "item" } else { "items" },
            if unknown == 1 { "it" } else { "them" },
            if unknown == 1 { "it" } else { "them" },
        ));
    }
    Prompt::new(
        report.summary_line(),
        body.join("\n\n"),
        choices,
        Some(true),
    )
}

#[cfg(test)]
mod prerequisite_report_tests {
    use super::*;
    use crate::data::config::workflows::UnresolvedMode;
    use crate::data::workflow_prerequisites::*;

    fn report(
        status: PrerequisiteStatus,
        origin: PrerequisiteOrigin,
    ) -> WorkflowPrerequisiteReport {
        WorkflowPrerequisiteReport {
            phase: PrerequisitePhase::Start,
            origin,
            checks: vec![PrerequisiteCheck {
                prerequisite: WorkflowPrerequisite::GitHubToken {
                    host: "github.com".into(),
                },
                status,
                explanation: "No token".into(),
                needed_by: vec![StepRef {
                    phase: crate::data::workflow_state::PhaseKind::Teardown,
                    kind: PrerequisiteStepKind::CreatePullRequest,
                    index: 0,
                    description: "open PR".into(),
                }],
                remediations: vec![],
            }],
            advisories: vec![],
            notes: vec![],
        }
    }

    #[test]
    fn continuing_with_gaps_says_what_is_asked_and_what_fails() {
        let r = report(PrerequisiteStatus::Missing, PrerequisiteOrigin::Static);
        let warn = prerequisite_report(
            &r,
            PrerequisiteReportNext::Questions {
                unresolved: UnresolvedMode::Warn,
            },
        );
        assert_eq!(
            warn.labels(),
            vec!["Continue to the questions", "Abort the run"]
        );
        assert!(warn.body.contains("one at a time"), "{}", warn.body);
        assert!(warn.body.contains("does not stop the run"), "{}", warn.body);
        assert!(
            warn.body.contains("create_pull_request will fail"),
            "{}",
            warn.body
        );
        assert_eq!(warn.default_on_dismiss, Some(true));
        let fail = prerequisite_report(
            &r,
            PrerequisiteReportNext::Questions {
                unresolved: UnresolvedMode::Fail,
            },
        );
        assert!(fail.body.contains("stops the run"), "{}", fail.body);
    }

    #[test]
    fn nothing_to_resolve_and_check_only_offer_no_questions() {
        let ok = report(PrerequisiteStatus::Satisfied, PrerequisiteOrigin::Static);
        let start = prerequisite_report(
            &ok,
            PrerequisiteReportNext::Questions {
                unresolved: UnresolvedMode::Warn,
            },
        );
        assert_eq!(start.labels(), vec!["Start the workflow", "Abort the run"]);
        let check = prerequisite_report(
            &report(PrerequisiteStatus::Missing, PrerequisiteOrigin::AssumedAll),
            PrerequisiteReportNext::CheckOnly,
        );
        assert_eq!(check.labels(), vec!["Close"]);
        assert!(check.body.contains("no workflow ran"));
        assert!(!check.body.contains("dynamic"));
    }

    #[test]
    fn dynamic_runs_and_unknown_items_are_explained() {
        let unknown = report(PrerequisiteStatus::Unknown, PrerequisiteOrigin::AssumedAll);
        let prompt = prerequisite_report(
            &unknown,
            PrerequisiteReportNext::Questions {
                unresolved: UnresolvedMode::Warn,
            },
        );
        assert!(prompt.body.contains("dynamic workflow"), "{}", prompt.body);
        assert!(
            prompt.body.contains("1 item could not be checked"),
            "{}",
            prompt.body
        );
    }
}
