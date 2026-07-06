//! `CleanCommandFrontend` impl for the CLI.
//!
//! Prints the itemized cleanup summary to stdout, then prompts on stdin for
//! confirmation. When `--yes` is passed the prompt is skipped; when stdin is
//! not a TTY and `--yes` was not passed, the command aborts so scripted
//! invocations never silently delete or hang.

use std::io::Write;

use crate::command::commands::clean::{CleanCommandFrontend, CleanSummary};
use crate::command::dispatch::CommandFrontend;
use crate::command::error::CommandError;
use crate::frontend::cli::command_frontend::CliFrontend;
use crate::frontend::cli::output::stdin_is_tty;

impl CleanCommandFrontend for CliFrontend {
    fn confirm_deletion(&mut self, summary: &CleanSummary) -> Result<bool, CommandError> {
        // Print the itemized list to stdout so it is visible even when message
        // output is redirected.
        println!("{}", summary.render());

        // `--yes` short-circuits the prompt (scripting).
        if self.flag_bool(&["clean"], "yes")?.unwrap_or(false) {
            return Ok(true);
        }

        // Refuse to guess when we cannot ask: abort rather than silently no-op.
        if !stdin_is_tty() {
            return Err(CommandError::InteractiveInputUnavailable {
                prompt: "yes".to_string(),
            });
        }

        print!("Delete the above? [y/N]: ");
        let _ = std::io::stdout().flush();
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return Ok(false);
        }
        Ok(matches!(buf.trim(), "y" | "Y"))
    }
}
