//! `cargo xtask release <subcommand>`: release operator and pipeline commands.
//!
//! This file is only the dispatch. `operator` and `runner` implement the
//! unchanged `make release` interface; `candidate`, `promote`, `verify`,
//! `validate`, `status` and `abandon` implement the build-once, validate,
//! promote pipeline. `state` is the one module both sides share. Do not add
//! logic here.

use std::process::ExitCode;

mod abandon;
mod candidate;
mod operator;
mod promote;
mod runner;
pub mod state;
mod status;
mod validate;
mod verify;

const USAGE: &str = "\
usage: cargo xtask release <subcommand> [args]

  run <vX.Y.Z>          the `make release` operator flow (RELEASE_RUNNER=auto|local|none)
  status <vX.Y.Z>       current state, failing gate and evidence links (read-only)
  abandon <vX.Y.Z>      delete an unpromoted candidate
  validate              run candidate validation for the current main commit, no release
  runner check [--hardware] | provision | busy <owner/repo> | verify-archive
         | up <owner/repo> <tag> | down
  candidate <step>      steps of candidate.yml
  promote <step>        steps of promote.yml
  verify <step>         post-publish verification steps
";

pub fn run(args: &[String]) -> anyhow::Result<ExitCode> {
    let Some((subcommand, rest)) = args.split_first() else {
        eprint!("{USAGE}");
        return Ok(ExitCode::from(2));
    };
    match subcommand.as_str() {
        "run" => operator::run(rest),
        "runner" => runner::run(rest),
        "status" => status::run(rest),
        "abandon" => abandon::run(rest),
        "validate" => validate::run(rest),
        "candidate" => candidate::run(rest),
        "promote" => promote::run(rest),
        "verify" => verify::run(rest),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        other => {
            eprintln!("xtask release: unknown subcommand `{other}`\n\n{USAGE}");
            Ok(ExitCode::from(2))
        }
    }
}
