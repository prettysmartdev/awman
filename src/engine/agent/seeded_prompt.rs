//! Keep large user prompts out of exec argv without changing terminal input.

use super::{AgentDefinition, AgentRunOptions};
use crate::data::config::{agents::LaunchMode, env::EnvSnapshot};
use crate::engine::container::options::{ContainerOption, MountGuard};
use crate::engine::error::EngineError;

// Leave room below Linux's 128 KiB per-string limit on 4 KiB pages. The
// limit counts bytes, not characters; a shell wrapper would hit it again.
const MAX_ARGV_PROMPT_BYTES: usize = 64 * 1024;

pub(super) fn stage_large_argv_prompt(
    env: &EnvSnapshot,
    def: &AgentDefinition,
    run: &AgentRunOptions,
    options: &mut Vec<ContainerOption>,
) -> Result<(), EngineError> {
    if run.launch_mode == LaunchMode::Acp || (run.non_interactive && !def.headless_prompt_in_argv) {
        return Ok(());
    }
    let Some(text) = run
        .initial_prompt
        .as_deref()
        .filter(|s| s.len() > MAX_ARGV_PROMPT_BYTES)
    else {
        return Ok(());
    };
    let dir = tempfile::Builder::new()
        .prefix("awman-task-prompt-")
        .tempdir_in(env.temp_dir())
        .map_err(|e| EngineError::io(env.temp_dir(), e))?;
    let host_path = dir.path().join("prompt.md");
    std::fs::write(&host_path, text).map_err(|e| EngineError::io(&host_path, e))?;
    // The private parent protects host access. The mounted file must be
    // readable by non-root image users as well as root.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&host_path, std::fs::Permissions::from_mode(0o444))
            .map_err(|e| EngineError::io(&host_path, e))?;
    }
    let container_path =
        std::path::PathBuf::from(format!("/awman/prompts/{}.md", uuid::Uuid::new_v4()));
    let seed = format!(
        "Read the entire file at {} before starting. It contains my complete task prompt. \
         Treat its contents as my instructions for this turn and carry them out; \
         do not merely summarize them. Read all parts if the file requires multiple reads.",
        container_path.display()
    );
    // SeededPrompt is last-write-wins at resolution. Never interpolate the
    // file into a shell command or append the original text back into argv.
    options.retain(|o| !matches!(o, ContainerOption::SeededPrompt(_)));
    options.push(ContainerOption::SeededPrompt(seed));
    options.push(ContainerOption::TemporaryFileMount {
        host_path,
        container_path,
        guard: MountGuard::new_dir(dir),
    });
    Ok(())
}
