//! Standalone developer tool; registration requested in WI 0144 manifest changes.
//! Keeps reference regeneration separate from the test runner's ignored tests.
use std::{path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(output) = args.next().map(PathBuf::from) else {
        eprintln!("Usage: awman-command-reference <output.md>");
        return ExitCode::from(2);
    };
    if args.next().is_some() {
        eprintln!("Usage: awman-command-reference <output.md>");
        return ExitCode::from(2);
    }
    match std::fs::write(
        &output,
        awman::command::dispatch::catalogue::CommandCatalogue::get().markdown_reference(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}: {error}", output.display());
            ExitCode::FAILURE
        }
    }
}
