//! Bounded, non-interactive workflow prerequisite preflight.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::data::config::workflows::{
    AcquireRefresh, CredentialUse, PolicySource, PreflightMode, PrerequisitePolicy,
};
use crate::data::fs::phase_tools_cache::PhaseTools;
use crate::data::session::SessionType;
use crate::data::workflow_prerequisites::*;
use crate::engine::agent_runtime::AgentRuntimeEngine;
use crate::engine::auth::redaction::RedactionSet;
use crate::engine::container::ContainerRuntime;
use crate::engine::workflow::prerequisite_grants::{PrerequisiteGrants, TokenGrantSource};

mod probes;
pub mod sources;
pub use sources::PreflightSources;

#[cfg(test)]
mod tests;

pub const PROBE_BUDGET: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RememberedGrants {
    pub ssh_key: Option<PathBuf>,
    pub ssh_dir: bool,
    pub token_env: Option<String>,
    pub identity: Option<(String, String)>,
}

#[derive(Clone)]
pub struct PrerequisitePreflightOptions {
    pub prerequisites: WorkflowPrerequisites,
    pub phase: PrerequisitePhase,
    pub git_root: PathBuf,
    pub worktree: Option<PathBuf>,
    pub worktree_planned: bool,
    pub phase_image: Option<String>,
    pub runtime: Arc<dyn AgentRuntimeEngine>,
    pub container_runtime: Option<Arc<ContainerRuntime>>,
    /// Remote API sessions use this URL even before the clone exists.
    pub session_type: Option<SessionType>,
    pub policy: PrerequisitePolicy,
    pub remembered: RememberedGrants,
    pub repo_acquire_approved: bool,
    pub can_ask: bool,
    pub existing_grants: Option<PrerequisiteGrants>,
    pub notes: Vec<String>,
    pub probe_budget: Duration,
}

impl PrerequisitePreflightOptions {
    /// Populate session context so remote API sessions never probe an unrelated
    /// host checkout's origin. The caller supplies the already-resolved policy.
    pub fn for_session(
        prerequisites: WorkflowPrerequisites,
        session: &crate::data::session::Session,
        runtime: Arc<dyn AgentRuntimeEngine>,
        container_runtime: Option<Arc<ContainerRuntime>>,
        policy: PrerequisitePolicy,
    ) -> Self {
        let mut options = Self::new(
            prerequisites,
            session.git_root().to_path_buf(),
            runtime,
            policy,
        );
        options.session_type = Some(session.session_type().clone());
        options.container_runtime = container_runtime;
        options
    }

    pub fn new(
        prerequisites: WorkflowPrerequisites,
        git_root: PathBuf,
        runtime: Arc<dyn AgentRuntimeEngine>,
        policy: PrerequisitePolicy,
    ) -> Self {
        Self {
            prerequisites,
            git_root,
            runtime,
            policy,
            phase: PrerequisitePhase::Start,
            worktree: None,
            worktree_planned: false,
            phase_image: None,
            container_runtime: None,
            session_type: None,
            remembered: RememberedGrants::default(),
            repo_acquire_approved: false,
            can_ask: false,
            existing_grants: None,
            notes: Vec::new(),
            probe_budget: PROBE_BUDGET,
        }
    }
}

#[derive(Clone)]
pub struct PrerequisitePreflight {
    options: PrerequisitePreflightOptions,
    sources: PreflightSources,
}

impl PrerequisitePreflight {
    pub fn new(mut options: PrerequisitePreflightOptions, sources: PreflightSources) -> Self {
        // Derivation already strips URL credentials; defend direct engine
        // callers too, including the remote field retained after classification.
        for need in &mut options.prerequisites.needs {
            match &mut need.prerequisite {
                WorkflowPrerequisite::GitRemote { remote }
                | WorkflowPrerequisite::PushAuth { remote }
                | WorkflowPrerequisite::PushAuthSsh { remote, .. }
                | WorkflowPrerequisite::PushAuthHttps { remote, .. } => {
                    *remote = GitRemoteUrl::parse(remote).redacted;
                }
                _ => {}
            }
        }
        // Policy resolution normally does this; also protect direct engine callers.
        if options
            .session_type
            .as_ref()
            .is_some_and(SessionType::is_remote)
            && options.policy.ssh.source == PolicySource::Default
        {
            options.policy.ssh.use_ = CredentialUse::Deny;
        }
        Self { options, sources }
    }

    fn repo(&self) -> &Path {
        self.options
            .worktree
            .as_deref()
            .unwrap_or(&self.options.git_root)
    }

    /// Blocking, but bounded by the longest probe budget, not their sum.
    /// Owned worker threads are deliberately detached: joining scoped threads
    /// would wait for a hung injected source after its timeout had elapsed.
    /// Host-secret and container sources also enforce process-level timeouts.
    pub fn run(&self) -> WorkflowPrerequisiteReport {
        let mut report = WorkflowPrerequisiteReport {
            phase: self.options.phase,
            origin: self.options.prerequisites.origin,
            checks: vec![],
            advisories: self.options.prerequisites.advisories.clone(),
            notes: self.options.notes.clone(),
        };
        let redaction = RedactionSet::new();
        if let Some(grants) = &self.options.existing_grants {
            for secret in grants.redaction_secrets() {
                redaction.add(secret);
            }
        }
        if self.options.policy.preflight == PreflightMode::Off {
            report.checks = self
                .options
                .prerequisites
                .needs
                .iter()
                .map(|need| {
                    Self::check(
                        need,
                        PrerequisiteStatus::Unknown,
                        "preflight disabled",
                        vec![],
                    )
                })
                .collect();
            return Self::redact_report(report, &redaction);
        }
        let mut pending = Vec::new();
        for need in &self.options.prerequisites.needs {
            let worker = self.clone();
            let need = need.clone();
            let fallback = need.clone();
            let secrets = redaction.clone();
            let (tx, rx) = mpsc::sync_channel(1);
            let started = Instant::now();
            let spawn = std::thread::Builder::new()
                .name("prerequisite-probe".into())
                .spawn(move || {
                    let checks = worker.probe(&need, &secrets);
                    let _ = tx.send((Instant::now(), checks));
                });
            pending.push((fallback, started, spawn.is_ok(), rx));
        }
        let budget = self.options.probe_budget.min(PROBE_BUDGET);
        for (need, started, spawned, rx) in pending {
            let remaining = budget.saturating_sub(started.elapsed());
            match rx.recv_timeout(remaining) {
                Ok((finished, checks)) if finished.duration_since(started) <= budget => report.checks.extend(checks),
                _ => report.checks.push(Self::check(
                    &need, PrerequisiteStatus::Unknown,
                    if spawned { "probe timed out or unavailable; retry --check-prerequisites before running the workflow" }
                    else { "probe could not start; retry --check-prerequisites" },
                    vec![],
                )),
            }
        }
        // Several SSH destinations can add the same tool check. Keep one row
        // and retain all affected steps for the consent layer.
        let mut checks: Vec<PrerequisiteCheck> = Vec::new();
        for check in report.checks {
            if let Some(existing) = checks
                .iter_mut()
                .find(|c| c.prerequisite == check.prerequisite)
            {
                for step in check.needed_by {
                    if !existing.needed_by.contains(&step) {
                        existing.needed_by.push(step);
                    }
                }
                let severity = |status| match status {
                    PrerequisiteStatus::Satisfied => 0,
                    PrerequisiteStatus::Unknown => 1,
                    PrerequisiteStatus::NeedsConsent => 2,
                    PrerequisiteStatus::Missing => 3,
                };
                if severity(check.status) > severity(existing.status) {
                    existing.status = check.status;
                    existing.explanation = check.explanation;
                    existing.remediations = check.remediations;
                }
            } else {
                checks.push(check);
            }
        }
        report.checks = checks;
        Self::redact_report(report, &redaction)
    }

    /// Run once before teardown and return one aggregate report for the caller's
    /// warning event. No questions, no login, and no unapproved acquisition.
    /// Only an already-granted command with refresh=beforeTeardown is run again.
    pub fn recheck_before_teardown(
        &self,
        grants: &mut PrerequisiteGrants,
        redaction: &RedactionSet,
        refused_binaries: &[String],
    ) -> WorkflowPrerequisiteReport {
        let mut worker = self.clone();
        worker.options.phase = PrerequisitePhase::BeforeTeardown;
        worker.options.can_ask = false;
        worker.options.prerequisites.needs.retain_mut(|need| {
            if need.needed_by.is_empty() {
                return true;
            }
            need.needed_by
                .retain(|step| step.phase == crate::data::workflow_state::PhaseKind::Teardown);
            !need.needed_by.is_empty()
        });
        worker
            .options
            .prerequisites
            .advisories
            .retain(|a| a.step.phase == crate::data::workflow_state::PhaseKind::Teardown);
        let mut refresh_failed = false;
        let mut extra_lines = false;
        if worker.options.policy.preflight != PreflightMode::Off
            && worker.options.policy.github_token.use_ != CredentialUse::Deny
            && worker.options.prerequisites.needs.iter().any(|need| {
                matches!(
                    need.prerequisite,
                    WorkflowPrerequisite::GitHubToken { .. }
                        | WorkflowPrerequisite::PushAuth { .. }
                        | WorkflowPrerequisite::PushAuthHttps { .. }
                )
            })
        {
            if let Some(token) = &mut grants.token {
                redaction.add(token.secret.clone());
                if let Some(acquire) = token
                    .acquire
                    .as_ref()
                    .filter(|a| a.refresh == AcquireRefresh::BeforeTeardown)
                {
                    let approved = acquire.scope == AcquireScope::Global
                        || worker.options.repo_acquire_approved;
                    // Never use an unrelated policy command to refresh an env/pasted token.
                    if approved && matches!(token.source, TokenGrantSource::HostCommand(_)) {
                        match worker
                            .sources
                            .tokens
                            .acquire_token(acquire, refused_binaries)
                        {
                            Ok(output) if output.secret.is_some() => {
                                extra_lines = output.extra_lines > 0;
                                let secret = output.secret.expect("checked token output");
                                redaction.add(secret.clone());
                                token.secret = secret;
                            }
                            _ => {
                                refresh_failed = true;
                            }
                        }
                    } else if matches!(token.source, TokenGrantSource::HostCommand(_)) {
                        refresh_failed = true;
                    }
                }
                if let TokenGrantSource::Env(var) = &token.source {
                    // Update the value as well as checking presence, so a rotated
                    // env token reaches both execution and verification.
                    if let Some(secret) = worker.sources.tokens.env_token(var) {
                        redaction.add(secret.clone());
                        token.secret = secret;
                    }
                }
            }
        }
        if grants.token.as_ref().is_some_and(|t| match &t.source {
            TokenGrantSource::Env(var) => worker
                .sources
                .tokens
                .env_token_name(std::slice::from_ref(var))
                .is_none(),
            _ => false,
        }) {
            grants.token = None;
            grants.token_destinations.clear();
        }
        worker.options.existing_grants = Some(grants.clone());
        let mut report = worker.run();
        if refresh_failed {
            for check in &mut report.checks {
                if matches!(
                    check.prerequisite,
                    WorkflowPrerequisite::GitHubToken { .. }
                        | WorkflowPrerequisite::PushAuthHttps { .. }
                ) {
                    check.status = PrerequisiteStatus::Unknown;
                    check.explanation =
                        "token refresh failed; re-run the prerequisite check before publishing"
                            .into();
                }
            }
        }
        if extra_lines {
            report.notes.push(
                "token command returned extra lines; only its first non-empty line was used".into(),
            );
        }
        Self::redact_report(report, redaction)
    }

    pub fn resolved_prerequisites(
        &self,
        report: &WorkflowPrerequisiteReport,
    ) -> WorkflowPrerequisites {
        let mut resolved = WorkflowPrerequisites {
            origin: report.origin,
            needs: vec![],
            advisories: report.advisories.clone(),
        };
        for check in &report.checks {
            resolved.merge_needs(PrerequisiteNeed {
                prerequisite: check.prerequisite.clone(),
                needed_by: check.needed_by.clone(),
            });
        }
        resolved
    }

    pub fn key_readable_in_image(tools: &PhaseTools) -> Option<bool> {
        #[cfg(unix)]
        {
            tools
                .uid
                .map(|uid| uid == 0 || uid == nix::unistd::geteuid().as_raw())
        }
        #[cfg(not(unix))]
        {
            tools.uid.map(|uid| uid == 0)
        }
    }

    fn check(
        need: &PrerequisiteNeed,
        status: PrerequisiteStatus,
        explanation: impl Into<String>,
        remediations: Vec<Remediation>,
    ) -> PrerequisiteCheck {
        PrerequisiteCheck {
            prerequisite: need.prerequisite.clone(),
            needed_by: need.needed_by.clone(),
            status,
            explanation: explanation.into(),
            remediations,
        }
    }

    pub fn redact_report(
        report: WorkflowPrerequisiteReport,
        redaction: &RedactionSet,
    ) -> WorkflowPrerequisiteReport {
        // Apply to all string fields, including notes, identities and argv; never
        // interpolate source errors, which may contain unknown credential values.
        fn redact(value: &mut serde_json::Value, secrets: &RedactionSet) {
            match value {
                serde_json::Value::String(s) => *s = secrets.redact(s),
                serde_json::Value::Array(items) => {
                    items.iter_mut().for_each(|v| redact(v, secrets))
                }
                serde_json::Value::Object(items) => {
                    items.values_mut().for_each(|v| redact(v, secrets))
                }
                _ => {}
            }
        }
        if redaction.is_empty() {
            return report;
        }
        let mut value = serde_json::to_value(&report).expect("report is serializable");
        redact(&mut value, redaction);
        serde_json::from_value(value).unwrap_or_else(|_| WorkflowPrerequisiteReport {
            phase: report.phase,
            origin: report.origin,
            checks: report
                .checks
                .iter()
                .map(|_need| {
                    // Do not retain potentially sensitive labels if a secret happened
                    // to equal an enum discriminant in the serialized report.
                    PrerequisiteCheck {
                        prerequisite: WorkflowPrerequisite::NotDetachedHead,
                        status: PrerequisiteStatus::Unknown,
                        explanation: "prerequisite details redacted".into(),
                        needed_by: vec![],
                        remediations: vec![],
                    }
                })
                .collect(),
            advisories: vec![],
            notes: vec![],
        })
    }
}
