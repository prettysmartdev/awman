//! Admission runs before allocating a clone or launching any container.
use crate::data::config::overlays::{parse_overlay_list, SkillSpec, TypedOverlay};
use crate::data::config::{
    daemon::{Interaction, WorkerSettings},
    flags::{IdentityFlag, PrerequisiteFlags, SshFlag, TokenFlag},
    work_servers::ResolvedWorkServer,
    workflows::DynamicConfig,
};
use crate::data::secret::SecretName;
use crate::data::workflow_definition::{Workflow, WorkflowFormat};
pub use crate::engine::overlay::SkillStatus;
use crate::engine::worker::lease_workspace::LeaseRuntimeSupport;
use awman_wsp::{WorkItem, WorkKind};
use std::collections::BTreeMap;

/// Host facts captured for admission; foreground hosts supply no lease features.
pub struct LeaseHostView<'a> {
    pub features: &'a [&'a str],
    pub worker_env: &'a dyn Fn(&str) -> Option<String>,
    pub skills: &'a dyn Fn(&str) -> SkillStatus,
    pub prerequisite_policy: Option<&'a crate::data::config::workflows::PrerequisitePolicy>,
}

const DYNAMIC_ONLY: &str = "exec.workflows_dynamic requires a workflow item with exec.dynamic=true";

fn require_feature(host: &LeaseHostView<'_>, feature: &str) -> Result<(), String> {
    if host.features.contains(&feature) {
        Ok(())
    } else {
        Err(format!("this worker does not support {feature}"))
    }
}

fn malformed(entry: &str) -> String {
    let entry: String = entry.chars().take(128).collect();
    format!("lease overlay '{entry}' is malformed; expected env(NAME) or skill(name)")
}

fn overlay_parts(entry: &str) -> Result<(&str, &str), String> {
    let (kind, tail) = entry.split_once('(').ok_or_else(|| malformed(entry))?;
    let name = tail.strip_suffix(')').ok_or_else(|| malformed(entry))?;
    if kind.is_empty()
        || !kind.bytes().all(|b| b.is_ascii_lowercase())
        || name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '(' | ')' | ','))
    {
        return Err(malformed(entry));
    }
    Ok((kind, name))
}

fn review_env(
    secret_names: &[String],
    name: &str,
    server: &ResolvedWorkServer,
    host: &LeaseHostView<'_>,
) -> Result<(), String> {
    require_feature(host, awman_wsp::features::LEASE_SECRETS)?;
    crate::engine::container::host_env_denylist::validate_caller_env_name(name)?;
    if SecretName::parse(name).is_ok()
        && (secret_names.iter().any(|secret| secret == name)
            || (server.allow_env.iter().any(|n| n == name) && (host.worker_env)(name).is_some()))
    {
        Ok(())
    } else {
        Err(format!("lease overlay env({name}) refused: {name} is not in exec.secrets and is not a set worker variable listed in allowEnv"))
    }
}

fn review_skill(
    name: &str,
    server: &ResolvedWorkServer,
    host: &LeaseHostView<'_>,
) -> Result<(), String> {
    require_feature(host, awman_wsp::features::LEASE_SKILLS)?;
    if crate::data::config::work_servers::validate_allow_skill_entry(name).is_err() {
        return Err(malformed(&format!("skill({name})")));
    }
    match (host.skills)(name) {
        SkillStatus::Missing => Err(format!(
            "lease overlay skill({name}) refused: {name} is not installed on this worker"
        )),
        SkillStatus::Installed { default_library } => {
            if default_library
                || server.allow_skills.iter().any(|allowed| {
                    allowed == name
                        || name
                            .strip_prefix(allowed)
                            .is_some_and(|tail| tail.starts_with('/'))
                })
            {
                Ok(())
            } else {
                Err(format!("lease overlay skill({name}) refused: {name} is not in the default skill library or allowSkills"))
            }
        }
    }
}

fn review_overlays(
    item: &WorkItem,
    server: &ResolvedWorkServer,
    host: &LeaseHostView<'_>,
) -> Result<Vec<String>, String> {
    let mut accepted = Vec::new();
    for entry in &item.exec.overlays {
        let (kind, name) = overlay_parts(entry)?;
        let expected = match kind {
            "env" => { review_env(&item.exec.secrets.names(), name, server, host)?; TypedOverlay::Env(name.into()) }
            "skill" => { review_skill(name, server, host)?; TypedOverlay::Skill(SkillSpec::Named(name.into())) }
            "context" => return Err("lease overlay kind 'context' is not supported; leases receive context through the work item's context field".into()),
            _ => return Err(format!("lease overlay kind '{kind}' is not supported; a lease may request env(NAME) and skill(name) only")),
        };
        if parse_overlay_list(entry).map_err(|_| malformed(entry))? != [expected] {
            return Err(malformed(entry));
        }
        if !accepted.contains(entry) {
            accepted.push(entry.clone());
        }
    }
    Ok(accepted)
}

fn review_workflow_overlays(
    workflow: &Workflow,
    secret_names: &[String],
    server: &ResolvedWorkServer,
    host: &LeaseHostView<'_>,
) -> Result<(), String> {
    let review = || -> Result<(), String> {
        for entry in workflow.overlays.iter().flatten().chain(
            workflow
                .steps
                .iter()
                .flat_map(|step| step.overlays.iter().flatten()),
        ) {
            for overlay in parse_overlay_list(entry)? {
                match overlay {
                    TypedOverlay::Env(name) => review_env(secret_names, &name, server, host)?,
                    TypedOverlay::Skill(SkillSpec::Named(name)) => {
                        review_skill(&name, server, host)?
                    }
                    TypedOverlay::Skill(SkillSpec::All) => {
                        return Err(
                            "lease overlay skill(*) is not supported; request skills by name"
                                .into(),
                        )
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    };
    review().map_err(|error| format!("workflow_toml: {error}"))
}

/// Turn `exec.prerequisites` into the `--prerequisite-*` flags push accepts,
/// refusing any choice the worker's own global policy does not grant. The
/// refusal is decided by the same `check_flag_widening` push uses (which also
/// honours `workflows.prerequisites.allowApiClientGrants`); `"allow"` only ever
/// selects a credential that policy already names.
fn lease_prerequisites(
    wire: &awman_wsp::ExecPrerequisites,
    host: &LeaseHostView<'_>,
) -> Result<(PrerequisiteFlags, Vec<String>), String> {
    use crate::command::commands::workflow_prerequisites as prerequisites;
    use awman_wsp::ExecCredentialChoice as Choice;
    let policy = host.prerequisite_policy.ok_or_else(|| {
        format!(
            "this worker does not support {}",
            awman_wsp::features::LEASE_PREREQUISITES
        )
    })?;
    let code = awman_wsp::codes::PREREQUISITE_WIDENING_REFUSED;
    let not_granted = |field: &str| {
        format!("{code}: exec.prerequisites.{field}=allow is not granted by this worker's workflows.prerequisites policy")
    };
    let mut flags = PrerequisiteFlags::default();
    let mut args = Vec::new();
    if let Some(identity) = &wire.git_identity {
        prerequisites::validate_identity(&identity.name, &identity.email)
            .map_err(|reason| format!("exec.prerequisites.git_identity is invalid: {reason}"))?;
        let text = format!("{} <{}>", identity.name, identity.email);
        let fixed = IdentityFlag::Fixed {
            name: identity.name.clone(),
            email: identity.email.clone(),
        };
        // The in-run parser must read back exactly this identity.
        if prerequisites::parse_identity(&text).as_ref() != Ok(&fixed) {
            return Err("exec.prerequisites.git_identity is invalid: the name and email must not contain '<' or '>' or surrounding whitespace".into());
        }
        flags.identity = Some(fixed);
        args.extend([format!("--{}", prerequisites::FLAG_IDENTITY), text]);
    }
    if let Some(choice) = wire.ssh {
        let (flag, text) = match choice {
            Choice::Deny => (SshFlag::Deny, "deny".to_string()),
            Choice::Allow if policy.ssh.whole_dir => (SshFlag::All, "all".to_string()),
            Choice::Allow => match (&policy.ssh.key, policy.ssh.key_is_suggestion) {
                (Some(key), false) => {
                    let text = key.to_str().ok_or_else(|| not_granted("ssh"))?.to_string();
                    (SshFlag::Key(key.clone()), text)
                }
                _ => return Err(not_granted("ssh")),
            },
        };
        if prerequisites::parse_ssh(&text).as_ref() != Ok(&flag) {
            return Err(not_granted("ssh"));
        }
        flags.ssh = Some(flag);
        args.extend([format!("--{}", prerequisites::FLAG_SSH), text]);
    }
    if let Some(choice) = wire.github_token {
        let global_acquire =
            policy.github_token.acquire.as_ref().is_some_and(|a| {
                a.scope == crate::data::workflow_prerequisites::AcquireScope::Global
            });
        let (flag, text) = match choice {
            Choice::Deny => (TokenFlag::Deny, "deny".to_string()),
            Choice::Allow => match policy.github_token.env_vars.first() {
                Some(var) => (TokenFlag::Env(var.clone()), format!("env:{var}")),
                None if global_acquire => (TokenFlag::Acquire, "acquire".to_string()),
                None => return Err(not_granted("github_token")),
            },
        };
        if prerequisites::parse_token(&text).as_ref() != Ok(&flag) {
            return Err(not_granted("github_token"));
        }
        flags.token = Some(flag);
        args.extend([format!("--{}", prerequisites::FLAG_TOKEN), text]);
    }
    // An `allow` the policy names but does not itself allow is decided below,
    // by the operator's allowApiClientGrants setting, exactly as for push.
    prerequisites::check_flag_widening(
        crate::command::dispatch::catalogue::FrontendKind::WORKER_LOCAL,
        &flags,
        policy,
    )
    .map_err(|error| format!("{code}: {error}"))?;
    Ok((flags, args))
}

/// Validate `exec.workflows_dynamic` like the `workflows.dynamic` config keys.
fn lease_workflows_dynamic(
    wire: &awman_wsp::ExecWorkflowsDynamic,
) -> Result<DynamicConfig, String> {
    for agent in wire.agents_to_models.iter().flat_map(BTreeMap::keys) {
        if crate::data::session::AgentName::new(agent.as_str()).is_err() {
            let agent: String = agent.chars().take(64).collect();
            return Err(format!(
                "exec.workflows_dynamic.agents_to_models.{agent} is not a valid agent name"
            ));
        }
    }
    let config = DynamicConfig {
        default_leader: wire.default_leader.clone(),
        agents_to_models: wire.agents_to_models.clone(),
        guidance: wire.guidance.clone(),
    };
    config
        .validate("exec.workflows_dynamic")
        .map_err(|error| error.to_string())?;
    Ok(config)
}

/// Admission policy retained in memory for generated and resumed workflows.
/// It carries secret names only, never values or worker environment values.
#[derive(Debug)]
pub struct LeaseWorkflowReview {
    server: ResolvedWorkServer,
    features: Vec<String>,
    secret_names: Vec<String>,
}

impl LeaseWorkflowReview {
    /// Foreground handoffs advertise no lease env/skill features. Apply the
    /// same reviewer even though the handoff carries no server allowlists.
    pub fn foreground() -> Self {
        Self {
            server: crate::data::config::work_servers::WorkServerConfig::default().resolved(),
            features: Vec::new(),
            secret_names: Vec::new(),
        }
    }

    pub fn review(
        &self,
        workflow: &Workflow,
        env: &crate::data::config::env::EnvSnapshot,
    ) -> Result<(), String> {
        validate_workflow(workflow)?;
        let features: Vec<_> = self.features.iter().map(String::as_str).collect();
        review_workflow_overlays(
            workflow,
            &self.secret_names,
            &self.server,
            &LeaseHostView {
                features: &features,
                worker_env: &|name| env.host_payload(name),
                skills: &|name| {
                    crate::engine::overlay::OverlayEngine::lease_skill_status(env, name)
                },
                prerequisite_policy: None,
            },
        )
    }
}

#[derive(Debug)]
pub struct LeasePlan {
    pub workflow_review: Option<std::sync::Arc<LeaseWorkflowReview>>,
    pub interaction: Interaction,
    pub command: Vec<String>,
    pub args: Vec<String>,
    /// `exec.prerequisites` as admitted; its flags are also in `args`.
    pub prerequisites: Option<PrerequisiteFlags>,
    /// `exec.workflows_dynamic`, validated; the runner installs it as the
    /// lease session's task config layer.
    pub workflows_dynamic: Option<DynamicConfig>,
}

pub(super) fn delivery(
    lease: &awman_wsp::Lease,
    plan: &LeasePlan,
    terminal_relay: bool,
) -> Result<(), String> {
    if lease.remote_input && !lease.terminal {
        return Err("remote_input requires a terminal lease".into());
    }
    if lease.terminal && (!terminal_relay || plan.interaction.is_headless()) {
        return Err(
            "this execution host cannot relay the requested interactive lease terminal".into(),
        );
    }
    Ok(())
}

pub fn validate_workflow(workflow: &Workflow) -> Result<(), String> {
    // Host teardown transport has no lease fence. Fail closed instead of
    // allowing a workflow to bypass the lease runner's push lifecycle.
    if !workflow.setup.is_empty() || !workflow.teardown.is_empty() {
        return Err("lease workflows currently require empty setup and teardown phases; use the lease push field for publication".into());
    }
    Ok(())
}

pub fn plan(
    item: &WorkItem,
    labels: &BTreeMap<String, String>,
    server: &ResolvedWorkServer,
    settings: &WorkerSettings,
    support: &LeaseRuntimeSupport,
    runtime: &str,
    host: &LeaseHostView<'_>,
) -> Result<LeasePlan, String> {
    if !item.exec.secrets.is_empty() {
        require_feature(host, awman_wsp::features::LEASE_SECRETS)?;
    }
    for entry in &item.exec.overlays {
        let (kind, _) = overlay_parts(entry)?;
        match kind {
            "env" => require_feature(host, awman_wsp::features::LEASE_SECRETS)?,
            "skill" => require_feature(host, awman_wsp::features::LEASE_SKILLS)?,
            _ => {}
        }
    }
    if item.exec.prerequisites.is_some() {
        require_feature(host, awman_wsp::features::LEASE_PREREQUISITES)?;
    }
    if item.exec.workflows_dynamic.is_some() {
        require_feature(host, awman_wsp::features::LEASE_WORKFLOWS_DYNAMIC)?;
    }
    super::lease_context::admit(item, host.features)?;
    super::lease_workflow_context::admit(item, host.features)?;
    item.affinity.validate().map_err(|e| e.to_string())?;
    if !item.affinity.eligible(labels) {
        return Err("required affinity changed before execution".into());
    }
    awman_wsp::repo_url::validate(&item.repo.url).map_err(str::to_owned)?;
    let credentials = crate::engine::git::lease_transport::LeaseCredentials::from_wsp(
        item.repo.credentials.as_ref(),
    )
    .map_err(|e| e.to_string())?;
    if credentials.has_secret()
        && !reqwest::Url::parse(&item.repo.url)
            .is_ok_and(|url| matches!(url.scheme(), "https" | "http"))
    {
        return Err("lease credential objects require an HTTP repository URL".into());
    }
    if credentials
        .expires_at()
        .is_some_and(|at| at <= chrono::Utc::now())
    {
        return Err("initial lease repository credentials already expired".into());
    }
    support.check(item, runtime).map_err(|e| e.0)?;
    if item
        .outputs
        .as_ref()
        .is_some_and(|outputs| outputs.max_bytes > 32 * 1024 * 1024)
    {
        return Err("lease output limit exceeds this worker's 32 MiB upload bound".into());
    }
    if item.exec.timeout_ms == Some(0) {
        return Err("timeout_ms must be positive".into());
    }
    let overlays = review_overlays(item, server, host)?;
    for reference in std::iter::once(&item.repo.base_ref)
        .chain(std::iter::once(&item.repo.branch))
        .chain(item.repo.merge_refs.iter())
        .chain(
            item.push
                .iter()
                .map(|push| &push.branch)
                .filter(|branch| !branch.is_empty()),
        )
    {
        if reference.is_empty()
            || reference.starts_with('-')
            || reference.contains(['\n', '\r', '\0', ' ', ':', '~', '^', '?', '*', '[', '\\'])
            || reference.contains("..")
            || reference.contains("@{")
        {
            return Err("invalid lease Git reference".into());
        }
    }
    if let Some(leader) = &item.exec.leader {
        crate::data::config::leader_spec::LeaderSpec::parse(leader)
            .map_err(|_| "lease leader must use agent::model syntax".to_string())?;
    }
    let configured = server.interaction.unwrap_or(settings.pull_interaction);
    let requested = item.exec.interaction.map(|i| match i {
        awman_wsp::Interaction::Interactive => Interaction::Interactive,
        awman_wsp::Interaction::Headless => Interaction::Headless,
    });
    if requested.is_some_and(|i| i != configured) && !settings.pull_allow_override {
        return Err("the worker refuses this interaction override".into());
    }
    let interaction = requested.unwrap_or(configured);
    let mut args = vec![format!("--{}", interaction.as_str())];
    // Always explicit: the daemon profile defaults an absent flag to on, so
    // `exec.yolo: false` must be stated to be honoured.
    args.push(if item.exec.yolo {
        "--yolo".into()
    } else {
        "--yolo=false".into()
    });
    if let Some(agent) = &item.exec.agent {
        args.extend(["--agent".into(), agent.clone()]);
    }
    if let Some(model) = &item.exec.model {
        args.extend(["--model".into(), model.clone()]);
    }
    for overlay in overlays {
        args.extend(["--overlay".into(), overlay]);
    }
    let command = match item.kind {
        WorkKind::Prompt => {
            if item.exec.prerequisites.is_some() {
                return Err("exec.prerequisites is only valid on workflow items".into());
            }
            if item.exec.workflows_dynamic.is_some() {
                return Err(DYNAMIC_ONLY.into());
            }
            if item.exec.dynamic || item.exec.workflow_toml.is_some() || item.exec.leader.is_some()
            {
                return Err("prompt leases cannot select a workflow or leader".into());
            }
            // The body is carried by Session::inline_work_item(), never by
            // persisted command arguments or a host process command line.
            vec!["exec".into(), "prompt".into()]
        }
        WorkKind::Workflow => {
            if item.exec.workflows_dynamic.is_some() && !item.exec.dynamic {
                return Err(DYNAMIC_ONLY.into());
            }
            if item.exec.dynamic && !item.exec.yolo && interaction.is_headless() {
                return Err("dynamic workflows with exec.yolo=false require interactive execution; set daemon.features.worker.pull.interaction=interactive".into());
            }
            if let Some(toml) = &item.exec.workflow_toml {
                if item.exec.leader.is_some() {
                    return Err("workflow_toml cannot select a leader".into());
                }
                if item.exec.dynamic {
                    return Err("dynamic and workflow_toml are mutually exclusive".into());
                }
                let workflow =
                    Workflow::parse(toml, WorkflowFormat::Toml).map_err(|e| e.to_string())?;
                validate_workflow(&workflow)?;
                review_workflow_overlays(&workflow, &item.exec.secrets.names(), server, host)?;
                // The runner replaces this with a private file, never a clone file.
                args.push("LEASE_WORKFLOW_FILE".into());
            } else {
                if !item.exec.dynamic {
                    return Err("workflow lease requires dynamic or workflow_toml".into());
                }
                // No `--work-item`: the lease body is attached as the session's inline
                // work item, which stands in for the flag (validate_dynamic_flags).
                args.push("--dynamic".into());
                if let Some(leader) = &item.exec.leader {
                    args.extend(["--leader".into(), leader.clone()]);
                }
            }
            vec!["exec".into(), "workflow".into()]
        }
    };
    let prerequisites = match &item.exec.prerequisites {
        Some(wire) => {
            let (flags, flag_args) = lease_prerequisites(wire, host)?;
            args.extend(flag_args);
            Some(flags)
        }
        None => None,
    };
    let workflows_dynamic = item
        .exec
        .workflows_dynamic
        .as_ref()
        .map(lease_workflows_dynamic)
        .transpose()?;
    let refs: Vec<_> = command.iter().map(String::as_str).collect();
    crate::command::dispatch::catalogue::CommandCatalogue::get()
        .parse_raw_args(&refs, &args)
        .map_err(|e| e.to_string())?;
    Ok(LeasePlan {
        workflow_review: Some(std::sync::Arc::new(LeaseWorkflowReview {
            server: server.clone(),
            features: host
                .features
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
            secret_names: item.exec.secrets.names(),
        })),
        interaction,

        command,
        args,
        prerequisites,
        workflows_dynamic,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> WorkerSettings {
        WorkerSettings {
            enabled: true,
            max_concurrent_jobs: 1,
            push_enabled: false,
            push_interaction: Interaction::Headless,
            push_allow_override: false,
            push_work_dirs: vec![],
            push_work_dir_roots: vec![],
            pull_enabled: true,
            pull_interaction: Interaction::Headless,
            pull_allow_override: false,
            pull_work_root: None,
            work_servers: Default::default(),
        }
    }

    #[test]
    fn dynamic_workflow_leases_carry_no_placeholder_work_item() {
        let mut item = awman_wsp::conformance::work_item("dyn");
        item.kind = WorkKind::Workflow;
        item.exec.dynamic = true;
        let server = ResolvedWorkServer {
            url: Some("https://example.invalid".into()),
            enabled: true,
            claim: Default::default(),
            max_concurrent_jobs: 1,
            interaction: None,
            labels: BTreeMap::new(),
            allow_env: vec![],
            allow_skills: vec![],
        };
        let support = LeaseRuntimeSupport {
            read_only_workspace: false,
            outputs_mount: false,
            skill_mounts: false,
            secret_env: true,
        };
        let plan = plan(
            &item,
            &BTreeMap::new(),
            &server,
            &settings(),
            &support,
            "docker",
            &LeaseHostView {
                features: &[],
                worker_env: &|_| None,
                skills: &|_| SkillStatus::Missing,
                prerequisite_policy: None,
            },
        )
        .expect("dynamic lease plans");
        assert_eq!(plan.command, ["exec", "workflow"]);
        assert!(plan.args.iter().any(|arg| arg == "--dynamic"));
        // The lease body is the work item (Session::inline_work_item); a
        // placeholder reference would surface in resume hints and state keys.
        assert!(!plan.args.iter().any(|arg| arg == "--work-item"));
    }
}

#[cfg(test)]
mod lease_exec_tests {
    use super::*;

    const FEATURES: &[&str] = &[
        awman_wsp::features::LEASE_SECRETS,
        awman_wsp::features::LEASE_SKILLS,
    ];

    fn server() -> ResolvedWorkServer {
        crate::data::config::work_servers::WorkServerConfig::default().resolved()
    }
    fn host<'a>(features: &'a [&'static str]) -> LeaseHostView<'a> {
        LeaseHostView {
            features,
            worker_env: &|_| None,
            skills: &|_| SkillStatus::Missing,
            prerequisite_policy: None,
        }
    }
    fn secret(item: &mut WorkItem) {
        item.exec.secrets = awman_wsp::ExecSecrets::new([(
            "TOKEN".into(),
            crate::data::secret::SecretString::new("lease-private-token"),
        )])
        .unwrap();
    }

    #[test]
    fn env_review_prefers_the_secret_and_never_reads_the_host_value() {
        let mut item = awman_wsp::conformance::work_item("secret");
        secret(&mut item);
        item.exec.overlays = vec!["env(TOKEN)".into(), "env(TOKEN)".into()];
        let mut server = server();
        server.allow_env.push("TOKEN".into());
        let host = LeaseHostView {
            worker_env: &|_| panic!("secret wins"),
            ..host(FEATURES)
        };
        assert_eq!(
            review_overlays(&item, &server, &host).unwrap(),
            ["env(TOKEN)"]
        );
    }

    #[test]
    fn worker_env_requires_both_allowlisting_and_a_set_value_and_names_refusals() {
        let mut item = awman_wsp::conformance::work_item("env");
        item.exec.overlays = vec!["env(TOKEN)".into()];
        let mut server = server();
        let host = LeaseHostView {
            worker_env: &|_| Some("host-private-value".into()),
            ..host(FEATURES)
        };
        let error = review_overlays(&item, &server, &host).unwrap_err();
        assert!(error.contains("TOKEN") && error.contains("allowEnv"));
        assert!(!error.contains("host-private-value"));
        server.allow_env.push("TOKEN".into());
        assert!(review_overlays(&item, &server, &host).is_ok());
        assert!(
            review_overlays(&item, &server, &super::lease_exec_tests::host(FEATURES))
                .unwrap_err()
                .contains("TOKEN")
        );
        item.exec.overlays = vec!["env(lowercase)".into()];
        assert!(review_overlays(&item, &server, &host)
            .unwrap_err()
            .contains("lowercase"));
    }

    #[test]
    fn skills_require_installation_and_review_or_the_default_library() {
        let mut item = awman_wsp::conformance::work_item("skills");
        item.exec.overlays = vec!["skill(team/planner)".into()];
        let mut server = server();
        server.allow_skills.push("team".into());
        assert!(review_overlays(&item, &server, &host(FEATURES))
            .unwrap_err()
            .contains("team/planner is not installed"));
        let installed = LeaseHostView {
            skills: &|_| SkillStatus::Installed {
                default_library: false,
            },
            ..host(FEATURES)
        };
        assert!(review_overlays(&item, &server, &installed).is_ok());
        server.allow_skills.clear();
        assert!(review_overlays(&item, &server, &installed)
            .unwrap_err()
            .contains("team/planner is not in"));
        server.allow_skills.push("tea".into());
        assert!(review_overlays(&item, &server, &installed).is_err());
        let builtin = LeaseHostView {
            skills: &|_| SkillStatus::Installed {
                default_library: true,
            },
            ..host(FEATURES)
        };
        assert!(review_overlays(&item, &server, &builtin).is_ok());
    }

    #[test]
    fn overlay_kinds_and_ambiguous_grammar_fail_closed() {
        let mut item = awman_wsp::conformance::work_item("overlays");
        for (entry, reason) in [
            ("dir(/tmp:/target)", "kind 'dir'"),
            ("ssh()", "kind 'ssh'"),
            ("unknown(x)", "kind 'unknown'"),
            ("context(repo)", "context field"),
            ("host:container", "malformed"),
            ("env(A),env(B)", "malformed"),
            (" env(TOKEN)", "malformed"),
            ("skill(*)", "malformed"),
            ("skill(..)", "malformed"),
            ("skill(a/b/c)", "malformed"),
        ] {
            item.exec.overlays = vec![entry.into()];
            assert!(
                review_overlays(&item, &server(), &host(FEATURES))
                    .unwrap_err()
                    .contains(reason),
                "{entry}"
            );
        }
    }

    #[test]
    fn supplied_workflow_overlays_cannot_bypass_env_or_skill_review() {
        let mut item = awman_wsp::conformance::work_item("workflow");
        let mut workflow = Workflow::parse(
            r#"
overlays = ["env(TOKEN)"]
[[steps]]
name = "first"
prompt = "go"
overlays = ["skill(team/planner)"]
"#,
            WorkflowFormat::Toml,
        )
        .unwrap();
        assert!(review_workflow_overlays(
            &workflow,
            &item.exec.secrets.names(),
            &server(),
            &host(FEATURES)
        )
        .unwrap_err()
        .starts_with("workflow_toml: lease overlay env(TOKEN)"));
        secret(&mut item);
        assert!(review_workflow_overlays(
            &workflow,
            &item.exec.secrets.names(),
            &server(),
            &host(FEATURES)
        )
        .unwrap_err()
        .contains("team/planner"));
        let mut server = server();
        server.allow_skills.push("team".into());
        let installed = LeaseHostView {
            skills: &|_| SkillStatus::Installed {
                default_library: false,
            },
            ..host(FEATURES)
        };
        assert!(review_workflow_overlays(
            &workflow,
            &item.exec.secrets.names(),
            &server,
            &installed
        )
        .is_ok());
        assert!(review_workflow_overlays(
            &workflow,
            &item.exec.secrets.names(),
            &server,
            &host(&[])
        )
        .unwrap_err()
        .contains("lease_secrets"));
        workflow.overlays = Some(vec!["skill(*)".into()]);
        assert!(review_workflow_overlays(
            &workflow,
            &item.exec.secrets.names(),
            &server,
            &installed
        )
        .unwrap_err()
        .contains("skill(*)"));
    }

    #[test]
    fn featureless_hosts_refuse_each_exec_feature_before_other_admission() {
        let settings =
            crate::data::config::daemon::DaemonSettings::from_merged(&Default::default()).worker;
        let support = LeaseRuntimeSupport {
            read_only_workspace: true,
            outputs_mount: true,
            skill_mounts: true,
            secret_env: true,
        };
        for (wire, expected) in [
            (
                serde_json::json!({"secrets": {"UNUSED": "unused-private-value"}}),
                "lease_secrets",
            ),
            (
                serde_json::json!({"overlays": ["env(TOKEN)"]}),
                "lease_secrets",
            ),
            (
                serde_json::json!({"overlays": ["skill(team)"]}),
                "lease_skills",
            ),
            (
                serde_json::json!({"prerequisites": {}}),
                "lease_prerequisites",
            ),
            (
                serde_json::json!({"workflows_dynamic": {}}),
                "lease_workflows_dynamic",
            ),
        ] {
            let mut item = awman_wsp::conformance::work_item("unsupported");
            item.exec = serde_json::from_value(wire).unwrap();
            item.repo.url = "invalid-url".into();
            assert_eq!(
                plan(
                    &item,
                    &BTreeMap::new(),
                    &server(),
                    &settings,
                    &support,
                    "docker",
                    &host(&[])
                )
                .unwrap_err(),
                format!("this worker does not support {expected}")
            );
        }
    }

    #[test]
    fn accepted_overlays_are_name_only_args_and_unused_secrets_are_accepted() {
        let mut item = awman_wsp::conformance::work_item("accepted");
        secret(&mut item);
        let settings =
            crate::data::config::daemon::DaemonSettings::from_merged(&Default::default()).worker;
        let support = LeaseRuntimeSupport {
            read_only_workspace: true,
            outputs_mount: true,
            skill_mounts: true,
            secret_env: true,
        };
        let plan_item = |item: &WorkItem| {
            plan(
                item,
                &BTreeMap::new(),
                &server(),
                &settings,
                &support,
                "docker",
                &host(FEATURES),
            )
            .unwrap()
        };
        assert!(!plan_item(&item)
            .args
            .iter()
            .any(|arg| arg.contains("TOKEN")));
        item.exec.overlays = vec!["env(TOKEN)".into(), "env(TOKEN)".into()];
        let plan = plan_item(&item);
        assert_eq!(
            plan.args
                .windows(2)
                .filter(|args| args == &["--overlay", "env(TOKEN)"])
                .count(),
            1
        );
        assert!(!format!("{plan:?}").contains("lease-private-token"));
    }
}

#[cfg(test)]
mod lease_prerequisite_and_dynamic_tests {
    use super::*;
    use crate::data::config::workflows::{
        PrerequisitePolicy, PrerequisitePolicyLayers, PrerequisitesConfig,
    };
    use awman_wsp::features::{LEASE_PREREQUISITES, LEASE_WORKFLOWS_DYNAMIC};

    const FEATURES: &[&str] = &[LEASE_PREREQUISITES, LEASE_WORKFLOWS_DYNAMIC];

    fn policy(config: serde_json::Value) -> PrerequisitePolicy {
        let config: PrerequisitesConfig = serde_json::from_value(config).unwrap();
        PrerequisitePolicy::resolve(PrerequisitePolicyLayers {
            global: Some(&config),
            task_file: None,
            repo: None,
            task: None,
            flags: &Default::default(),
            context: Default::default(),
        })
    }
    fn workflow(exec: serde_json::Value) -> WorkItem {
        let mut item = awman_wsp::conformance::work_item("item");
        item.kind = WorkKind::Workflow;
        item.exec.dynamic = true;
        let mut value = serde_json::to_value(&item.exec).unwrap();
        for (key, field) in exec.as_object().unwrap() {
            value[key] = field.clone();
        }
        item.exec = serde_json::from_value(value).unwrap();
        item
    }
    fn admit(
        item: &WorkItem,
        policy: Option<&PrerequisitePolicy>,
        features: &[&str],
    ) -> Result<LeasePlan, String> {
        plan(
            item,
            &BTreeMap::new(),
            &crate::data::config::work_servers::WorkServerConfig::default().resolved(),
            &WorkerSettings {
                pull_enabled: true,
                ..crate::data::config::daemon::DaemonSettings::from_merged(&Default::default())
                    .worker
            },
            &LeaseRuntimeSupport {
                read_only_workspace: false,
                outputs_mount: false,
                skill_mounts: true,
                secret_env: true,
            },
            "docker",
            &LeaseHostView {
                features,
                worker_env: &|_| None,
                skills: &|_| SkillStatus::Missing,
                prerequisite_policy: policy,
            },
        )
    }
    fn tail(plan: &LeasePlan) -> Vec<&str> {
        let at = plan
            .args
            .iter()
            .position(|arg| arg.starts_with("--prerequisite-"))
            .unwrap_or(plan.args.len());
        plan.args[at..].iter().map(String::as_str).collect()
    }

    #[test]
    fn choices_within_the_worker_policy_become_push_flags() {
        let worker = policy(serde_json::json!({
            "ssh": {"use": "allow", "key": "/keys/id_lease"},
            "githubToken": {"use": "allow", "envVars": ["GH_TOKEN"]},
        }));
        let item = workflow(serde_json::json!({"prerequisites": {
            "git_identity": {"name": "Lease Bot", "email": "bot@example.test"},
            "ssh": "allow",
            "github_token": "allow",
        }}));
        let plan = admit(&item, Some(&worker), FEATURES).unwrap();
        assert_eq!(
            tail(&plan),
            [
                "--prerequisite-identity",
                "Lease Bot <bot@example.test>",
                "--prerequisite-ssh",
                "/keys/id_lease",
                "--prerequisite-token",
                "env:GH_TOKEN",
            ]
        );
        let flags = plan.prerequisites.unwrap();
        assert_eq!(
            flags.identity,
            Some(IdentityFlag::Fixed {
                name: "Lease Bot".into(),
                email: "bot@example.test".into()
            })
        );
        // Deny always narrows, whatever the worker grants.
        let deny = workflow(serde_json::json!({"prerequisites": {
            "git_identity": null, "ssh": "deny", "github_token": "deny",
        }}));
        let plan = admit(&deny, Some(&policy(serde_json::json!({}))), FEATURES).unwrap();
        assert_eq!(
            tail(&plan),
            ["--prerequisite-ssh", "deny", "--prerequisite-token", "deny"]
        );
        // Without exec.prerequisites nothing is added: worker config decides.
        let plain = workflow(serde_json::json!({}));
        let plan = admit(&plain, None, &[]).unwrap();
        assert!(plan.prerequisites.is_none());
        assert!(tail(&plan).is_empty());
    }

    #[test]
    fn widening_choices_are_refused_with_the_push_code_unless_api_grants_are_allowed() {
        let code = "prerequisite_widening_refused: ";
        let cases = [
            // A key the worker names but does not itself allow.
            (
                serde_json::json!({"ssh": {"use": "ask", "key": "/keys/id_lease"}}),
                serde_json::json!({"ssh": "allow"}),
            ),
            // A variable the worker lists but does not allow.
            (
                serde_json::json!({"githubToken": {"use": "deny", "envVars": ["GH_TOKEN"]}}),
                serde_json::json!({"github_token": "allow"}),
            ),
            // A fixed identity where the worker denies identities.
            (
                serde_json::json!({"gitIdentity": {"use": "deny"}}),
                serde_json::json!({"git_identity": {"name": "A", "email": "a@b.test"}}),
            ),
        ];
        for (config, choice) in cases {
            let item = workflow(serde_json::json!({"prerequisites": choice}));
            let error = admit(&item, Some(&policy(config.clone())), FEATURES).unwrap_err();
            assert!(error.starts_with(code), "{error}");
            // The operator's allowApiClientGrants is honoured, as for push.
            let mut granted = config;
            granted["allowApiClientGrants"] = true.into();
            admit(&item, Some(&policy(granted)), FEATURES).unwrap();
        }
        // "allow" never names a credential the worker's policy does not.
        for (field, choice) in [
            ("ssh", serde_json::json!({"ssh": "allow"})),
            ("github_token", serde_json::json!({"github_token": "allow"})),
        ] {
            let item = workflow(serde_json::json!({"prerequisites": choice}));
            // The default policy lists GH_TOKEN/GITHUB_TOKEN; name nothing.
            let worker = policy(serde_json::json!({
                "allowApiClientGrants": true,
                "githubToken": {"envVars": []},
            }));
            let error = admit(&item, Some(&worker), FEATURES).unwrap_err();
            assert_eq!(
                error,
                format!("{code}exec.prerequisites.{field}=allow is not granted by this worker's workflows.prerequisites policy")
            );
        }
        // No resolved worker policy (foreground host or unreadable config).
        let item = workflow(serde_json::json!({"prerequisites": {"ssh": "deny"}}));
        assert_eq!(
            admit(&item, None, FEATURES).unwrap_err(),
            "this worker does not support lease_prerequisites"
        );
    }

    #[test]
    fn invalid_identities_and_misplaced_fields_are_refused() {
        let worker = policy(serde_json::json!({}));
        for (name, email) in [
            ("A", "no-at"),
            ("", "a@b.test"),
            ("A <x", "a@b.test"),
            (" A", "a@b.test"),
        ] {
            let item = workflow(serde_json::json!({"prerequisites": {
                "git_identity": {"name": name, "email": email},
            }}));
            let error = admit(&item, Some(&worker), FEATURES).unwrap_err();
            assert!(
                error.starts_with("exec.prerequisites.git_identity is invalid: "),
                "{error}"
            );
        }
        let mut prompt = workflow(serde_json::json!({"prerequisites": {"ssh": "deny"}}));
        prompt.kind = WorkKind::Prompt;
        prompt.exec.dynamic = false;
        assert_eq!(
            admit(&prompt, Some(&worker), FEATURES).unwrap_err(),
            "exec.prerequisites is only valid on workflow items"
        );
        let mut prompt = workflow(serde_json::json!({"workflows_dynamic": {}}));
        prompt.kind = WorkKind::Prompt;
        prompt.exec.dynamic = false;
        assert_eq!(admit(&prompt, None, FEATURES).unwrap_err(), DYNAMIC_ONLY);
        let mut file = workflow(serde_json::json!({"workflows_dynamic": {}}));
        file.exec.dynamic = false;
        file.exec.workflow_toml = Some("[[steps]]\nname = \"a\"\nprompt = \"x\"\n".into());
        assert_eq!(admit(&file, None, FEATURES).unwrap_err(), DYNAMIC_ONLY);
    }

    #[test]
    fn dynamic_config_is_validated_like_the_config_keys() {
        let ok = workflow(serde_json::json!({"workflows_dynamic": {
            "default_leader": "codex::gpt-5",
            "agents_to_models": {"codex": ["gpt-5"]},
            "guidance": ["Keep steps small"],
        }}));
        let plan = admit(&ok, None, FEATURES).unwrap();
        let config = plan.workflows_dynamic.unwrap();
        assert_eq!(config.default_leader.as_deref(), Some("codex::gpt-5"));
        assert_eq!(config.guidance, Some(vec!["Keep steps small".to_string()]));
        // Nothing reaches the command line; it is the session's task layer.
        assert!(!plan.args.iter().any(|arg| arg.contains("gpt-5")));
        for (wire, needle) in [
            (
                serde_json::json!({"default_leader": "codex"}),
                "exec.workflows_dynamic.defaultLeader",
            ),
            (
                serde_json::json!({"agents_to_models": {"codex": []}}),
                "exec.workflows_dynamic.agentsToModels.codex",
            ),
            (
                serde_json::json!({"agents_to_models": {"bad agent": ["m"]}}),
                "exec.workflows_dynamic.agents_to_models.bad agent is not a valid agent name",
            ),
            (
                serde_json::json!({"guidance": [""]}),
                "exec.workflows_dynamic",
            ),
        ] {
            let item = workflow(serde_json::json!({"workflows_dynamic": wire}));
            let error = admit(&item, None, FEATURES).unwrap_err();
            assert!(error.contains(needle), "{error}");
        }
    }

    #[test]
    fn a_worker_without_the_features_refuses_both_fields() {
        let worker = policy(serde_json::json!({}));
        let item = workflow(serde_json::json!({"prerequisites": {"ssh": "deny"}}));
        assert_eq!(
            admit(&item, Some(&worker), &[LEASE_WORKFLOWS_DYNAMIC]).unwrap_err(),
            "this worker does not support lease_prerequisites"
        );
        let item = workflow(serde_json::json!({"workflows_dynamic": {"guidance": ["g"]}}));
        assert_eq!(
            admit(&item, None, &[LEASE_PREREQUISITES]).unwrap_err(),
            "this worker does not support lease_workflows_dynamic"
        );
    }
}
