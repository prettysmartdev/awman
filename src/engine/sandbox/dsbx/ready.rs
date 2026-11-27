//! `awman ready` for the Docker Sandbox runtime (contract §9).
//!
//! `ready` proves a refused-or-supported agent route, an admitted `sbx`
//! client, a signed-in CLI, and a parseable, valid local kit. It does not
//! create a sandbox, pull an image, build a workload, register or check a
//! provider credential, or run a host agent — and its output does not
//! suggest otherwise. It never removes a sandbox, `--no-cache` included.

use std::path::Path;

use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::agent::sbx_contract::EmittedKit;
use crate::engine::error::EngineError;

use super::cli;
use super::identity;
use super::spawn::SbxCommand;

fn emit(sink: &mut dyn UserMessageSink, level: MessageLevel, text: String) {
    sink.write_message(UserMessage { level, text });
}

/// Prepare the sbx runtime for a single agent. Every failure is an error;
/// nothing here is a soft skip.
pub(in crate::engine::sandbox) fn ready_agent_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    agent: &str,
    no_cache: bool,
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    // 1. Matrix: a refused or unknown agent fails before any `sbx` call.
    let route = super::backend::supported_route(agent)?;
    // 2. CLI admission.
    cli::admit_installed_cli(snapshot)?;
    // 3. Signed in?
    let existing = super::backend::list_sandbox_names(snapshot).map_err(|e| {
        EngineError::Sandbox(format!(
            "Docker Sandboxes is not available or you are not signed in ({e}); run 'sbx login' \
             and retry"
        ))
    })?;
    // 4. Emit and load the kit.
    emit(
        sink,
        MessageLevel::Info,
        format!("Preparing {agent} kit (sbx)"),
    );
    let kit = super::backend::emit_and_load_kit(snapshot, route.agent)?;
    // 5. Validate the local artifact. This checks the kit's syntax only, not
    //    the effective composition with a published workload, pull access,
    //    credential approval or agent behaviour.
    validate_kit(snapshot, &kit.dir, sink)?;
    // 6. Stale report.
    let data_home = crate::data::fs::data_paths::data_home(snapshot)?;
    report_stale(&data_home, agent, &kit, &existing, no_cache, sink);
    Ok(())
}

fn validate_kit(
    snapshot: &crate::data::config::env::EnvSnapshot,
    kit_dir: &Path,
    sink: &mut dyn UserMessageSink,
) -> Result<(), EngineError> {
    // Test isolation gives every case a private, signed-out HOME; the local
    // artifact can still be checked offline.
    SbxCommand::new(cli::kit_validate_argv(kit_dir, snapshot.test_isolation()))
        .in_env(snapshot)
        .check_preparation(sink)
        .map_err(|e| {
            EngineError::Sandbox(format!(
                "kit validation failed for {}: {e}",
                kit_dir.display()
            ))
        })
}

/// Warn about this agent's existing sandboxes that a launch would refuse,
/// and with `--no-cache` list every awman sandbox of the agent with its
/// removal command. Removes nothing.
fn report_stale(
    data_home: &Path,
    agent: &str,
    kit: &EmittedKit,
    existing: &[String],
    no_cache: bool,
    sink: &mut dyn UserMessageSink,
) {
    let suffix = format!("-{agent}");
    let ours: Vec<&String> = existing
        .iter()
        .filter(|n| n.starts_with("awman-") && n.ends_with(&suffix))
        .collect();
    let mut stale: Vec<String> = Vec::new();
    for name in &ours {
        let record = identity::load(data_home, name);
        let is_stale = match &record {
            None => true,
            Some(r) => {
                r.state != identity::RecordState::Created
                    || r.kit_digest != kit.digest
                    || r.kit_handle != kit.manifest.handle
                    || r.apply_protocol != crate::engine::agent::sbx_contract::APPLY_PROTOCOL
            }
        };
        if is_stale {
            stale.push((*name).clone());
        }
    }
    if !stale.is_empty() {
        emit(sink, MessageLevel::Warning, stale_report(agent, &stale));
    }
    if no_cache && !ours.is_empty() {
        let lines: Vec<String> = ours.iter().map(|n| format!("  sbx rm {n}")).collect();
        emit(
            sink,
            MessageLevel::Info,
            format!(
                "sbx: --no-cache does not remove sandboxes. awman sandboxes of '{agent}' (save \
                 anything you need from inside a sandbox first; files in the workspace are \
                 kept):\n{}",
                lines.join("\n")
            ),
        );
    }
}

fn stale_report(agent: &str, sandboxes: &[String]) -> String {
    format!(
        "sbx: these '{agent}' sandboxes were not created from the current awman kit and will be \
         refused at launch: {}. awman will not remove them for you. To continue:\n  \
         1. Save anything you need from inside each sandbox. Workspace files are on this machine \
         and are kept; agent conversations, logins and other files in the sandbox home are \
         deleted with the sandbox.\n  \
         2. Remove it:  sbx rm <name>\n  \
         3. Run the command again; awman creates a new sandbox with the current kit.",
        sandboxes.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_report_names_each_sandbox_and_never_offers_automatic_removal() {
        let text = stale_report("opencode", &["awman-abc-opencode".into()]);
        assert!(text.contains("awman-abc-opencode"), "{text}");
        assert!(text.contains("sbx rm <name>"), "{text}");
        assert!(text.contains("will not remove"), "{text}");
        assert!(!text.contains("--no-cache"), "{text}");
    }
}
