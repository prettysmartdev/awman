//! `awman ready` orchestration for the Docker Sandbox runtime.
//!
//! Replaces the Docker path's "build images" work with kit emission plus
//! subprocess validation: there is no `docker build`, no registry push. Every
//! `sbx` invocation is announced on the user message sink per WI 0090's
//! subprocess-transparency requirement.

use crate::data::fs::SandboxKitPaths;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::error::EngineError;

use super::kit::DSbxKitEmitter;
use super::spawn::SbxCommand;

fn emit(sink: &mut dyn UserMessageSink, level: MessageLevel, text: String) {
    sink.write_message(UserMessage { level, text });
}

/// Prepare the sbx runtime for a single agent: check the binary + login, emit
/// the kit, and validate it. Returns an error for launch-blocking failures
/// (missing binary, kit emission failure, kit validation failure); soft
/// issues (not logged in, missing `kit validate` subcommand) are surfaced as
/// warnings and do not abort. `no_cache` removes the agent's existing awman
/// sandboxes first so the next launch re-runs the kit install.
///
/// No credentials are registered here: all `sbx secret set` calls are
/// sandbox-scoped and happen at agent-launch time (`run_interactive`), so the
/// sandbox exists to scope to and rotated keys apply per launch.
pub(in crate::engine::sandbox) fn ready_agent_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    agent: &str,
    no_cache: bool,
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    // 1. Is `sbx` installed?
    if SbxCommand::new(["version"])
        .in_env(snapshot)
        .run_announced(sink)
        .is_err()
    {
        return Err(EngineError::Sandbox(
            "`sbx` binary not found on PATH. Install Docker Sandboxes with \
             `brew install docker/tap/sbx`, then re-run `awman ready`."
                .into(),
        ));
    }

    // 2. Logged in? `sbx ls` fails with an auth error when not.
    if SbxCommand::new(["ls"])
        .in_env(snapshot)
        .run_announced(sink)
        .is_err()
    {
        emit(
            sink,
            MessageLevel::Warning,
            "sbx: `sbx ls` failed — you may not be logged in. Run `sbx login` to \
             authenticate Docker Sandboxes, then re-run `awman ready`."
                .to_string(),
        );
    }

    // 3. `--no-cache`: remove this agent's awman sandboxes so the next launch
    //    re-runs the kit install against the freshly emitted kit (Phase 6).
    if no_cache {
        let suffix = format!("-{agent}");
        for name in super::backend::list_all_sandbox_names_in(snapshot) {
            if name.starts_with("awman-") && name.ends_with(&suffix) {
                if let Err(e) = SbxCommand::new(["rm", &name])
                    .in_env(snapshot)
                    .run_announced(sink)
                {
                    emit(
                        sink,
                        MessageLevel::Warning,
                        format!("sbx: failed to remove sandbox {name}: {e}"),
                    );
                }
            }
        }
    }

    // 4. Emit the per-agent kit (spec.yaml + startup script).
    let paths = SandboxKitPaths::at_home(crate::data::fs::data_paths::config_home(snapshot)?);
    let kit_dir = paths.kit_dir(agent);
    emit(
        sink,
        MessageLevel::Info,
        format!("Emitting sbx kit for '{agent}' at {}", kit_dir.display()),
    );
    DSbxKitEmitter::new().emit_for_agent(agent, &kit_dir)?;

    // 5. Validate the kit. A real validation failure is surfaced verbatim
    //    with the kit path and fails loudly (WI 0090); only an sbx build that
    //    lacks the `kit validate` subcommand is soft-skipped.
    let kit_dir_str = kit_dir.display().to_string();
    if let Err(e) = SbxCommand::new(["kit", "validate", &kit_dir_str])
        .in_env(snapshot)
        .run_announced(sink)
    {
        let msg = e.to_string();
        let lower = msg.to_lowercase();
        let subcommand_missing = [
            "unknown command",
            "unknown subcommand",
            "unrecognized",
            "no such command",
        ]
        .iter()
        .any(|needle| lower.contains(needle));
        if subcommand_missing {
            emit(
                sink,
                MessageLevel::Warning,
                format!(
                    "sbx: this sbx version does not support `kit validate`; skipping \
                     validation for {kit_dir_str}."
                ),
            );
        } else {
            return Err(EngineError::Sandbox(format!(
                "kit validation failed for {kit_dir_str}: {msg}"
            )));
        }
    }

    // 6. Networking note — raw TCP/UDP is blocked by default.
    emit(
        sink,
        MessageLevel::Info,
        "sbx: only HTTP/HTTPS egress via the sandbox proxy is available by default; raw \
         TCP/UDP (SSH git, databases) is blocked unless added to the kit's \
         network.allowedDomains."
            .to_string(),
    );

    Ok(())
}
