//! `cargo xtask`: awman's gate manifest, evidence and release tooling.
//!
//! This file is only the top-level dispatch. Each command lives in its own
//! module, parses its own arguments and returns a process exit code, so the
//! modules can be written and changed independently. Do not add logic here.
//!
//! Exit codes shared by every command:
//!   0  success
//!   1  the check ran and found a failure (a red gate, a contract mismatch)
//!   2  the command could not run (bad usage, unreadable input, missing tool)
//! `release run` additionally exits 130 when interrupted.

use std::process::ExitCode;

mod ci;
mod evidence;
mod fixtures;
mod gates;
mod lint;
mod manifest;
mod matrix;
mod release;
mod test;

const USAGE: &str = "\
usage: cargo xtask <command> [args]

  gates check|render|plan      validate, render and plan from ci/gates.toml
  evidence emit|aggregate|...  write and aggregate evidence.json
  test run|archive|list|...    run manifest suites with the pinned runner
  fixtures fetch|verify|build  content-addressed test fixtures
  lint hygiene|shell|pins|...  repository lints that are not clippy
  matrix render|check          acceptance matrix
  ci <helper>                  helpers called by the GitHub workflows
  release <subcommand>         release operator and pipeline commands
";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" || args[0] == "help" {
        print!("{USAGE}");
        return ExitCode::from(if args.is_empty() { 2 } else { 0 });
    }
    let command = args.remove(0);
    let result = match command.as_str() {
        "gates" => gates::run(&args),
        "evidence" => evidence::run(&args),
        "test" => test::run(&args),
        "fixtures" => fixtures::run(&args),
        "lint" => lint::run(&args),
        "matrix" => matrix::run(&args),
        "ci" => ci::run(&args),
        "release" => release::run(&args),
        other => {
            eprintln!("xtask: unknown command `{other}`\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("xtask {command}: {error:#}");
            ExitCode::from(2)
        }
    }
}
