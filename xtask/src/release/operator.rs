//! The operator shell preserves the terminal contract; Rust owns state/evidence.
mod record;

use std::process::{Command, ExitCode};

pub fn run(args: &[String]) -> anyhow::Result<ExitCode> {
    if args.first().is_some_and(|a| a.starts_with("--internal-")) {
        return internal(args);
    }
    exec_shell("xtask/src/release/operator/flow.sh", args)
}

pub(super) fn exec_shell(script: &str, args: &[String]) -> anyhow::Result<ExitCode> {
    let mut command = Command::new("bash");
    command
        .arg(script)
        .args(args)
        .env("AWMAN_RELEASE_XTASK", std::env::current_exe()?);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec().into())
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        anyhow::bail!("releases are run from macOS or Linux (requires bash)")
    }
}

fn internal(args: &[String]) -> anyhow::Result<ExitCode> {
    match args.first().map(String::as_str) {
        Some("--internal-state") if args.len() == 3 => {
            let state = super::state::query(&args[1], &args[2])?;
            println!(
                "{}|{}|{}|{}|{}",
                serde_json::to_value(state.state)?
                    .as_str()
                    .unwrap_or("none"),
                state.candidate.map(|v| v.to_string()).unwrap_or_default(),
                state.commit.unwrap_or_default(),
                state.run_id.map(|v| v.to_string()).unwrap_or_default(),
                state.tag_on_origin
            );
        }
        _ => record::run(args)?,
    }
    Ok(ExitCode::SUCCESS)
}
