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

use crate::command::commands::specs::WorkItemKind;
use crate::command::commands::squad::commands::{SquadConfirmDecision, TaskWorkspaceChoice};
use crate::command::dispatch::catalogue::{CommandCatalogue, FlagDefault};
use crate::command::dispatch::frontend_action::FrontendAction;
use crate::data::fs::task_store::MountScope;
use crate::data::prompt::{Choice, Prompt, TextPrompt};
use crate::engine::init::frontend::DockerfileSetupChoice;

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

#[cfg(test)]
mod tests {
    use super::*;

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
