//! `AgentArgv` — the agent's own argv tail, shared by every container-tier
//! backend.
//!
//! Everything after the entrypoint (mode flags, tool restrictions, model,
//! system prompt, add-dirs, user `agentFlags`, interactive seeded prompt) is
//! the agent CLI's contract, not the runtime's: Docker and Apple append it
//! after the image and entrypoint of a `run` argv, and builtin appends it to
//! the entrypoint it hands the sandbox driver. One builder keeps the three from drifting.
//!
//! Pure: no I/O, no env reads. A tool list the agent has no flag for is a
//! typed, fail-closed configuration error on every runtime, because silently
//! dropping a tool restriction is the less safe outcome.

use std::path::Path;

use super::options::{ModelFlagForm, ResolvedContainerOptions};
use crate::engine::error::{EngineError, ToolRestriction};

/// The agent argv that follows the entrypoint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentArgv(Vec<String>);

impl AgentArgv {
    /// Build the tail from resolved options.
    ///
    /// ACP launches yield only the user's `agentFlags`: everything else an ACP
    /// agent needs travels over the JSON-RPC channel, and awman-generated argv
    /// past the entrypoint would corrupt the launch. An ACP launch carrying a tool restriction is
    /// refused, since nothing can deliver it.
    pub fn from_options(options: &ResolvedContainerOptions) -> Result<Self, EngineError> {
        Self::build(options, PathRendering::Lossy)
    }

    /// As [`AgentArgv::from_options`], but a container path that is not
    /// absolute UTF-8 is refused instead of rendered lossily. Builtin plans a
    /// guest filesystem and must not guess; Docker/Apple keep their historical
    /// rendering.
    pub fn from_options_strict(options: &ResolvedContainerOptions) -> Result<Self, EngineError> {
        Self::build(options, PathRendering::Strict)
    }

    fn build(
        options: &ResolvedContainerOptions,
        paths: PathRendering,
    ) -> Result<Self, EngineError> {
        let mut argv = Vec::new();
        if options.acp {
            // No argv reaches an ACP agent, and awman has no JSON-RPC delivery
            // for tool lists: a restriction here would be silently dropped
            // whether or not the agent has a flag for it.
            if !options.allowed_tools.is_empty() || !options.disallowed_tools.is_empty() {
                return Err(EngineError::Config(
                    "tool restrictions cannot be delivered to an ACP launch; remove the \
                     allowed/disallowed tools or launch without ACP"
                        .into(),
                ));
            }
            // The user's own `agentFlags` are the one exception: they were
            // configured for exactly this agent and launch.
            argv.extend(options.agent_flags.iter().cloned());
            return Ok(Self(argv));
        }
        // The non-interactive sub-command or flag (`exec`, `run`, `--print`) is
        // already part of the entrypoint (`AgentDefinition::entrypoint_for`); it
        // is never repeated here.
        // Per-agent mode flags (yolo, auto, plan) — literal args.
        argv.extend(options.agent_mode_flags.iter().cloned());
        for (tools, flag, restriction) in [
            (
                &options.disallowed_tools,
                &options.disallowed_tools_flag,
                ToolRestriction::Disallowed,
            ),
            (
                &options.allowed_tools,
                &options.allowed_tools_flag,
                ToolRestriction::Allowed,
            ),
        ] {
            if tools.is_empty() {
                continue;
            }
            let flag = flag
                .as_ref()
                .ok_or(EngineError::ToolRestrictionFlagMissing { restriction })?;
            argv.extend([flag.clone(), tools.join(",")]);
        }
        if let Some(model) = &options.model {
            match model {
                ModelFlagForm::Argument(name) => argv.extend(["--model".into(), name.clone()]),
                ModelFlagForm::Shorthand(flag) => argv.push(flag.clone()),
            }
        }
        if let Some((_host, container_path, flag)) = &options.system_prompt_file {
            argv.extend([flag.clone(), paths.arg(container_path)?]);
        }
        if let Some((flag, text)) = &options.system_prompt_inline {
            argv.extend([flag.clone(), text.clone()]);
        }
        for (flag, container_path) in &options.agent_add_dirs {
            argv.extend([flag.clone(), paths.arg(container_path)?]);
        }
        // User-configured extra agent args (`agentFlags` / `--agent-flag`): after
        // every awman-generated flag, before the seeded prompt positional.
        argv.extend(options.agent_flags.iter().cloned());
        // Interactive + seeded prompt: most agents take the prompt as the final
        // positional; agents with an `interactive_seed_flag` (opencode
        // `--prompt`) take a flag pair, because their bare positional means
        // something else. Non-interactive prompts are piped on stdin instead.
        if options.interactive {
            if let Some(prompt) = &options.seeded_prompt {
                if let Some(flag) = &options.interactive_seed_flag {
                    argv.push(flag.clone());
                }
                argv.push(prompt.clone());
            }
        }
        Ok(Self(argv))
    }

    /// Append the tail to `argv` (an entrypoint or a full `run` argv).
    pub fn append_to(self, mut argv: Vec<String>) -> Vec<String> {
        argv.extend(self.0);
        argv
    }
}

#[derive(Clone, Copy)]
enum PathRendering {
    /// `Path::display()`, as Docker and Apple have always rendered them.
    Lossy,
    /// Absolute UTF-8 only.
    Strict,
}

impl PathRendering {
    fn arg(self, path: &Path) -> Result<String, EngineError> {
        match self {
            Self::Lossy => Ok(path.display().to_string()),
            Self::Strict => path
                .to_str()
                .filter(|s| s.starts_with('/'))
                .map(str::to_owned)
                .ok_or_else(|| EngineError::Config("guest path must be absolute UTF-8".into())),
        }
    }
}

#[cfg(test)]
mod tests;
