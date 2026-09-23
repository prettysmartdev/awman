//! `awman new spec` — scaffolding a work item.
//!
//! Split out of `commands/new.rs` by WI 0114 F-51: `run_with_frontend` was
//! one 530-line `match` over three unrelated scaffolds.

use super::*;

/// Run `awman new spec`.
pub(super) async fn run(
    command: &NewCommand,
    f: NewSpecFlags,
    frontend: &mut dyn NewCommandFrontend,
) -> Result<NewOutcome, CommandError> {
    Ok({
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "new spec: starting work item creation".into(),
        });
        let new_outcome = match crate::command::commands::specs::create_new_spec(
            &command.engines,
            command.session.clone(),
            f.interview,
            f.non_interactive,
            f.issue_source,
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
        NewOutcome::Spec(NewSpecOutcome {
            interview: new_outcome.interview,
            path: new_outcome.created_path,
        })
    })
}
