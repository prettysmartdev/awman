//! `CleanCommandFrontend` impl for the CLI.
//!
//! Prints the itemized cleanup summary to stdout, then prompts on stdin for
//! confirmation. When `--yes` is passed the prompt is skipped; when stdin is
//! not a TTY and `--yes` was not passed, the command aborts so scripted
//! invocations never silently delete or hang.

use std::io::Write;

use crate::command::commands::clean::{CleanCommandFrontend, CleanSummary};
use crate::command::error::CommandError;
use crate::frontend::cli::command_frontend::CliFrontend;

impl CleanCommandFrontend for CliFrontend {
    fn show_summary(&mut self, summary: &CleanSummary) {
        // Print the itemized list to stdout so it is visible even when message
        // output is redirected.
        println!("{}", summary.render());
    }

    fn confirm_deletion(&mut self, _summary: &CleanSummary) -> Result<bool, CommandError> {
        // Refuse to guess when we cannot ask: abort rather than silently no-op.
        if self.non_interactive {
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
