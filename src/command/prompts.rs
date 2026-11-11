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

/// Browser confirmation projection. Native resume remains an immediate
/// action; native callers keep using `squad_task_confirm` above.
pub fn squad_task_confirmation_view(
    action: FrontendAction,
    name: &str,
) -> Option<Prompt<SquadConfirmDecision>> {
    if let Some(prompt) = squad_task_confirm(action, name) {
        return Some(prompt);
    }
    let (title, body, verb) = match action {
        FrontendAction::ResumeSquadTask => {
            ("Resume task", format!("Resume task \"{name}\"?"), "resume")
        }
        FrontendAction::RemoveSquadTask => {
            ("Delete task", format!("Delete task \"{name}\"?"), "delete")
        }
        _ => return None,
    };
    Some(Prompt::new(
        title,
        body,
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

/// Prerequisite questions always number answers in display order. Longer SSH
/// key lists continue with letters after 9.
fn prerequisite_prompt<D>(
    title: impl Into<String>,
    body: impl Into<String>,
    mut choices: Vec<Choice<D>>,
    default_on_dismiss: Option<D>,
) -> Prompt<D> {
    const KEYS: &str = "123456789abcdefghijklmnopqrstuvwxyz";
    assert!(choices.len() <= KEYS.len());
    for (choice, key) in choices.iter_mut().zip(KEYS.chars()) {
        choice.key = key;
    }
    Prompt::new(title, body, choices, default_on_dismiss)
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
    prerequisite_prompt(
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
    prerequisite_prompt(
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
/// `remote` is the credential-stripped remote URL. `home` is the session's
/// home directory, used only to shorten key paths to `~/...` for display.
pub fn prerequisite_ssh(
    check: &PrerequisiteCheck,
    remote: &str,
    home: Option<&Path>,
) -> Prompt<SshChoice> {
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
        let display = tilde_path(path, home);
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
    prerequisite_prompt(
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

/// `$HOME/.ssh/id_x` → `~/.ssh/id_x` for display, where `home` is the
/// session's home directory. Paths outside it (or with no home) are shown as
/// they are.
fn tilde_path(path: &str, home: Option<&Path>) -> String {
    match home {
        Some(home) if !home.as_os_str().is_empty() => match Path::new(path).strip_prefix(home) {
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
    prerequisite_prompt(
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
    prerequisite_prompt(
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
    prerequisite_prompt(
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
    prerequisite_prompt(
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
    prerequisite_prompt(
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

/// The prerequisite summary's choices, and what each leads to.
///
/// The body is not the table (each frontend draws the report its own way)
/// but what happens next, so "Continue" is never a leap in the dark: who is
/// asked what, and what an item left unresolved does to the run. Esc
/// cancels; Enter continues to the questions.
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
            vec![Choice::new('\n', "Start the workflow", true)]
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
            vec![Choice::new(
                '\n',
                "Enter to configure missing prerequisites",
                true,
            )]
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
        Some(!matches!(next, PrerequisiteReportNext::Questions { .. })),
    )
}

/// Presenter confirmations. The daemon and TUI consume the same copy.
pub fn frontend_prompt(kind: &str) -> Option<Prompt<String>> {
    let (title, body, choices): (&str, &str, Vec<Choice<String>>) = match kind {
        "quit_confirm" => ("Quit awman?", "  Press Ctrl-C again to quit awman\n\n  [Esc] cancel", vec![Choice::new('y',"quit","quit".into()),Choice::new('n',"cancel","dismiss".into())]),
        "close_tab_confirm" => ("Close tab?", "  Press Ctrl-C again to quit awman\n  Press Ctrl-T to close this tab\n\n  [Esc] cancel", vec![Choice::new('y',"close tab","close_tab".into()),Choice::new('n',"cancel","dismiss".into())]),
        "workflow_cancel_confirm" => ("Cancel Workflow Execution", "  Cancel workflow execution?\n\n  The running container will be killed and the\n  current step returned to Pending for resumption.\n\n  [y] cancel execution   [n / Esc] keep running", vec![Choice::new('y',"cancel execution","cancel".into()),Choice::new('n',"keep running","dismiss".into())]),
        "new_tab" => ("New tab", "Select an authorised launch context and a directory beneath its root.", vec![Choice::new('y',"open tab","open_tab".into()),Choice::new('n',"cancel","dismiss".into())]),
        _ => return None,
    };
    Some(Prompt::new(title, body, choices, Some("dismiss".into())))
}
