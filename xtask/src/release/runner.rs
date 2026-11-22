//! Temporary runner implementation kept in portable bash for macOS parity.
use std::process::ExitCode;
pub fn run(args: &[String]) -> anyhow::Result<ExitCode> {
    super::operator::exec_shell("xtask/src/release/runner/runner.sh", args)
}
