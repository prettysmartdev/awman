//! Create a custom agent seed and author it in an isolated agent session.
use super::*;
use crate::data::config::agent_definition::{is_first_party_name, validate_agent_file_stem};
use crate::data::fs::agent_dirs::{AgentDirs, DefinitionScope};

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
    command.engines.require_container_runtime()?;
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
    let summary = f
        .input
        .summary(f.non_interactive, || frontend.ask_agent_summary())?;

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
    let paths = dirs.publish(scope, &bodies)?;
    let files_written = paths.iter().map(|p| p.display().to_string()).collect();
    let path = paths[0].clone();

    author_artifact(
        command,
        crate::data::fs::edit_target::ArtifactKind::Agent,
        &path,
        f.global,
        NewSession {
            headless: f.non_interactive,
            yolo: f.yolo,
            summary,
        },
        frontend,
    )
    .await?;

    let next_step = format!("awman ready --agent {}", f.name);
    let outcome = NewAgentOutcome {
        name: f.name,
        path: path.display().to_string(),
        scope,
        files_written,
        next_step,
    };
    frontend.show_new_agent(&outcome);
    Ok(NewOutcome::Agent(outcome))
}

impl NewAgentOutcome {
    pub fn render(&self) -> String {
        format!(
            "Created agent definition ({}): {}\nReview and prepare it with: {}",
            self.scope.as_str(),
            self.path,
            self.next_step
        )
    }
}
