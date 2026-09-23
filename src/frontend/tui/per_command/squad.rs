//! `SquadCommandFrontend` impl for the TUI — the task-creation interview
//! (BLOCKER-3, §9.3) and the persistent-directory delete confirmation
//! (BLOCKER-2, §9.2), both driven through `ask_dialog`, exactly as
//! `NewCommandFrontend` collects a workflow.
//!
//! These COLLECT input only. No validation or scheduling decision is made
//! here; the answers flow to Layer 2, which builds the `CreateTask` and
//! reaches `LocalTaskGateway::validate_create` for every rejection.

use std::path::{Path, PathBuf};

use crate::command::commands::squad::commands::{SquadCommandFrontend, TaskWorkspaceChoice};
use crate::command::commands::squad::supervisor::SquadKeySetup;
use crate::command::error::CommandError;
use crate::data::fs::task_store::MountScope;
use crate::data::prompt::{Prompt, TextPrompt};
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{Dialog, DialogRequest, DialogResponse};

/// The task-description modal's title. Deliberately short — it is drawn into
/// the dialog rect's upper border, where a long sentence overflows or clips.
/// The full instruction lives in the dialog body (`ask_task_description`'s
/// prompt), which wraps and is always readable.
pub const TASK_DESCRIPTION_TITLE: &str = "New squad task description";

/// The title of the one-shot squad key disclosure, in the modal's border.
pub(crate) const KEY_SETUP_TITLE: &str = "squad authentication";

/// Draw the one-time squad key disclosure for the TUI.
///
/// The TUI's counterpart to the CLI's `render_key_setup` (WI 0114 F-56):
/// Layer 2 supplies the key, the shell and the export line, and each frontend
/// says them its own way. There is no box here — `Dialog::Notice` draws the
/// frame, and box-drawing inside one would sit inside another.
pub(crate) fn key_setup_body(setup: &SquadKeySetup) -> String {
    format!(
        "squad API key (store this — it will not be shown again):\n\n  \
         {key}\n\n\
         Add this to {rc_file} so the awman CLI and TUI can authenticate to \
         squad:\n\n  {export_line}\n\n\
         Until you do, export it in the current shell — `awman squad` commands \
         without the key are refused by the daemon with 401 Unauthorized.\n\n\
         Prefer to run without a key? Stop the daemon and start it with\n  \
         awman squad start --dangerously-skip-auth\n\
         which mints no key and accepts unauthenticated requests. squad binds \
         to loopback (127.0.0.1) only, so nothing off this machine can reach it.",
        key = setup.key,
        rc_file = setup.rc_file(),
        export_line = setup.export_line,
    )
}

/// The key-disclosure notice, built the one way for all three sites that
/// raise it (the command thread, and the two squad-tab startup paths).
pub(crate) fn key_setup_notice(setup: &SquadKeySetup) -> DialogRequest {
    DialogRequest::KeySetupNotice {
        title: KEY_SETUP_TITLE.to_string(),
        body: key_setup_body(setup),
        copy_key: setup.key.clone(),
        copy_zshrc_snippet: setup.export_line.clone(),
    }
}

/// The same notice as a ready-built [`Dialog`], for the two startup paths that
/// raise it straight on the `App` rather than through the command thread's
/// `DialogRequest` channel. One spelling, so a first run and a key refresh
/// cannot drift apart in what they show.
pub(crate) fn key_setup_dialog(setup: &SquadKeySetup) -> Dialog {
    Dialog::Notice {
        title: KEY_SETUP_TITLE.to_string(),
        body: key_setup_body(setup),
        copy_key: Some(setup.key.clone()),
        copy_zshrc_snippet: Some(setup.export_line.clone()),
    }
}

impl TuiCommandFrontend {
    /// One optional-field edit prompt (WI 0110): prefilled with the current
    /// value, cleared box means "fall back to the squad default".
    ///
    /// The TUI can express `Option<Option<_>>` without a sentinel token the way
    /// the CLI needs one: the box arrives holding the current value, so
    /// *emptying* it is an unambiguous "clear this", and leaving it alone is
    /// "keep this".
    fn ask_edited_optional(
        &mut self,
        title: &str,
        noun: &str,
        current: Option<&str>,
    ) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: title.to_string(),
            prompt: format!("Leader {noun} (clear the box to use the squad default):"),
            default_text: current.map(str::to_string),
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(Some(t.trim().to_string())),
            DialogResponse::Text(_) => Ok(None),
            _ => Err(CommandError::Aborted),
        }
    }
}

impl SquadCommandFrontend for TuiCommandFrontend {
    /// The same dismissable notice the squad tab raises on a first start,
    /// with the `[c]`/`[z]` copy actions the key snippet needs. Sent, not
    /// asked: a notice has no answer, so blocking the command thread on it
    /// would only stall the command that minted the key.
    fn show_key_setup(&mut self, setup: &SquadKeySetup) {
        let _ = self.dialog_tx.send(key_setup_notice(setup));
    }

    fn ask_task_name(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Task name".into(),
            prompt: "Enter the task slug:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
            _ => Err(CommandError::Aborted),
        }
    }

    /// One freeform description covering both halves of a task — when it fires
    /// and what to do about it — through the same multiline editor
    /// `new spec --interview` uses. A one-line box could not hold either half.
    fn ask_task_description(&mut self) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::MultilineInput {
            title: TASK_DESCRIPTION_TITLE.into(),
            // The border title is a short label; the full instruction lives
            // here in the body, pre-wrapped (the multiline dialog renders its
            // prompt without wrapping).
            prompt: "Describe the new squad task including its triggering conditions\n\
                     and how squad should handle the task each time it is triggered.\n\
                     (Ctrl+Enter to submit)"
                .into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Err(CommandError::Aborted),
        }
    }

    /// The title and the options are `prompt`'s (F-19); this draws the
    /// `KindSelect` dialog and maps the key or the index back.
    fn ask_task_workspace_choice(
        &mut self,
        prompt: &Prompt<TaskWorkspaceChoice>,
    ) -> Result<TaskWorkspaceChoice, CommandError> {
        self.pick_from_prompt(prompt)
    }

    fn confirm_non_git_workspace(&mut self, path: &Path) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Not a Git repository".into(),
            body: format!(
                "{} is not the root of a Git repository.\n\n\
                 Keep this path? (No = choose a different one)",
                path.display()
            ),
        })?;
        match response {
            DialogResponse::Yes => Ok(true),
            DialogResponse::No => Ok(false),
            // Dismissing is not "No": "No" asks for a different path, while
            // Esc abandons the interview outright (WI 0106's interrupted-
            // interview rule). Nothing may be persisted after it.
            _ => Err(CommandError::Aborted),
        }
    }

    fn confirm_parent_directory_workspace(
        &mut self,
        path: &Path,
        current_dir: &Path,
    ) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Mount a parent directory?".into(),
            body: format!(
                "{} is a parent of {}.\n\n\
                 Mount it anyway? (No = choose a different one)",
                path.display(),
                current_dir.display()
            ),
        })?;
        match response {
            DialogResponse::Yes => Ok(true),
            DialogResponse::No => Ok(false),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_task_overlay(&mut self, existing: &[String]) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: format!("Overlays ({} added)", existing.len()),
            prompt: "Add an overlay? [dir()/ssh()/env()/skill() syntax, blank to finish]:".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(Some(t.trim().to_string())),
            // A *blank submission* means "no more overlays" and ends the loop.
            // A dismissal does not: it abandons the interview, and nothing may
            // be persisted after it.
            DialogResponse::Text(_) => Ok(None),
            _ => Err(CommandError::Aborted),
        }
    }

    /// The wording and the default are `prompt`'s (F-19). Submitting an empty
    /// box takes that default; dismissing the dialog abandons the interview.
    fn ask_task_interval(&mut self, prompt: &TextPrompt) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: prompt.title.clone(),
            prompt: prompt.body.clone(),
            default_text: prompt.default.clone(),
        })?;
        match response {
            DialogResponse::Text(typed) => prompt.resolve(&typed).ok_or(CommandError::Aborted),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_task_repo(&mut self) -> Result<PathBuf, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Custom Folder / Repo".into(),
            prompt: "Folder or repository to bind this task to (Enter for current dir):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(PathBuf::from(t.trim())),
            DialogResponse::Text(_) => std::env::current_dir().map_err(|error| {
                CommandError::Other(format!("cannot resolve current dir: {error}"))
            }),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_task_agent(&mut self) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Leader agent".into(),
            prompt: "Leader agent (optional, Enter to skip):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(Some(t.trim().to_string())),
            DialogResponse::Text(_) => Ok(None),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_task_model(&mut self) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Leader model".into(),
            prompt: "Leader model (optional, Enter to skip):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(Some(t.trim().to_string())),
            DialogResponse::Text(_) => Ok(None),
            _ => Err(CommandError::Aborted),
        }
    }

    /// The choices are `prompt`'s (F-19). This was a `YesNo` dialog whose
    /// "yes" meant the git root and whose "no" meant the current directory —
    /// a two-value decision the frontend spelled for itself, worded
    /// differently from the CLI's `[gitroot]/cwd` line.
    fn ask_task_mount_scope(
        &mut self,
        prompt: &Prompt<MountScope>,
    ) -> Result<MountScope, CommandError> {
        self.pick_from_prompt(prompt)
    }

    // ── Task agent pool (WI 0110) ──────────────────────────────────────

    fn ask_use_global_squad_config(&mut self) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Agents and models".into(),
            body: "A global squad configuration exists.\n\n\
                   Use those settings for this task? \
                   (No = give this task its own agents and models)"
                .into(),
        })?;
        match response {
            DialogResponse::Yes => Ok(true),
            DialogResponse::No => Ok(false),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_agent_model(
        &mut self,
        agent: &str,
        existing: &[String],
    ) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: format!("Models for {agent} ({} added)", existing.len()),
            prompt: format!("Add a model {agent} may use (blank to finish):"),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(Some(t.trim().to_string())),
            // A blank submission ends the loop; a dismissal abandons the
            // interview, exactly as in the overlay step.
            DialogResponse::Text(_) => Ok(None),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_additional_agent(
        &mut self,
        existing: &[String],
    ) -> Result<Option<String>, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: format!("Available agents ({} added)", existing.len()),
            prompt: "Add another agent this task may use (blank to finish):".into(),
            default_text: None,
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(Some(t.trim().to_string())),
            DialogResponse::Text(_) => Ok(None),
            _ => Err(CommandError::Aborted),
        }
    }

    // ── Task edit (WI 0110) ────────────────────────────────────────────

    fn ask_edited_description(&mut self, current: &str) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::MultilineInput {
            title: "Edit squad task description".into(),
            prompt: "Describe when this task fires and what squad should do.\n\
                     (Ctrl+Enter to submit)"
                .into(),
            default_text: Some(current.to_string()),
        })?;
        match response {
            DialogResponse::Text(t) => Ok(t),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_edited_interval(&mut self, current: &str) -> Result<String, CommandError> {
        let response = self.ask_dialog(DialogRequest::TextInput {
            title: "Evaluation interval".into(),
            prompt: "How often to evaluate (e.g. 6h, 1d):".into(),
            default_text: Some(current.to_string()),
        })?;
        match response {
            DialogResponse::Text(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
            // A cleared box keeps the current value rather than meaning zero.
            DialogResponse::Text(_) => Ok(current.to_string()),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_edited_agent(&mut self, current: Option<&str>) -> Result<Option<String>, CommandError> {
        self.ask_edited_optional("Leader agent", "agent", current)
    }

    fn ask_edited_model(&mut self, current: Option<&str>) -> Result<Option<String>, CommandError> {
        self.ask_edited_optional("Leader model", "model", current)
    }

    fn ask_replace_overlays(&mut self, current: &[String]) -> Result<bool, CommandError> {
        let shown = if current.is_empty() {
            "(none)".to_string()
        } else {
            current.join(", ")
        };
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Overlays".into(),
            body: format!("Current overlays: {shown}\n\nReplace them? (No = leave them alone)"),
        })?;
        match response {
            DialogResponse::Yes => Ok(true),
            DialogResponse::No => Ok(false),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_replace_agent_pool(
        &mut self,
        current: &std::collections::BTreeMap<String, Vec<String>>,
    ) -> Result<bool, CommandError> {
        let shown = if current.is_empty() {
            "(inherits the global squad settings)".to_string()
        } else {
            current
                .iter()
                .map(|(agent, models)| format!("{agent} = {}", models.join(", ")))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Agents and models".into(),
            body: format!("Current agents:\n{shown}\n\nReplace them? (No = leave them alone)"),
        })?;
        match response {
            DialogResponse::Yes => Ok(true),
            DialogResponse::No => Ok(false),
            _ => Err(CommandError::Aborted),
        }
    }

    fn ask_delete_task_dir(&mut self, name: &str, path: &Path) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: format!("Delete {name} directory?"),
            body: format!(
                "Also delete the persistent task directory {}?",
                path.display()
            ),
        })?;
        Ok(matches!(response, DialogResponse::Yes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::tui::per_command::mount_scope::tests::make_frontend;

    /// Answer the next dialog with `response`, on a helper thread, so the
    /// blocking `ask_dialog` call under test can complete.
    fn answer_with(
        req_rx: std::sync::mpsc::Receiver<DialogRequest>,
        resp_tx: std::sync::mpsc::Sender<DialogResponse>,
        response: DialogResponse,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let _req = req_rx.recv().unwrap();
            resp_tx.send(response).unwrap();
        })
    }

    /// Dismissing any interview dialog abandons task creation. Nothing may be
    /// persisted after an interrupted interview (WI 0106's edge case), so no
    /// step is allowed to quietly substitute a default and let the remaining
    /// prompts carry on to `gateway.create`.
    #[test]
    fn dismissing_any_interview_step_aborts_instead_of_taking_a_default() {
        macro_rules! assert_dismissal_aborts {
            ($label:expr, $call:expr) => {{
                let (mut frontend, req_rx, resp_tx) = make_frontend();
                let handle = answer_with(req_rx, resp_tx, DialogResponse::Dismissed);
                #[allow(clippy::redundant_closure_call)]
                let result = ($call)(&mut frontend);
                handle.join().unwrap();
                assert!(
                    matches!(result, Err(CommandError::Aborted)),
                    "{} must abort when its dialog is dismissed",
                    $label
                );
            }};
        }

        assert_dismissal_aborts!("the name step", |f: &mut TuiCommandFrontend| f
            .ask_task_name()
            .map(|_| ()));
        assert_dismissal_aborts!("the description step", |f: &mut TuiCommandFrontend| f
            .ask_task_description()
            .map(|_| ()));
        assert_dismissal_aborts!("the interval step", |f: &mut TuiCommandFrontend| f
            .ask_task_interval(&crate::command::prompts::squad_task_interval())
            .map(|_| ()));
        assert_dismissal_aborts!("the workspace-choice step", |f: &mut TuiCommandFrontend| f
            .ask_task_workspace_choice(&crate::command::prompts::squad_task_workspace())
            .map(|_| ()));
        assert_dismissal_aborts!("the custom-path step", |f: &mut TuiCommandFrontend| f
            .ask_task_repo()
            .map(|_| ()));
        assert_dismissal_aborts!(
            "the not-a-repository warning",
            |f: &mut TuiCommandFrontend| f
                .confirm_non_git_workspace(std::path::Path::new("/tmp"))
                .map(|_| ())
        );
        assert_dismissal_aborts!(
            "the parent-directory warning",
            |f: &mut TuiCommandFrontend| f
                .confirm_parent_directory_workspace(
                    std::path::Path::new("/tmp"),
                    std::path::Path::new("/tmp/sub")
                )
                .map(|_| ())
        );
        assert_dismissal_aborts!("the overlay step", |f: &mut TuiCommandFrontend| f
            .ask_task_overlay(&[])
            .map(|_| ()));
        assert_dismissal_aborts!("the agent step", |f: &mut TuiCommandFrontend| f
            .ask_task_agent()
            .map(|_| ()));
        assert_dismissal_aborts!("the model step", |f: &mut TuiCommandFrontend| f
            .ask_task_model()
            .map(|_| ()));
        assert_dismissal_aborts!("the mount-scope step", |f: &mut TuiCommandFrontend| f
            .ask_task_mount_scope(&crate::command::prompts::squad_task_mount_scope())
            .map(|_| ()));
    }

    /// A *blank submission* is still a real answer: it keeps the documented
    /// default for optional steps and ends the overlay loop. Only dismissal
    /// aborts.
    #[test]
    fn a_blank_submission_still_means_the_documented_default() {
        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = answer_with(req_rx, resp_tx, DialogResponse::Text(String::new()));
        // The expected value is the prompt's own default, not a literal: that
        // is the whole point of F-19.
        let prompt = crate::command::prompts::squad_task_interval();
        assert_eq!(
            frontend.ask_task_interval(&prompt).unwrap(),
            prompt.default.clone().unwrap()
        );
        handle.join().unwrap();

        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = answer_with(req_rx, resp_tx, DialogResponse::Text("  ".into()));
        assert_eq!(frontend.ask_task_overlay(&[]).unwrap(), None);
        handle.join().unwrap();

        let (mut frontend, req_rx, resp_tx) = make_frontend();
        let handle = answer_with(req_rx, resp_tx, DialogResponse::Text(String::new()));
        assert_eq!(frontend.ask_task_agent().unwrap(), None);
        handle.join().unwrap();
    }
}
