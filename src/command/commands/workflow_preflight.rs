//! Checks and resolutions a workflow run makes before the engine exists.
//!
//! Decision Q13 (WI 0114 F-51): the helpers `squad/evaluation.rs` imports live
//! here rather than inside `exec_workflow`, so the squad evaluator does not
//! reach into the command it is a sibling of. Every one of them answers a
//! question about a *workflow*, not about this invocation of `exec workflow`:
//! which agents it will launch, whether their images exist, whether its flag
//! combination is coherent, and how to render the answer when it is not.

/// Where an image build's raw output stream goes when it is not wanted in the
/// caller's message sink. The squad daemon implements this with a per-build
/// log file so build output never floods the daemon log; `begin`/`finish` are
/// its hooks for the lifecycle messages (including the log file's path) that
/// replace the streamed lines.
use std::collections::HashMap;

use crate::command::commands::exec_workflow::{
    parse_leader_flag, workflow_flag_config, ExecWorkflowCommandFlags,
};
use crate::command::commands::launch_policy::LaunchPolicy;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;
use crate::data::workflow_definition::Workflow;
use crate::engine::error::EngineError;

pub(crate) fn validate_workflow_acp_preflight(
    workflow: &Workflow,
    session: &Session,
    flags: &ExecWorkflowCommandFlags,
    sink: &mut dyn UserMessageSink,
) -> Result<HashMap<String, crate::data::config::repo::LaunchMode>, EngineError> {
    use crate::data::config::global::LaunchModeFallback;
    use crate::data::config::repo::LaunchMode;

    let current = session.effective_config();
    let config = crate::data::config::effective::EffectiveConfig::new(
        workflow_flag_config(flags),
        current.env().clone(),
        current.repo().clone(),
        current.global().clone(),
    );
    let requested = config.launch_mode();
    let effective_default_agent = config.agent();
    let mut modes = HashMap::with_capacity(workflow.steps.len());
    for step in &workflow.steps {
        let agent_name = step
            .agent
            .as_deref()
            .or(workflow.agent.as_deref())
            .or(effective_default_agent.as_deref())
            .ok_or_else(|| {
                EngineError::Other(format!(
                    "workflow step '{}' resolves to no agent",
                    step.name
                ))
            })?;
        let agent = crate::data::session::AgentName::new(agent_name).map_err(EngineError::Data)?;
        let mode = if requested == LaunchMode::Acp
            && !crate::engine::agent::agent_matrix::matrix_for(agent.as_str())?.supports_acp
        {
            if config.launch_mode_fallback() == LaunchModeFallback::Error {
                return Err(EngineError::Other(format!(
                    "workflow ACP pre-flight failed: step '{}' uses agent '{}', which does not support ACP",
                    step.name,
                    agent.as_str()
                )));
            }
            sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "workflow step '{}': agent '{}' does not support ACP; falling back to stdio for this session — see launchModeFallback",
                    step.name,
                    agent.as_str()
                ),
            });
            LaunchMode::Stdio
        } else {
            requested
        };
        modes.insert(step.name.clone(), mode);
    }

    // Workflow steps cannot yet be driven over ACP. Unlike direct `chat` /
    // `exec prompt`, the workflow `AgentExecutionFactory` owns and returns the
    // `AgentExecution` that an `AcpSession` also needs to own to run the
    // JSON-RPC driver (initialize → session/new → session/prompt). Until that
    // ownership contract is reworked, a workflow ACP step would launch a
    // container we never speak ACP to — the agent blocks awaiting `initialize`
    // while awman holds stdin open — and the step would report a false success.
    // Fail closed, before any container starts, rather than ship that hang.
    // Unsupported-agent steps that fell back to `stdio` above still run.
    if modes.values().any(|m| matches!(m, LaunchMode::Acp)) {
        return Err(EngineError::NotImplemented(
            "ACP launch mode is not yet supported for workflow steps; run the agent over ACP \
             with `awman chat` or `awman exec prompt`, or set launchMode: stdio for workflows",
        ));
    }
    Ok(modes)
}

/// Validate the `--dynamic` / `--leader` flag relationships before any IO.
/// Runs for every `exec workflow` invocation (dynamic or not). The non-dynamic
/// missing-`workflow`-path error is handled separately in the dispatcher so the
/// existing missing-required-argument message is preserved.
pub(crate) fn validate_dynamic_flags(flags: &ExecWorkflowCommandFlags) -> Result<(), CommandError> {
    if flags.dynamic && flags.workflow.is_some() {
        return Err(CommandError::Other(
            "cannot specify a workflow file path with --dynamic; the path is \
             created automatically"
                .into(),
        ));
    }
    if flags.leader.is_some() && !flags.dynamic {
        return Err(CommandError::Other(
            "--leader is only valid with --dynamic".into(),
        ));
    }
    if flags.dynamic && flags.work_item.is_none() {
        return Err(CommandError::Other("--dynamic requires --work-item".into()));
    }
    if flags.dynamic && flags.plan {
        return Err(CommandError::Other(
            "--dynamic cannot be used with --plan because dynamic mode enforces --yolo".into(),
        ));
    }
    // Parse --leader eagerly so a malformed value fails before any container work.
    if let Some(raw) = &flags.leader {
        parse_leader_flag(raw)?;
    }
    Ok(())
}

/// Apply the implied flags for `--dynamic` mode (WI-0092 §4): forces `yolo`
/// and `worktree` to `true` and appends `context(workflow)` to the overlay
/// list if it is not already present. Called once before any downstream
/// resolution so all subsequent code sees the correct values.
pub(crate) fn apply_dynamic_implied_flags(flags: &mut ExecWorkflowCommandFlags) {
    flags.yolo = true;
    flags.worktree = true;
    if !flags
        .overlay
        .iter()
        .any(|o| o.trim_start().starts_with("context(workflow"))
    {
        flags.overlay.push("context(workflow)".to_string());
    }
}

/// Resolve the leader agent name and optional model override from `flags`,
/// `session`, and the repo config (WI-0092 §7, WI-0095 §5 precedence).
///
/// Precedence:
/// 1. `--leader agent::model` provided → `leader_agent = spec.agent`, `leader_model = spec.model`;
///    `--model` is ignored for the leader.
/// 2. `dynamicWorkflows.defaultLeader` set in repo config (and no `--leader`) →
///    it governs both the leader agent and leader model; `--model` does not
///    override the configured leader model.
/// 3. `--model` provided, no `--leader`/`defaultLeader` → default agent, `leader_model = flags.model`.
/// 4. None of the above → default agent, `leader_model = None`.
pub(crate) fn resolve_leader_model(
    flags: &ExecWorkflowCommandFlags,
    session: &Session,
) -> Result<(crate::data::session::AgentName, Option<String>), CommandError> {
    // 1. `--leader` flag wins.
    if let Some(raw) = &flags.leader {
        let spec = parse_leader_flag(raw)?;
        let agent =
            crate::data::session::AgentName::new(&spec.agent).map_err(CommandError::from)?;
        return Ok((agent, Some(spec.model)));
    }
    // 2. `dynamicWorkflows.defaultLeader` from repo config. Already validated in
    //    RepoConfig::load; re-parse here with the command-layer LeaderSpec to
    //    construct the leader selection.
    if let Some(default_leader) = session
        .repo_config()
        .dynamic_workflows
        .as_ref()
        .and_then(|dw| dw.default_leader.as_deref())
    {
        let spec = parse_leader_flag(default_leader)?;
        let agent =
            crate::data::session::AgentName::new(&spec.agent).map_err(CommandError::from)?;
        return Ok((agent, Some(spec.model)));
    }
    // 3 & 4. `--model` + default-agent fallback (WI-0092 behavior).
    let agent = LaunchPolicy::for_session(session).resolve_agent(&flags.agent)?;
    Ok((agent, flags.model.clone()))
}

/// Validate the `workflow.toml` produced by the leader agent: checks file
/// presence, TOML parse, and resolved-agent Dockerfile validation. Returns the
/// parsed [`Workflow`] on success or a human-readable error string that is
/// passed to the repair loop (WI-0092 §9).
pub(crate) fn validate_generated_workflow(
    generated_path: &std::path::Path,
    session: &Session,
    paths: &crate::data::RepoDockerfilePaths,
) -> Result<Workflow, String> {
    if !generated_path.exists() {
        return Err(format!(
            "leader agent did not produce workflow.toml at {}",
            generated_path.display()
        ));
    }
    match Workflow::load(generated_path) {
        Err(e) => Err(e.to_string()),
        Ok(wf) => match resolve_and_validate_workflow_agents(&wf, session, paths) {
            Err(e) => Err(e),
            Ok(_) => Ok(wf),
        },
    }
}

/// Format the discovered agents into the newline-separated listing substituted
/// into the leader prompt's `{{available_agents}}` slot.
pub(crate) fn format_available_agents(agents: &[(String, std::path::PathBuf)]) -> String {
    if agents.is_empty() {
        return "(no agents discovered — the project has no .awman/Dockerfile.<agent> files)"
            .to_string();
    }
    agents
        .iter()
        .map(|(name, _)| format!("  - {name}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render a configured agent→models map into the newline-separated listing
/// substituted into the leader prompt's `{{available_agents}}` slot (WI-0095 §3).
///
/// Agent names are sorted alphabetically so the leader prompt is deterministic
/// for stable tests and reproducible workflow design; each agent's configured
/// model-list order is preserved.
pub(crate) fn format_agents_with_models(
    map: &std::collections::HashMap<String, Vec<String>>,
) -> String {
    let mut names: Vec<&String> = map.keys().collect();
    names.sort();
    names
        .iter()
        .map(|name| {
            let models = map.get(*name).map(|m| m.join(", ")).unwrap_or_default();
            format!("  - {name}: {models}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the effective (normalized) agent→models map for a dynamic workflow
/// from the configured `agentsToModels`, validating each configured agent
/// against the set of discovered Dockerfile agents (WI-0095 §2).
///
/// Matching is case-insensitive as a compatibility aid, but the returned map is
/// always keyed by the lowercase agent name; the config file is never silently
/// rewritten. Fails with a single descriptive error when any configured agent
/// has no Dockerfile, or when two configured keys collapse to the same
/// discovered agent after case folding. Case-folded (non-exact) matches are
/// appended to `warnings` for the caller to surface.
pub(crate) fn build_effective_agents_to_models(
    configured: &std::collections::HashMap<String, Vec<String>>,
    available_agents: &[(String, std::path::PathBuf)],
    warnings: &mut Vec<String>,
) -> Result<std::collections::HashMap<String, Vec<String>>, CommandError> {
    use std::collections::HashMap;

    // lowercased discovered name → discovered name as spelled by the Dockerfile
    let mut discovered: HashMap<String, &str> = HashMap::new();
    for (name, _) in available_agents {
        discovered.insert(name.to_ascii_lowercase(), name.as_str());
    }

    let mut effective: HashMap<String, Vec<String>> = HashMap::new();
    let mut missing: Vec<String> = Vec::new();
    // lowercase name → the configured key that produced it (dup detection)
    let mut claimed: HashMap<String, String> = HashMap::new();

    // Deterministic iteration over configured keys for stable errors/warnings.
    let mut configured_keys: Vec<&String> = configured.keys().collect();
    configured_keys.sort();

    for key in configured_keys {
        let models = configured.get(key).expect("key drawn from same map");
        let folded = key.to_ascii_lowercase();
        match discovered.get(&folded) {
            Some(found) => {
                if let Some(prev) = claimed.get(&folded) {
                    return Err(CommandError::Other(format!(
                        "dynamicWorkflows.agentsToModels contains keys {prev:?} and {key:?} that \
                         both refer to the discovered agent {found:?} after case folding; remove \
                         one so model lists are not ambiguously merged."
                    )));
                }
                claimed.insert(folded.clone(), key.clone());
                if key.as_str() != *found {
                    warnings.push(format!(
                        "dynamicWorkflows.agentsToModels key {key:?} matched discovered agent \
                         {found:?} only after case folding; the workflow will use {folded:?}."
                    ));
                }
                effective.insert(folded, models.clone());
            }
            None => missing.push(key.clone()),
        }
    }

    if !missing.is_empty() {
        let mut available_names: Vec<String> =
            available_agents.iter().map(|(n, _)| n.clone()).collect();
        available_names.sort();
        return Err(CommandError::Other(format!(
            "dynamicWorkflows.agentsToModels references agents that have no Dockerfile in this \
             repo: [{}].\nAvailable agents: [{}].\nAdd a .awman/Dockerfile.<agent> for each \
             missing agent, or remove it from agentsToModels.",
            missing.join(", "),
            available_names.join(", ")
        )));
    }

    Ok(effective)
}

/// Resolve the unique set of agent names a workflow will launch, using the same
/// precedence as `WorkflowEngine::resolve_agent` (step → workflow → session
/// default), and validate that each has a project Dockerfile. On success
/// returns the set of resolved agents; on failure returns a human-readable
/// error string suitable for the leader repair prompt (WI-0092 §9a).
pub(crate) fn resolve_and_validate_workflow_agents(
    workflow: &Workflow,
    session: &Session,
    paths: &crate::data::RepoDockerfilePaths,
) -> Result<Vec<String>, String> {
    let workflow_default = workflow.agent.as_deref();
    let session_default = session.default_agent().map(|a| a.as_str().to_string());

    let mut resolved: Vec<String> = Vec::new();
    for step in &workflow.steps {
        let agent = step
            .agent
            .as_deref()
            .or(workflow_default)
            .or(session_default.as_deref());
        match agent {
            Some(a) => {
                if !resolved.iter().any(|r| r == a) {
                    resolved.push(a.to_string());
                }
            }
            None => {
                let available = paths.discover_agent_dockerfiles();
                let names: Vec<String> = available.into_iter().map(|(n, _)| n).collect();
                return Err(format!(
                    "step '{}' resolves to no agent: it sets no agent, the workflow sets no \
                     default agent, and the session has no default agent. Add a workflow-level \
                     `agent` field. Available agents: {}",
                    step.name,
                    if names.is_empty() {
                        "(none)".to_string()
                    } else {
                        names.join(", ")
                    },
                ));
            }
        }
    }

    let available = paths.discover_agent_dockerfiles();
    let available_names: Vec<String> = available.iter().map(|(n, _)| n.clone()).collect();
    let unknown: Vec<&String> = resolved
        .iter()
        .filter(|a| !paths.agent_dockerfile(a).exists())
        .collect();
    if !unknown.is_empty() {
        let mut msg =
            String::from("workflow.toml references agents with no Dockerfile in the project:\n");
        for a in &unknown {
            msg.push_str(&format!("  - \"{a}\" (expected .awman/Dockerfile.{a})\n"));
        }
        msg.push_str(&format!(
            "Available agents: {}",
            if available_names.is_empty() {
                "(none)".to_string()
            } else {
                available_names.join(", ")
            },
        ));
        return Err(msg);
    }
    Ok(resolved)
}

pub(crate) trait BuildOutputTarget {
    /// A build for `image` is actually starting (never called when the image
    /// already exists).
    fn begin(&mut self, image: &str);
    /// One line of raw build output.
    fn line(&mut self, line: &str);
    /// The build ended; `error` is `Some` on failure.
    fn finish(&mut self, image: &str, error: Option<&str>);
}

/// Ensure a Dockerfile-backed agent image is available for a container runtime,
/// building it from `.awman/Dockerfile.<agent>` when missing (WI-0092 §9b).
/// A missing Dockerfile is a hard error; a build failure is a hard error and
/// is never routed through the repair loop.
pub(crate) fn ensure_agent_image(
    engines: &Engines,
    git_root: &std::path::Path,
    paths: &crate::data::RepoDockerfilePaths,
    agent: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<(), CommandError> {
    ensure_agent_image_with_build_output(engines, git_root, paths, agent, sink, None)
}

/// [`ensure_agent_image`] with the raw build output optionally redirected to a
/// [`BuildOutputTarget`] instead of streaming through `sink`. Lifecycle
/// messages still go to `sink` either way.
pub(crate) fn ensure_agent_image_with_build_output(
    engines: &Engines,
    git_root: &std::path::Path,
    paths: &crate::data::RepoDockerfilePaths,
    agent: &str,
    sink: &mut dyn UserMessageSink,
    mut build_output: Option<&mut dyn BuildOutputTarget>,
) -> Result<(), CommandError> {
    let runtime = engines
        .require_container_runtime()
        .map_err(CommandError::from)?;
    let dockerfile = paths.agent_dockerfile(agent);
    if !dockerfile.exists() {
        return Err(CommandError::Other(format!(
            "agent '{agent}' has no Dockerfile (expected {})",
            dockerfile.display()
        )));
    }
    let tag = crate::data::image_tags::agent_image_tag(git_root, agent);
    if runtime.image_exists(&tag) {
        return Ok(());
    }
    sink.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("Building image for agent '{agent}' ({tag})…"),
    });
    if let Some(target) = build_output.as_mut() {
        target.begin(&tag);
    }
    let build_result = runtime
        .build_image(
            &tag,
            &dockerfile,
            git_root,
            false,
            &mut |line: &str| match build_output.as_mut() {
                Some(target) => target.line(line),
                None => sink.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: line.to_string(),
                }),
            },
        )
        .map_err(|e| {
            CommandError::Other(format!(
                "failed to build image for agent '{agent}' from {}: {e}",
                dockerfile.display()
            ))
        });
    if let Some(target) = build_output.as_mut() {
        target.finish(
            &tag,
            build_result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .as_deref(),
        );
    }
    build_result?;
    sink.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("Built image for agent '{agent}'."),
    });
    Ok(())
}
