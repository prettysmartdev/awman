//! Create a workflow seed, then let an agent author it through the shared artifact session.
use super::*;

pub(super) async fn run(
    command: &NewCommand,
    f: NewWorkflowFlags,
    frontend: &mut dyn NewCommandFrontend,
) -> Result<NewOutcome, CommandError> {
    command.engines.require_container_runtime()?;
    let title = f
        .input
        .title(f.non_interactive, || frontend.ask_workflow_title())?;
    let summary = f
        .input
        .summary(f.non_interactive, || frontend.ask_workflow_summary())?;
    let name = crate::command::work_item::spec_service::slugify(&title);
    let dirs = WorkflowDirs::from_env(
        command.session.env(),
        Some(command.session.git_root().to_path_buf()),
    )?;
    let dir = if f.global {
        dirs.global_dir()
    } else {
        command
            .session
            .git_root()
            .join(crate::data::fs::edit_target::REPO_ASPEC_WORKFLOWS_DIR)
    };
    let path = dir.join(format!("{name}.{}", f.format));
    // Serialization quotes user text; no title can inject workflow fields.
    let seed = match f.format.as_str() {
        "toml" => format!("title = {}\n", toml::Value::String(title)),
        "yaml" => serde_yaml::to_string(&serde_json::json!({"title": title, "steps": []}))
            .map_err(|e| CommandError::Other(e.to_string()))?,
        other => {
            return Err(CommandError::Other(format!(
                "unsupported workflow format: {other}"
            )))
        }
    };
    let root = if f.global {
        crate::data::fs::data_paths::data_home(command.session.env())?
    } else {
        command.session.git_root().to_path_buf()
    };
    write_seed(&root, &path, &seed)?;
    author_artifact(
        command,
        crate::data::fs::edit_target::ArtifactKind::Workflow,
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
    Ok(NewOutcome::Workflow(NewWorkflowOutcome {
        global: f.global,
        format: f.format,
        path: Some(path.display().to_string()),
    }))
}
