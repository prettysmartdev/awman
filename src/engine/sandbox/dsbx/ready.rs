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

/// The top-level `kind:` of an emitted kit spec, if one was emitted before.
fn kit_kind(spec: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(spec).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("kind:"))
        .map(|kind| kind.trim().trim_matches('"').to_string())
}

fn stale_kit_message(agent: &str, sandboxes: &[String]) -> String {
    format!(
        "sbx: the '{agent}' kit changed kind since these sandboxes were created: {}. They still \
         run the previous kit; run 'awman ready --agent {agent} --no-cache' to recreate them.",
        sandboxes.join(", ")
    )
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
    if let Some(def) = crate::engine::agent::first_party::first_party(agent) {
        def.validate_run(
            &Default::default(),
            crate::engine::agent::definition::RunParadigm::Sandbox,
        )?;
    }
    // 1. Is sbx installed?
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
    let previous_kind = kit_kind(&kit_dir.join("spec.yaml"));
    DSbxKitEmitter::new().emit_for_agent(agent, &kit_dir)?;
    // A kit whose `kind` changed (WI 0157: `opencode` moved from a mixin of
    // the stock kit to an awman-owned agent kit) does not reach sandboxes
    // created from the old one; only `--no-cache` recreates them.
    let current_kind = kit_kind(&kit_dir.join("spec.yaml"));
    if !no_cache && previous_kind.is_some() && previous_kind != current_kind {
        let suffix = format!("-{agent}");
        let stale: Vec<String> = super::backend::list_all_sandbox_names_in(snapshot)
            .into_iter()
            .filter(|name| name.starts_with("awman-") && name.ends_with(&suffix))
            .collect();
        if !stale.is_empty() {
            emit(
                sink,
                MessageLevel::Warning,
                stale_kit_message(agent, &stale),
            );
        }
    }

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

#[cfg(test)]
mod stale_kit_tests {
    use super::*;

    #[test]
    fn kit_kind_reads_the_emitted_spec_and_the_warning_names_no_cache() {
        let s = awman_test_support::TestSandbox::for_test(awman_test_support::TestCase::component(
            "engine::sandbox::dsbx::ready::stale_kit_tests::kit_kind_reads_the_emitted_spec_and_the_warning_names_no_cache",
        ));
        let spec = s.work().join("spec.yaml");
        assert_eq!(kit_kind(&spec), None);
        std::fs::write(
            &spec,
            "# header\nschemaVersion: \"1\"\nkind: mixin\nname: x\n",
        )
        .unwrap();
        assert_eq!(kit_kind(&spec).as_deref(), Some("mixin"));
        std::fs::write(&spec, "kind: agent\n").unwrap();
        assert_eq!(kit_kind(&spec).as_deref(), Some("agent"));
        let text = stale_kit_message("opencode", &["awman-abc-opencode".into()]);
        assert!(text.contains("awman-abc-opencode"), "{text}");
        assert!(
            text.contains("awman ready --agent opencode --no-cache"),
            "{text}"
        );
        s.finish();
    }
}
