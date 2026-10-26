//! `awman new agent <name>` — scaffold a custom agent definition.
//!
//! Writes files only: the commented skeleton, or one of the shipped examples
//! (`--from crush|maki`). It never builds. Content the user just asked awman
//! to write is approved on the spot (repo scope always; global scope when it
//! declares `[ping]`), so the next `awman ready --agent <name>` does not ask
//! about it again. A later edit changes the hash and needs a new review.

use super::*;
use crate::command::commands::agent_trust::HostCommandReview;
use crate::data::config::agent_definition::{
    content_hash, is_first_party_name, load_agent_definition_with_scope, validate_agent_file_stem,
};
use crate::data::fs::agent_dirs::{AgentDirs, DefinitionScope};
use crate::engine::agent::registry::AgentRegistry;

/// The commented skeleton: every key, documented.
pub const SKELETON: &str = include_str!("../../../../templates/agent-definition.toml");
/// Install-steps example (also the builtin skill's reference example).
pub const EXAMPLE_CRUSH: &str =
    include_str!("../../../../skills/create-custom-agent/examples/crush.toml");
/// Escape-hatch example; references [`EXAMPLE_MAKI_DOCKERFILE`].
pub const EXAMPLE_MAKI: &str =
    include_str!("../../../../skills/create-custom-agent/examples/maki.toml");
pub const EXAMPLE_MAKI_DOCKERFILE: &str =
    include_str!("../../../../skills/create-custom-agent/examples/maki.Dockerfile");
/// File name `maki.toml`'s `image.dockerfile` refers to.
const MAKI_DOCKERFILE_NAME: &str = "maki.Dockerfile";

/// The files `--from <example>` (or the skeleton) writes: the definition
/// itself, then any sibling files it references.
fn files_for(name: &str, from: Option<&str>) -> Result<Vec<(String, &'static str)>, String> {
    let toml = format!("{name}.toml");
    Ok(match from {
        None => vec![(toml, SKELETON)],
        Some("crush") => vec![(toml, EXAMPLE_CRUSH)],
        Some("maki") => vec![
            (toml, EXAMPLE_MAKI),
            (MAKI_DOCKERFILE_NAME.to_string(), EXAMPLE_MAKI_DOCKERFILE),
        ],
        Some(other) => return Err(format!("unknown example '{other}'; expected crush or maki")),
    })
}

/// Run `awman new agent`.
pub(super) async fn run(
    command: &NewCommand,
    f: NewAgentFlags,
    frontend: &mut dyn NewCommandFrontend,
) -> Result<NewOutcome, CommandError> {
    let invalid_name = |reason: String| CommandError::InvalidArgumentValue {
        command: vec!["new".into(), "agent".into()],
        argument: "name".into(),
        reason,
    };
    validate_agent_file_stem(&f.name).map_err(invalid_name)?;
    if is_first_party_name(&f.name) {
        return Err(invalid_name(format!(
            "'{}' is a first-party agent; custom definitions cannot replace it",
            f.name
        )));
    }
    let files =
        files_for(&f.name, f.from.as_deref()).map_err(|reason| CommandError::InvalidFlagValue {
            command: vec!["new".into(), "agent".into()],
            flag: "from".into(),
            reason,
        })?;

    let session = &command.session;
    let env = session.env();
    let root = session.definition_root();
    let scope = if f.global {
        DefinitionScope::Global
    } else {
        DefinitionScope::Repo
    };
    let dirs = AgentDirs::from_env(env, Some(root.to_path_buf()))?;
    let bodies: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(file, body)| (file.as_str(), body.as_bytes()))
        .collect();
    let paths = dirs.publish(scope, &bodies, f.force)?;
    let files_written = paths.iter().map(|p| p.display().to_string()).collect();
    let path = paths[0].clone();

    // The skeleton is incomplete until the user fills it in, so it is never
    // approved, whatever the file holds by now.
    let (approved, host_command_block) = match f.from.as_deref() {
        None => (false, None),
        Some(example) => {
            let expected = content_hash(
                files[0].1.as_bytes(),
                files.get(1).map(|(_, body)| body.as_bytes()),
            );
            approve_bundled(env, root, &f.name, scope, &path, example, &expected)?
        }
    };

    let next_step = format!("awman ready --agent {}", f.name);
    let outcome = NewAgentOutcome {
        name: f.name,
        path: path.display().to_string(),
        scope,
        skeleton: f.from.is_none(),
        files_written,
        approved,
        host_command_block,
        next_step,
    };
    frontend.show_new_agent(&outcome);
    Ok(NewOutcome::Agent(outcome))
}

impl NewAgentOutcome {
    /// The summary both frontends show: the path, the next step, and — when
    /// the definition declares `[ping]` — the host-command block.
    pub fn render(&self) -> String {
        let mut out = format!(
            "Created agent definition ({}): {}",
            self.scope.as_str(),
            self.path
        );
        if self.skeleton {
            out.push_str(&format!(
                "\nFill in display_name, [run] and [image], then run: {}",
                self.next_step
            ));
        } else {
            out.push_str(&format!("\nNext: {}", self.next_step));
        }
        if let Some(block) = &self.host_command_block {
            out.push_str("\n\n");
            out.push_str(block);
        }
        out
    }
}

/// Approve the definition at `path` as the bundled `example` whose content
/// hash is `expected`. The hash comes from the bytes compiled into awman,
/// never from what is now on disk: the file is re-read only to build the
/// definition, and only when its bytes still hash to `expected`. Anything
/// else (say, a concurrent replacement from a running container) is an
/// error and stays untrusted. Returns whether an approval was recorded and
/// the host-command block to show.
pub(super) fn approve_bundled(
    env: &crate::data::config::env::EnvSnapshot,
    root: &std::path::Path,
    name: &str,
    scope: DefinitionScope,
    path: &std::path::Path,
    example: &str,
    expected: &str,
) -> Result<(bool, Option<String>), CommandError> {
    let loaded = load_agent_definition_with_scope(path, scope).map_err(|e| {
        CommandError::Other(format!(
            "the bundled '{example}' example does not validate (please report this): {e}"
        ))
    })?;
    if loaded.sha256 != expected {
        return Err(CommandError::Other(format!(
            "{} changed while it was being written, so it was not approved; review it, \
             then run: awman ready --agent {name}",
            path.display()
        )));
    }
    let def = crate::engine::agent::custom::convert(&loaded, name, scope);
    let approved = AgentRegistry::needs_approval(&def);
    if approved {
        AgentRegistry::record_approval(env, &def, Some(root))?;
    }
    let block = def
        .ping_argv
        .as_deref()
        .map(|argv| HostCommandReview::from_argv(argv).render());
    Ok((approved, block))
}
