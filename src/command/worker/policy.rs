//! Admission runs before allocating a clone or launching any container.
use crate::data::config::{
    daemon::{Interaction, WorkerSettings},
    work_servers::ResolvedWorkServer,
};
use crate::data::workflow_definition::{Workflow, WorkflowFormat};
use crate::engine::worker::lease_workspace::LeaseRuntimeSupport;
use awman_wsp::{WorkItem, WorkKind};
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct LeasePlan {
    pub interaction: Interaction,
    pub command: Vec<String>,
    pub args: Vec<String>,
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
) -> Result<LeasePlan, String> {
    let registered = super::capabilities::for_settings(*support, settings);
    super::lease_context::admit(item, &registered)?;
    super::lease_workflow_context::admit(item, &registered)?;
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
    if !item.exec.overlays.is_empty() {
        return Err(
            "lease-supplied overlays are not supported; configure reviewed overlays on the worker"
                .into(),
        );
    }
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
    let command = match item.kind {
        WorkKind::Prompt => {
            if item.exec.dynamic || item.exec.workflow_toml.is_some() || item.exec.leader.is_some()
            {
                return Err("prompt leases cannot select a workflow or leader".into());
            }
            // The body is carried by Session::inline_work_item(), never by
            // persisted command arguments or a host process command line.
            vec!["exec".into(), "prompt".into()]
        }
        WorkKind::Workflow => {
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
                validate_workflow(
                    &Workflow::parse(toml, WorkflowFormat::Toml).map_err(|e| e.to_string())?,
                )?;
                // The runner replaces this with a private file, never a clone file.
                args.push("LEASE_WORKFLOW_FILE".into());
            } else {
                if !item.exec.dynamic {
                    return Err("workflow lease requires dynamic or workflow_toml".into());
                }
                args.extend(["--dynamic".into(), "--work-item".into(), "1".into()]);
                if let Some(leader) = &item.exec.leader {
                    args.extend(["--leader".into(), leader.clone()]);
                }
            }
            vec!["exec".into(), "workflow".into()]
        }
    };
    let refs: Vec<_> = command.iter().map(String::as_str).collect();
    crate::command::dispatch::catalogue::CommandCatalogue::get()
        .parse_raw_args(&refs, &args)
        .map_err(|e| e.to_string())?;
    Ok(LeasePlan {
        interaction,
        command,
        args,
    })
}
