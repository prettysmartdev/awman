//! `awman new spec` — scaffolding a work item.
//!
//! Split out of `commands/new.rs` by WI 0114 F-51: `run_with_frontend` was
//! one 530-line `match` over three unrelated scaffolds. The body lives in
//! `specs::create_new_spec`, which creates a local file or a
//! remote work item depending on `workItems.source` (WI 0147).

use super::*;

/// Run `awman new spec`.
pub(super) async fn run(
    command: &NewCommand,
    f: NewSpecFlags,
    frontend: &mut dyn NewCommandFrontend,
) -> Result<NewOutcome, CommandError> {
    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: "new spec: starting work item creation".into(),
    });
    let new_outcome = match crate::command::commands::specs::create_new_spec(
        &command.engines,
        command.session.clone(),
        f.interview,
        f.non_interactive,
        f.work_item,
        &command.cancel,
        frontend,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("new spec: failed to create spec: {e}"),
            });
            return Err(e);
        }
    };
    Ok(NewOutcome::Spec(NewSpecOutcome {
        interview: new_outcome.interview,
        path: new_outcome.created_path,
        url: new_outcome.created_url,
        number: new_outcome.number,
    }))
}
