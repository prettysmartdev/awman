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

use crate::command::commands::exec_workflow::{parse_leader_flag, ExecWorkflowCommandFlags};
use crate::command::commands::launch_policy::LaunchPolicy;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::{DynamicWorkflowsSource, ResolvedDynamicWorkflows};
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;
use crate::data::workflow_definition::Workflow;
use crate::engine::error::EngineError;

/// Review and approve, once per distinct agent, every custom definition the
/// workflow's steps will launch — before any of them is resolved, so a
/// workflow never fails halfway on an unapproved definition.
///
/// Agents resolve with the ACP preflight's precedence: step → workflow →
/// effective default (`--agent`, then config). A step that resolves to no
/// agent is left for the preflight below to report.
pub(crate) fn ensure_workflow_agents_approved(
    workflow: &Workflow,
    session: &Session,
    flags: &ExecWorkflowCommandFlags,
    frontend: &mut dyn crate::command::commands::agent_trust::AgentTrustFrontend,
) -> Result<(), CommandError> {
    let config = crate::command::commands::exec_workflow::workflow_effective_config(session, flags)
        .map_err(EngineError::Data)?;
    let default_agent = config.agent();
    let mut agents = Vec::new();
    for step in &workflow.steps {
        let name = step
            .agent
            .as_deref()
            .or(workflow.agent.as_deref())
            .or(default_agent.as_deref());
        if let Some(name) = name {
            agents.push(crate::data::session::AgentName::new(name)?);
        }
    }
    crate::command::commands::agent_trust::ensure_workflow_agents_trusted(
        session, &agents, frontend,
    )
}

pub(crate) fn validate_workflow_acp_preflight(
    workflow: &Workflow,
    session: &Session,
    flags: &ExecWorkflowCommandFlags,
    sink: &mut dyn UserMessageSink,
) -> Result<HashMap<String, crate::data::config::agents::LaunchMode>, EngineError> {
    use crate::data::config::agents::AcpFallback;
    use crate::data::config::agents::LaunchMode;

    let config = crate::command::commands::exec_workflow::workflow_effective_config(session, flags)
        .map_err(EngineError::Data)?;
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
        let definition = crate::engine::agent::AgentRegistry::resolve_for_session(session, &agent)?;
        let mode = if requested == LaunchMode::Acp && !definition.supports_acp {
            if config.acp_fallback() == AcpFallback::Error {
                return Err(EngineError::Other(format!(
                    "workflow ACP pre-flight failed: step '{}' uses agent '{}', which does not support ACP",
                    step.name,
                    agent.as_str()
                )));
            }
            sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "workflow step '{}': agent '{}' does not support ACP; falling back to stdio for this session — see agents.acpFallback",
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

/// The one way a dynamic run reads its settings (WI 0150 §2): an
/// `EffectiveConfig` built from the INVOKING session's env/repo/global config
/// and the workflow flags (so `--leader` is the `Flag` source), as
/// `factory.rs::workflow_agent_flags` does. `session` is the invoking checkout,
/// never a workflow worktree.
pub(crate) fn resolve_dynamic_workflows(
    session: &Session,
    flags: &ExecWorkflowCommandFlags,
) -> Result<ResolvedDynamicWorkflows, CommandError> {
    crate::command::commands::exec_workflow::workflow_effective_config(session, flags)?
        .dynamic_workflows()
        .map_err(CommandError::from)
}

/// Resolve the leader agent name and optional model override (WI-0092 §7,
/// WI-0095 §5, WI 0150 §2).
///
/// Precedence:
/// 1. `--leader agent::model` provided → `leader_agent = spec.agent`, `leader_model = spec.model`;
///    `--model` is ignored for the leader.
/// 2. `workflows.dynamic.defaultLeader` (repo, legacy repo key, then global)
///    and no `--leader` → it governs both the leader agent and leader model;
///    `--model` does not override the configured leader model.
/// 3. `--model` provided, no `--leader`/`defaultLeader` → default agent, `leader_model = flags.model`.
/// 4. None of the above → default agent, `leader_model = None`.
pub(crate) fn resolve_leader_model(
    flags: &ExecWorkflowCommandFlags,
    session: &Session,
    resolved: &ResolvedDynamicWorkflows,
) -> Result<(crate::data::session::AgentName, Option<String>), CommandError> {
    // 1. `--leader` flag wins. (Also reflected as the `Flag` source in
    //    `resolved`; parsed here so a malformed flag names the flag.)
    if let Some(raw) = &flags.leader {
        let spec = parse_leader_flag(raw)?;
        let agent =
            crate::data::session::AgentName::new(&spec.agent).map_err(CommandError::from)?;
        return Ok((agent, Some(spec.model)));
    }
    // 2. The configured default leader. Already validated at config load;
    //    re-parsed here to construct the leader selection. Errors name the key
    //    and file the user wrote.
    if let Some(leader) = &resolved.default_leader {
        let label = leader.source.describe("defaultLeader");
        let spec = parse_leader_flag(&leader.value)
            .map_err(|e| CommandError::Other(format!("{label}: {e}")))?;
        let agent = crate::data::session::AgentName::new(&spec.agent)
            .map_err(|e| CommandError::Other(format!("{label}: {e}")))?;
        return Ok((agent, Some(spec.model)));
    }
    // 3 & 4. `--model` + default-agent fallback (WI-0092 behavior).
    let agent = LaunchPolicy::for_session(session).resolve_agent(&flags.agent)?;
    Ok((agent, flags.model.clone()))
}

/// Name the configured leader's key and file on a leader-availability error
/// (an unknown agent, a missing Dockerfile, a failed image build), so a user
/// whose global `defaultLeader` names an agent this repo lacks sees where it
/// came from. A `--leader` (or no configured leader) leaves the error as is.
pub(crate) fn attribute_leader_error(
    resolved: &ResolvedDynamicWorkflows,
    error: CommandError,
) -> CommandError {
    match &resolved.default_leader {
        Some(leader) if leader.source != DynamicWorkflowsSource::Flag && error.exit_code() == 1 => {
            CommandError::Other(format!(
                "{error}\nThe leader is set by {}; pass --leader agent::model to override it.",
                leader.source.describe("defaultLeader")
            ))
        }
        _ => error,
    }
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

/// What to do with a configured agent that has no `.awman/Dockerfile.<agent>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissingDockerfilePolicy {
    /// Repo-vetted list: a missing Dockerfile is a hard error.
    Fail,
    /// Global list spans repos: skip the entry with a warning.
    SkipWithWarning,
}

/// Build the effective (normalized) agent→models map for a dynamic workflow
/// from a configured `agentsToModels`, validating each configured agent
/// against the set of discovered Dockerfile agents (WI-0095 §2, WI 0150 §2).
///
/// Matching is case-insensitive as a compatibility aid, but the returned map is
/// always keyed by the lowercase agent name; the config file is never silently
/// rewritten. Fails with a single descriptive error when two configured keys
/// collapse to the same discovered agent after case folding, and — under
/// [`MissingDockerfilePolicy::Fail`] — when any configured agent has no
/// Dockerfile. Under [`MissingDockerfilePolicy::SkipWithWarning`] such agents
/// are dropped with one warning each, and the returned map may be EMPTY,
/// which the caller treats as "fall back to discovery". `key_label` is what
/// messages call the map (`workflows.dynamic.agentsToModels`,
/// `dynamicWorkflows.agentsToModels`, `squad.agentsToModels`). Case-folded
/// matches and skips are appended to `warnings` for the caller to surface.
pub(crate) fn build_effective_agents_to_models(
    configured: &std::collections::HashMap<String, Vec<String>>,
    available_agents: &[(String, std::path::PathBuf)],
    policy: MissingDockerfilePolicy,
    key_label: &str,
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
                        "{key_label} contains keys {prev:?} and {key:?} that both refer to the \
                         discovered agent {found:?} after case folding; remove one so model \
                         lists are not ambiguously merged."
                    )));
                }
                claimed.insert(folded.clone(), key.clone());
                if key.as_str() != *found {
                    warnings.push(format!(
                        "{key_label} key {key:?} matched discovered agent {found:?} only after \
                         case folding; the workflow will use {folded:?}."
                    ));
                }
                effective.insert(folded, models.clone());
            }
            None => missing.push(key.clone()),
        }
    }

    if missing.is_empty() {
        return Ok(effective);
    }
    match policy {
        MissingDockerfilePolicy::SkipWithWarning => {
            for agent in &missing {
                warnings.push(format!(
                    "global {key_label}: skipping '{agent}' — no Dockerfile in this repo"
                ));
            }
            Ok(effective)
        }
        MissingDockerfilePolicy::Fail => {
            let mut available_names: Vec<String> =
                available_agents.iter().map(|(n, _)| n.clone()).collect();
            available_names.sort();
            Err(CommandError::Other(format!(
                "{key_label} references agents that have no Dockerfile in this repo: [{}].\n\
                 Available agents: [{}].\nAdd a .awman/Dockerfile.<agent> for each missing \
                 agent, or remove it from agentsToModels.",
                missing.join(", "),
                available_names.join(", ")
            )))
        }
    }
}

/// The `{{available_agents}}` section of the dynamic leader prompt, from the
/// resolved `agentsToModels` (WI 0150 §2): a repo map (either key) keeps the
/// hard missing-Dockerfile error; a global map skips agents this repo has no
/// Dockerfile for, and falls back to Dockerfile discovery when it skips them
/// all. With no map at all, discovery as before. Returns the section and the
/// warnings to emit.
pub(crate) fn agents_section_for_leader(
    resolved: &ResolvedDynamicWorkflows,
    available_agents: &[(String, std::path::PathBuf)],
) -> Result<(String, Vec<String>), CommandError> {
    let mut warnings = Vec::new();
    let Some(map) = &resolved.agents_to_models else {
        return Ok((format_available_agents(available_agents), warnings));
    };
    let key_label = "workflows.dynamic.agentsToModels".to_string();
    let policy = match map.source {
        DynamicWorkflowsSource::Global => MissingDockerfilePolicy::SkipWithWarning,
        _ => MissingDockerfilePolicy::Fail,
    };
    let effective = build_effective_agents_to_models(
        &map.value,
        available_agents,
        policy,
        &key_label,
        &mut warnings,
    )?;
    if effective.is_empty() {
        warnings.push(format!(
            "global {key_label}: no listed agent has a Dockerfile in this repo; falling back to \
             Dockerfile discovery"
        ));
        return Ok((format_available_agents(available_agents), warnings));
    }
    Ok((format_agents_with_models(&effective), warnings))
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
    for name in &resolved {
        let agent = crate::data::session::AgentName::new(name).map_err(|e| e.to_string())?;
        crate::engine::agent::AgentRegistry::resolve_for_session(session, &agent)
            .map_err(|e| e.to_string())?;
    }
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

pub(crate) trait BuildOutputTarget: Send {
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
pub(crate) async fn ensure_agent_image(
    engines: &Engines,
    git_root: &std::path::Path,
    paths: &crate::data::RepoDockerfilePaths,
    agent: &str,
    session: &crate::data::session::Session,
    sink: &mut dyn UserMessageSink,
) -> Result<(), CommandError> {
    ensure_agent_image_with_build_output(engines, git_root, paths, agent, session, sink, None).await
}

/// Static `exec workflow` preflight on a runtime that builds its images
/// (Docker, Apple Containers). Every agent the steps launch needs its image
/// before the first step's container starts; for each one that is missing
/// the frontend is asked, once per agent:
///
/// - **set up** — write the agent's Dockerfile from its definition when the
///   repo has none, then build its image;
/// - **fall back** — run those steps with the default agent instead (offered
///   only when the default agent's image exists);
/// - **abort** — stop before anything is created.
///
/// Importing runtimes (builtin) and sandbox runtimes prepare images their own
/// way and are left alone, as is a runtime that is not reachable (the launch
/// then reports the runtime error).
pub(crate) async fn ensure_static_workflow_agent_images(
    workflow: &mut Workflow,
    engines: &Engines,
    session: &Session,
    flags: &ExecWorkflowCommandFlags,
    frontend: &mut dyn crate::command::commands::agent_setup::AgentSetupFrontend,
) -> Result<(), CommandError> {
    let config = crate::command::commands::exec_workflow::workflow_effective_config(session, flags)
        .map_err(EngineError::Data)?;
    let default_agent = match config.agent() {
        Some(name) => crate::data::session::AgentName::new(name).map_err(EngineError::Data)?,
        None => session.configured_agent(),
    };
    ensure_workflow_agent_images_with_default(workflow, engines, session, &default_agent, frontend)
        .await
}

/// [`ensure_static_workflow_agent_images`] once the run's default agent (the
/// one a step runs when neither it nor the workflow names one) is resolved.
async fn ensure_workflow_agent_images_with_default(
    workflow: &mut Workflow,
    engines: &Engines,
    session: &Session,
    default_agent: &crate::data::session::AgentName,
    frontend: &mut dyn crate::command::commands::agent_setup::AgentSetupFrontend,
) -> Result<(), CommandError> {
    use crate::command::commands::agent_setup::AgentSetupDecision;
    use crate::data::image_tags::{agent_image_tag, project_image_tag};
    use crate::data::session::AgentName;
    use crate::engine::agent_runtime::ImageAcquisition;

    let runtime = &engines.runtime;
    // An unreachable runtime has no images to ask about; the launch reports
    // it with its own error (and exit code).
    if runtime.capabilities().image_acquisition != ImageAcquisition::Build
        || !runtime.is_available()
    {
        return Ok(());
    }
    let git_root = session.git_root();
    let paths = crate::data::RepoDockerfilePaths::new(git_root);
    let image_present = |agent: &str| runtime.image_exists(&agent_image_tag(git_root, agent));

    // Step → workflow → default, the order `validate_workflow_acp_preflight`
    // and the engine resolve a step's agent in.
    let workflow_agent = workflow.agent.clone();
    let resolve = |step_agent: &Option<String>| -> String {
        step_agent
            .clone()
            .or_else(|| workflow_agent.clone())
            .unwrap_or_else(|| default_agent.as_str().to_string())
    };
    let mut agents: Vec<String> = Vec::new();
    for step in &workflow.steps {
        let agent = resolve(&step.agent);
        if !agents.contains(&agent) {
            agents.push(agent);
        }
    }

    for agent in agents {
        if !matches!(image_present(&agent), Ok(false)) {
            continue;
        }
        let requested = AgentName::new(&agent).map_err(EngineError::Data)?;
        let image_only = paths.agent_dockerfile(&agent).exists();
        let default_available = agent != default_agent.as_str()
            && image_present(default_agent.as_str()).unwrap_or(false);
        match frontend.ask_agent_setup(&requested, default_agent, default_available, image_only)? {
            AgentSetupDecision::Setup => {
                let project_tag = project_image_tag(git_root);
                if !runtime.image_exists(&project_tag).unwrap_or(false) {
                    return Err(EngineError::AgentRequiresProjectImage { tag: project_tag }.into());
                }
                ensure_agent_image(engines, git_root, &paths, &agent, session, frontend).await?;
            }
            AgentSetupDecision::FallbackToDefault if default_available => {
                for step in &mut workflow.steps {
                    if resolve(&step.agent) == agent {
                        step.agent = Some(default_agent.as_str().to_string());
                    }
                }
                frontend.record_fallback(&requested, default_agent);
            }
            _ => {
                return Err(CommandError::Other(format!(
                    "workflow step agent '{agent}' has no image; set it up with \
                     `awman ready --agent {agent}` and run the workflow again"
                )));
            }
        }
    }
    Ok(())
}

/// Import the image a workflow or squad step needs. If Apple Containers has
/// lost image content and the matching builtin builder is enabled, rebuild
/// once and retry before starting the step.
async fn import_agent_image_for_step(
    runtime: std::sync::Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine>,
    git_root: &std::path::Path,
    agent: &str,
    session: &crate::data::session::Session,
    definition: crate::engine::agent::ResolvedAgent,
    sink: &mut dyn UserMessageSink,
) -> Result<(), CommandError> {
    use crate::engine::image_prep::{AgentImagePreparer, ImageBuildContext};
    let preparer = AgentImagePreparer::new(
        session.effective_config().builtin_settings()?,
        runtime,
        crate::data::fs::data_paths::data_home(session.effective_config().env())?,
        // The context's default agent is the session's configured one, not the
        // agent being prepared: a non-default agent must not reuse a
        // single-image default source, exactly as `ready` refuses it.
        ImageBuildContext::for_repo(
            git_root,
            &session.effective_config(),
            session.configured_agent().as_str(),
        ),
    )
    .with_definition_env(session.env().clone())
    .with_definition(definition);
    let tag = crate::data::image_tags::agent_image_tag(git_root, agent);
    prepare_importing_step_with_recovery(preparer, &tag, agent, sink).await
}

async fn prepare_importing_step_with_recovery(
    mut preparer: crate::engine::image_prep::AgentImagePreparer,
    tag: &str,
    agent: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<(), CommandError> {
    use crate::engine::image_prep::{AgentImageRequest, BuildPolicy, MessageSink};
    let result = preparer
        .prepare(
            &AgentImageRequest {
                tag,
                agent,
                policy: BuildPolicy::ImportOnly,
            },
            &mut MessageSink(sink),
        )
        .await?;
    if result.import_failure
        == Some(crate::data::ready_image::ReadyImageImportFailure::AppleContentMissing)
        && preparer.missing_image_plan(tag, agent)
            == crate::data::ready_image::ReadyImagePlan::BuildAndImport
    {
        sink.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: format!(
                "Apple Containers is missing content for agent '{agent}'; rebuilding its image before starting the step"
            ),
        });
        let repaired = preparer
            .prepare(
                &AgentImageRequest {
                    tag,
                    agent,
                    policy: BuildPolicy::Ready {
                        build: true,
                        no_cache: true,
                        refresh: false,
                    },
                },
                &mut MessageSink(sink),
            )
            .await?;
        if let crate::data::step_status::StepStatus::Failed(error) = repaired.build {
            return Err(CommandError::Other(format!(
                "failed to rebuild image for agent '{agent}': {error}"
            )));
        }
        if let crate::data::step_status::StepStatus::Failed(error) = repaired.import {
            return Err(CommandError::Other(format!(
                "failed to import rebuilt image for agent '{agent}': {error}"
            )));
        }
        return Ok(());
    }
    match result.import {
        crate::data::step_status::StepStatus::Failed(error) => {
            Err(CommandError::Other(if result.source_kind.is_some() {
                format!("failed to import image for agent '{agent}': {error}")
            } else {
                error
            }))
        }
        // A step needs this agent's image. `ready` may skip a non-default
        // agent whose default source names a single image (it is optional
        // there); a workflow step running that agent cannot.
        crate::data::step_status::StepStatus::Warn(reason) => Err(CommandError::Other(format!(
            "cannot import image for agent '{agent}': {reason}"
        ))),
        _ => Ok(()),
    }
}

/// [`ensure_agent_image`] with the raw build output optionally redirected to a
/// [`BuildOutputTarget`] instead of streaming through `sink`. Lifecycle
/// messages still go to `sink` either way.
pub(crate) async fn ensure_agent_image_with_build_output(
    engines: &Engines,
    git_root: &std::path::Path,
    paths: &crate::data::RepoDockerfilePaths,
    agent: &str,
    session: &crate::data::session::Session,
    sink: &mut dyn UserMessageSink,
    mut build_output: Option<&mut dyn BuildOutputTarget>,
) -> Result<(), CommandError> {
    let name = crate::data::session::AgentName::new(agent).map_err(EngineError::Data)?;
    let definition = engines.agent_engine.resolve_definition(session, &name)?;
    let runtime = engines
        .require_container_runtime()
        .map_err(CommandError::from)?;
    if runtime.capabilities().image_acquisition
        == crate::engine::agent_runtime::ImageAcquisition::Import
    {
        let runtime: std::sync::Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> =
            runtime.clone();
        return import_agent_image_for_step(runtime, git_root, agent, session, definition, sink)
            .await;
    }
    let dockerfile = paths.agent_dockerfile(agent);
    let sync = crate::engine::agent::provision_agent_dockerfile(
        &definition,
        paths,
        &crate::data::image_tags::project_image_tag(git_root),
    )
    .await?;
    for warning in &sync.warnings {
        sink.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: warning.clone(),
        });
    }
    if !dockerfile.exists() {
        return Err(CommandError::Other(format!(
            "agent '{agent}' has no Dockerfile (expected {})",
            dockerfile.display()
        )));
    }
    let tag = crate::data::image_tags::agent_image_tag(git_root, agent);
    if runtime.image_exists(&tag) {
        if sync.definition_changed() && definition.is_custom() {
            sink.write_message(UserMessage { level: MessageLevel::Warning, text: format!("agent definition '{agent}' changed since its image was built; run 'awman ready --agent {agent} --build'") });
        }
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
            &crate::engine::container::BuildRequest::runtime(&tag, &dockerfile, git_root, false),
            &mut |line: &str| match build_output.as_mut() {
                Some(target) => target.line(line),
                None => sink.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: line.to_string(),
                }),
            },
        )
        .map_err(|e| {
            let reason = crate::engine::agent::custom_build_failure(&definition, e);
            CommandError::Other(format!(
                "failed to build image for agent '{agent}' from {}: {reason}",
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

#[cfg(test)]
mod static_image_tests {
    use super::*;
    use crate::command::commands::agent_setup::{AgentSetupDecision, AgentSetupFrontend};
    use crate::data::image_tags::{agent_image_tag, project_image_tag};
    use crate::data::session::AgentName;
    use crate::engine::agent_runtime::{
        AgentHandle, AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities,
        ResolvedAgentOptions,
    };
    use std::collections::HashSet;
    use std::sync::Arc;

    /// A building runtime whose image store holds exactly `present`.
    struct Building {
        caps: Capabilities,
        present: HashSet<String>,
    }

    impl AgentRuntimeEngine for Building {
        fn runtime_name(&self) -> &'static str {
            "fake-build"
        }
        fn display_name(&self) -> &'static str {
            "Fake build"
        }
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }
        fn is_available(&self) -> bool {
            true
        }
        fn build(&self, _: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
            unreachable!()
        }
        fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
            unreachable!()
        }
        fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
            unreachable!()
        }
        fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
            unreachable!()
        }
        fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
            unreachable!()
        }
        fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
            unreachable!()
        }
        fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
            unreachable!()
        }
        fn ready_agent(
            &self,
            _: &str,
            _: crate::engine::agent_runtime::ReadyAgentOptions,
            _: &mut dyn UserMessageSink,
        ) -> Result<(), EngineError> {
            unreachable!()
        }
        fn image_exists(&self, tag: &str) -> Result<bool, EngineError> {
            Ok(self.present.contains(tag))
        }
        fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
            Ok(None)
        }
        fn build_image(
            &self,
            _: &crate::engine::container::BuildRequest,
            _: &mut dyn FnMut(&str),
        ) -> Result<(), EngineError> {
            panic!("these tests never reach a build")
        }
    }

    /// Answers every question with `decision` and records what it was asked.
    struct Answering {
        decision: AgentSetupDecision,
        /// `(requested, default, default_available, image_only)` per question.
        asked: Vec<(String, String, bool, bool)>,
        fallbacks: Vec<(String, String)>,
    }

    impl Answering {
        fn new(decision: AgentSetupDecision) -> Self {
            Self {
                decision,
                asked: Vec::new(),
                fallbacks: Vec::new(),
            }
        }
    }

    impl UserMessageSink for Answering {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl AgentSetupFrontend for Answering {
        fn ask_agent_setup(
            &mut self,
            requested: &AgentName,
            default: &AgentName,
            default_available: bool,
            image_only: bool,
        ) -> Result<AgentSetupDecision, CommandError> {
            self.asked.push((
                requested.as_str().into(),
                default.as_str().into(),
                default_available,
                image_only,
            ));
            Ok(self.decision)
        }
        fn record_fallback(&mut self, requested: &AgentName, fallback: &AgentName) {
            self.fallbacks
                .push((requested.as_str().into(), fallback.as_str().into()));
        }
    }

    const WORKFLOW: &str = r#"
[[steps]]
name = "plan"
prompt = "plan it"

[[steps]]
name = "build"
agent = "codex"
prompt = "build it"

[[steps]]
name = "review"
agent = "codex"
prompt = "review it"
"#;

    /// A repo, its session, and engines over a runtime holding the images of
    /// `present` agents (plus the project image unless `no_project`).
    fn fixture(
        present: &[&str],
        no_project: bool,
    ) -> (tempfile::TempDir, Session, Engines, Workflow) {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::for_tests_isolated(tmp.path(), &tmp.path().join("home"));
        let mut tags: HashSet<String> = present
            .iter()
            .map(|agent| agent_image_tag(tmp.path(), agent))
            .collect();
        if !no_project {
            tags.insert(project_image_tag(tmp.path()));
        }
        let mut engines = Engines::for_tests(tmp.path());
        engines.runtime = Arc::new(Building {
            caps: crate::engine::container::ContainerRuntime::docker()
                .capabilities()
                .clone(),
            present: tags,
        });
        let workflow = Workflow::parse(
            WORKFLOW,
            crate::data::workflow_definition::WorkflowFormat::Toml,
        )
        .unwrap();
        (tmp, session, engines, workflow)
    }

    fn claude() -> AgentName {
        AgentName::new("claude").unwrap()
    }

    async fn run(
        workflow: &mut Workflow,
        engines: &Engines,
        session: &Session,
        frontend: &mut Answering,
    ) -> Result<(), CommandError> {
        ensure_workflow_agent_images_with_default(workflow, engines, session, &claude(), frontend)
            .await
    }

    #[tokio::test]
    async fn every_image_present_asks_nothing() {
        let (_tmp, session, engines, mut workflow) = fixture(&["claude", "codex"], false);
        let mut frontend = Answering::new(AgentSetupDecision::Abort);
        run(&mut workflow, &engines, &session, &mut frontend)
            .await
            .unwrap();
        assert!(frontend.asked.is_empty());
    }

    #[tokio::test]
    async fn a_missing_agent_image_is_asked_about_once_and_abort_stops_the_run() {
        let (_tmp, session, engines, mut workflow) = fixture(&["claude"], false);
        let mut frontend = Answering::new(AgentSetupDecision::Abort);
        let err = run(&mut workflow, &engines, &session, &mut frontend)
            .await
            .unwrap_err();
        // Two codex steps, one question; no Dockerfile yet, default available.
        assert_eq!(
            frontend.asked,
            vec![("codex".into(), "claude".into(), true, false)]
        );
        assert!(
            err.to_string().contains("awman ready --agent codex"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn fallback_runs_those_steps_with_the_default_agent() {
        let (_tmp, session, engines, mut workflow) = fixture(&["claude"], false);
        let mut frontend = Answering::new(AgentSetupDecision::FallbackToDefault);
        run(&mut workflow, &engines, &session, &mut frontend)
            .await
            .unwrap();
        let agents: Vec<Option<&str>> = workflow
            .steps
            .iter()
            .map(|step| step.agent.as_deref())
            .collect();
        // The step that named no agent is untouched; it already ran claude.
        assert_eq!(agents, vec![None, Some("claude"), Some("claude")]);
        assert_eq!(
            frontend.fallbacks,
            vec![("codex".to_string(), "claude".to_string())]
        );
    }

    #[tokio::test]
    async fn no_fallback_is_offered_when_the_default_image_is_missing_too() {
        let (_tmp, session, engines, mut workflow) = fixture(&[], false);
        let mut frontend = Answering::new(AgentSetupDecision::FallbackToDefault);
        let err = run(&mut workflow, &engines, &session, &mut frontend)
            .await
            .unwrap_err();
        assert_eq!(
            frontend.asked,
            vec![("claude".into(), "claude".into(), false, false)],
            "the default agent's own image is asked about first, with no fallback"
        );
        assert!(err.to_string().contains("claude"), "{err}");
    }

    #[tokio::test]
    async fn an_existing_dockerfile_makes_it_an_image_only_question() {
        let (tmp, session, engines, mut workflow) = fixture(&["claude"], false);
        let paths = crate::data::RepoDockerfilePaths::new(tmp.path());
        std::fs::create_dir_all(paths.agent_dockerfile("codex").parent().unwrap()).unwrap();
        std::fs::write(paths.agent_dockerfile("codex"), "FROM scratch\n").unwrap();
        let mut frontend = Answering::new(AgentSetupDecision::Abort);
        let _ = run(&mut workflow, &engines, &session, &mut frontend).await;
        assert_eq!(
            frontend.asked,
            vec![("codex".into(), "claude".into(), true, true)]
        );
    }

    #[tokio::test]
    async fn setup_without_the_project_image_says_to_build_it_first() {
        let (_tmp, session, engines, mut workflow) = fixture(&["claude"], true);
        let mut frontend = Answering::new(AgentSetupDecision::Setup);
        let err = run(&mut workflow, &engines, &session, &mut frontend)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                CommandError::Engine(EngineError::AgentRequiresProjectImage { .. })
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn importing_runtimes_are_left_to_their_own_preparation() {
        let (_tmp, session, mut engines, mut workflow) = fixture(&[], false);
        let mut caps = engines.runtime.capabilities().clone();
        caps.image_acquisition = crate::engine::agent_runtime::ImageAcquisition::Import;
        engines.runtime = Arc::new(Building {
            caps,
            present: HashSet::new(),
        });
        let mut frontend = Answering::new(AgentSetupDecision::Abort);
        run(&mut workflow, &engines, &session, &mut frontend)
            .await
            .unwrap();
        assert!(frontend.asked.is_empty());
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;
    use crate::data::config::image_source::ImageSourceSpec;
    use crate::engine::agent_runtime::{
        AgentHandle, AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities, ImageAcquisition,
        ImageImportRequest, ImportedImage, ResolvedAgentOptions,
    };
    use std::sync::Arc;
    use std::sync::Mutex;

    /// An importing runtime that records imports and refuses to build.
    struct Importing {
        caps: Capabilities,
        present: bool,
        imports: Mutex<Vec<ImageImportRequest>>,
    }

    impl Importing {
        fn new(present: bool) -> Self {
            let mut caps = crate::engine::container::ContainerRuntime::docker()
                .capabilities()
                .clone();
            caps.image_acquisition = ImageAcquisition::Import;
            Self {
                caps,
                present,
                imports: Mutex::new(Vec::new()),
            }
        }
    }

    impl AgentRuntimeEngine for Importing {
        fn runtime_name(&self) -> &'static str {
            "fake-import"
        }
        fn display_name(&self) -> &'static str {
            "Fake import"
        }
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }
        fn is_available(&self) -> bool {
            true
        }
        fn build(&self, _: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
            unreachable!()
        }
        fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
            unreachable!()
        }
        fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
            unreachable!()
        }
        fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
            unreachable!()
        }
        fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
            unreachable!()
        }
        fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
            unreachable!()
        }
        fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
            unreachable!()
        }
        fn ready_agent(
            &self,
            _: &str,
            _: crate::engine::agent_runtime::ReadyAgentOptions,
            _: &mut dyn UserMessageSink,
        ) -> Result<(), EngineError> {
            unreachable!()
        }
        fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
            Ok(self.present)
        }
        fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
            Ok(None)
        }
        fn build_image(
            &self,
            _request: &crate::engine::container::BuildRequest,
            _: &mut dyn FnMut(&str),
        ) -> Result<(), EngineError> {
            panic!("an importing runtime must never build")
        }
        fn import_image(
            &self,
            request: &ImageImportRequest,
            _: &mut dyn UserMessageSink,
        ) -> Result<ImportedImage, EngineError> {
            self.imports.lock().unwrap().push(request.clone());
            Err(EngineError::Other("recorded".into()))
        }
    }

    struct Quiet;
    impl UserMessageSink for Quiet {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    async fn prepare_test_image(
        runtime: Arc<Importing>,
        git_root: &std::path::Path,
        paths: &crate::data::RepoDockerfilePaths,
        agent: &str,
        sources: &crate::engine::oci::ImageSources,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        use crate::engine::image_prep::*;
        let mut resolved = crate::data::config::EffectiveConfig::default()
            .builtin_settings()
            .unwrap();
        resolved.image_source = sources.default.clone();
        resolved.images = sources.images.clone();
        resolved.registries = sources.registries.clone();
        let mut preparer = AgentImagePreparer::new(
            resolved,
            runtime,
            git_root.join("data"),
            ImageBuildContext {
                git_root: git_root.to_path_buf(),
                project_dockerfile: paths.project_dockerfile(),
                default_agent: agent.into(),
            },
        );
        let tag = crate::data::image_tags::agent_image_tag(git_root, agent);
        let result = preparer
            .prepare(
                &AgentImageRequest {
                    tag: &tag,
                    agent,
                    policy: BuildPolicy::ImportOnly,
                },
                &mut MessageSink(sink),
            )
            .await?;
        match result.import {
            crate::data::step_status::StepStatus::Failed(error) => Err(EngineError::Other(error)),
            _ => Ok(()),
        }
    }

    fn sources(spec: Option<ImageSourceSpec>) -> crate::engine::oci::ImageSources {
        crate::engine::oci::ImageSources {
            default: spec,
            images: Default::default(),
            registries: Default::default(),
        }
    }

    #[tokio::test]
    async fn missing_apple_content_rebuilds_once_when_builder_is_enabled() {
        use crate::engine::oci::SourceImage;
        use crate::engine::ready::tests::fakes::{FakeImportRuntime, FakeLocalBuilder};
        use crate::engine::ready::tests::{builder_engine, built_identity};

        let runtime = Arc::new(FakeImportRuntime::new(false));
        runtime.fail_next_apple_content();
        let builder = Arc::new(FakeLocalBuilder::new());
        let (engine, _frontend, root, _, _) = builder_engine(
            runtime.clone(),
            false,
            false,
            sources(Some(ImageSourceSpec::AppleStore { reference: None })),
            builder.clone(),
        );
        let tag = crate::data::image_tags::agent_image_tag(root.path(), "claude");
        let base = crate::data::image_tags::project_image_tag(root.path());
        let platform = crate::data::oci_identity::OciPlatform::host_linux();
        builder.script_build(&base, Ok(built_identity('b', platform.clone())));
        builder.script_inspect(&base, Ok(SourceImage::Unbound));
        builder.script_build(&tag, Ok(built_identity('a', platform)));

        let (_, preparer, _) = engine.into_image_test_parts();
        prepare_importing_step_with_recovery(preparer, &tag, "claude", &mut Quiet)
            .await
            .unwrap();
        assert_eq!(runtime.imports.lock().unwrap().len(), 2);
        let builds = builder.build_calls();
        assert_eq!(builds.len(), 2);
        assert!(builds.iter().all(|build| build.no_cache));
    }

    #[tokio::test]
    async fn other_apple_import_failures_do_not_trigger_a_rebuild() {
        use crate::engine::ready::tests::builder_engine;
        use crate::engine::ready::tests::fakes::{FakeImportRuntime, FakeLocalBuilder};

        let runtime = Arc::new(FakeImportRuntime::new(false));
        runtime.fail_next_import(1);
        let builder = Arc::new(FakeLocalBuilder::new());
        let (engine, _frontend, root, _, _) = builder_engine(
            runtime.clone(),
            false,
            false,
            sources(Some(ImageSourceSpec::AppleStore { reference: None })),
            builder.clone(),
        );
        let tag = crate::data::image_tags::agent_image_tag(root.path(), "claude");
        let (_, preparer, _) = engine.into_image_test_parts();
        let error = prepare_importing_step_with_recovery(preparer, &tag, "claude", &mut Quiet)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("fake import failure"));
        assert_eq!(runtime.imports.lock().unwrap().len(), 1);
        assert!(builder.build_calls().is_empty());
    }

    #[tokio::test]
    async fn missing_apple_content_without_builder_reports_repair() {
        use crate::engine::image_prep::{AgentImagePreparer, ImageBuildContext};
        use crate::engine::ready::tests::fakes::FakeImportRuntime;

        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(FakeImportRuntime::new(false));
        runtime.fail_next_apple_content();
        let mut settings = crate::data::config::EffectiveConfig::default()
            .builtin_settings()
            .unwrap();
        settings.image_source = Some(ImageSourceSpec::AppleStore { reference: None });
        settings.builder_enabled = false;
        let preparer = AgentImagePreparer::new(
            settings,
            runtime.clone(),
            root.path().join("data"),
            ImageBuildContext {
                git_root: root.path().to_path_buf(),
                project_dockerfile: root.path().join("Dockerfile.dev"),
                default_agent: "claude".into(),
            },
        );
        let tag = crate::data::image_tags::agent_image_tag(root.path(), "claude");
        let error = prepare_importing_step_with_recovery(preparer, &tag, "claude", &mut Quiet)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Rebuild or re-pull"), "{error}");
        assert_eq!(runtime.imports.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn importing_runtime_imports_from_the_configured_source_and_never_builds() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::data::RepoDockerfilePaths::new(root.path());
        let runtime = Arc::new(Importing::new(false));
        let archive = ImageSourceSpec::Archive {
            path: "/images/agent.tar".into(),
        };
        let err = prepare_test_image(
            runtime.clone(),
            root.path(),
            &paths,
            "claude",
            &sources(Some(archive.clone())),
            &mut Quiet,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("recorded"), "{err}");
        let imports = runtime.imports.lock().unwrap();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].source, archive);
        assert_eq!(
            imports[0].tag,
            crate::data::image_tags::agent_image_tag(root.path(), "claude")
        );
    }

    /// WI 0126 A1: the import request carries the registries the source was
    /// resolved with, so the backend pulls with the session's settings rather
    /// than any process-wide ones.
    #[tokio::test]
    async fn import_request_carries_the_sessions_registries() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::data::RepoDockerfilePaths::new(root.path());
        let runtime = Arc::new(Importing::new(false));
        let mut configured = sources(Some(ImageSourceSpec::Registry {
            registry: Some("registry.example:5000".into()),
            reference: Some("team/agent:1".into()),
        }));
        configured.registries.insert(
            "registry.example:5000".into(),
            crate::data::config::image_source::RegistryHostConfig {
                insecure: true,
                ..Default::default()
            },
        );
        let _ = prepare_test_image(
            runtime.clone(),
            root.path(),
            &paths,
            "claude",
            &configured,
            &mut Quiet,
        )
        .await;
        let imports = runtime.imports.lock().unwrap();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].registries, configured.registries);
    }

    #[tokio::test]
    async fn importing_runtime_keeps_a_present_image_and_reports_a_missing_source() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::data::RepoDockerfilePaths::new(root.path());
        let present = Arc::new(Importing::new(true));
        prepare_test_image(
            present.clone(),
            root.path(),
            &paths,
            "claude",
            &sources(None),
            &mut Quiet,
        )
        .await
        .unwrap();
        assert!(present.imports.lock().unwrap().is_empty());
        let missing = Arc::new(Importing::new(false));
        let err = prepare_test_image(
            missing.clone(),
            root.path(),
            &paths,
            "claude",
            &sources(None),
            &mut Quiet,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("no image source is configured"),
            "{err}"
        );
        assert!(missing.imports.lock().unwrap().is_empty());
    }

    /// C1: `workflow preflight`'s import request and `awman ready`'s image
    /// acquisition must resolve the same `(source, reference)` for the same
    /// tag and configured sources — both go through `resolve_source`, `ready`
    /// via `ImageSourceResolver::resolve_for_image`, which wraps it without
    /// changing `reference` or (for a non-Docker source) `source`. A
    /// preflight/ready divergence here would mean the two commands could
    /// import from different content for an identical tag.
    #[test]
    fn preflight_and_ready_resolve_the_same_source_and_reference_for_the_same_tag() {
        use crate::engine::oci::{BuildHint, ImageSourceResolver};
        let root = tempfile::tempdir().unwrap();
        let paths = crate::data::RepoDockerfilePaths::new(root.path());
        let tag = crate::data::image_tags::agent_image_tag(root.path(), "claude");
        let hint = crate::engine::oci::BuildHint {
            agent_dockerfile: paths.agent_dockerfile("claude"),
            project: Some((
                paths.project_dockerfile(),
                crate::data::image_tags::project_image_tag(root.path()),
            )),
            context: root.path().to_path_buf(),
        };

        for spec in [
            ImageSourceSpec::Archive {
                path: "/images/agent.tar".into(),
            },
            ImageSourceSpec::AppleStore { reference: None },
            ImageSourceSpec::Registry {
                registry: Some("ghcr.io".into()),
                reference: None,
            },
        ] {
            // The resolver and preparer must agree on the logical source and reference.
            let (preflight_source, preflight_reference) = crate::engine::oci::resolve_source(
                &tag,
                &sources(Some(spec.clone())),
                &hint,
                false,
            )
            .unwrap();

            // The path `ReadyEngine` takes: `ImageSourceResolver::resolve_for_image`,
            // which calls `resolve_source` internally and layers endpoint
            // resolution on top (a no-op for every non-Docker source kind).
            let resolved = ImageSourceResolver::new(
                &sources(Some(spec.clone())),
                &crate::data::config::env::EnvSnapshot::empty(),
            )
            .resolve_for_image(
                &tag,
                &BuildHint {
                    agent_dockerfile: hint.agent_dockerfile.clone(),
                    project: hint.project.clone(),
                    context: hint.context.clone(),
                },
                crate::data::oci_identity::OciPlatform::host_linux(),
            )
            .unwrap();

            assert_eq!(
                resolved.reference, preflight_reference,
                "reference must match for {spec:?}"
            );
            assert_eq!(
                resolved.source, preflight_source,
                "source must match for {spec:?}"
            );
        }
    }

    /// A default source naming a single image (an archive, or a fixed
    /// reference) represents only the configured default agent. Preflight
    /// for a step running any other agent refuses it, exactly as `ready`
    /// does, rather than importing the default agent's image under the other
    /// agent's tag.
    #[tokio::test]
    async fn preflight_refuses_a_single_image_default_source_for_another_agent() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".awman")).unwrap();
        std::fs::write(
            root.path().join(".awman/config.json"),
            r#"{"agents":{"default":"claude"},"runtime":{"builtin":{"imageSource":{"type":"archive","path":"/images/claude.tar"}}}}"#,
        )
        .unwrap();
        let session = Session::for_tests(root.path());
        assert_eq!(session.configured_agent().as_str(), "claude");
        let runtime = Arc::new(Importing::new(true));

        let err = import_agent_image_for_step(
            runtime.clone(),
            root.path(),
            "codex",
            &session,
            crate::engine::agent::AgentRegistry::resolve_for_session(
                &session,
                &crate::data::session::AgentName::new("codex").unwrap(),
            )
            .unwrap(),
            &mut Quiet,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("names a single image"), "{err}");
        assert!(
            runtime.imports.lock().unwrap().is_empty(),
            "codex must not be imported from claude's archive"
        );

        // The configured default agent still imports from it.
        let _ = import_agent_image_for_step(
            runtime.clone(),
            root.path(),
            "claude",
            &session,
            crate::engine::agent::AgentRegistry::resolve_for_session(
                &session,
                &crate::data::session::AgentName::new("claude").unwrap(),
            )
            .unwrap(),
            &mut Quiet,
        )
        .await;
        assert_eq!(runtime.imports.lock().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod custom_definition_tests {
    use super::*;
    #[test]
    fn deleted_custom_definition_is_unknown_even_with_a_rendered_dockerfile() {
        let temp = tempfile::tempdir().unwrap();
        let session = Session::for_tests_isolated(temp.path(), &temp.path().join("global"));
        let paths = crate::data::RepoDockerfilePaths::new(temp.path());
        std::fs::create_dir_all(paths.awman_dir()).unwrap();
        std::fs::write(
            paths.agent_dockerfile("pi"),
            "# awman-generated from repo:deleted\nFROM project\n",
        )
        .unwrap();
        let workflow = Workflow::parse(
            "agent = 'pi'\n[[steps]]\nname = 'test'\nprompt = 'do it'\n",
            crate::data::workflow_definition::WorkflowFormat::Toml,
        )
        .unwrap();
        let error = resolve_and_validate_workflow_agents(&workflow, &session, &paths).unwrap_err();
        assert!(error.contains("unknown agent 'pi'"), "{error}");
    }
}
