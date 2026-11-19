//! Owner-local, image-independent profile management.
use super::AgentsCommandFrontend;
use crate::command::commands::{launch_policy::settings_profile as policy, Command};
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::data::fs::{agent_profiles::*, DataPaths};
use crate::data::session::{AgentName, Session};
use crate::data::settings_profile::{ProfileState, SettingsProfileName};
use async_trait::async_trait;
use serde::Serialize;
use std::time::Duration;
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ProfileOutcome {
    List {
        profiles: Vec<ProfileListing>,
    },
    Delete {
        agent: String,
        name: String,
        deleted: bool,
    },
    Close {
        agent: String,
        name: String,
        state: ProfileState,
        snapshot: String,
        cleanup_reason: Option<CleanupReason>,
        failure: Option<ProfileCloseFailure>,
    },
}
#[derive(Debug, Clone, Serialize)]
pub struct ProfileCloseFailure {
    pub code: String,
    pub message: String,
}
pub struct ProfileCommand {
    action: String,
    agent: Option<AgentName>,
    name: Option<SettingsProfileName>,
    session: Session,
    engines: Engines,
}
impl ProfileCommand {
    pub fn from_input(ctx: &BuildContext<'_>) -> Result<Self, CommandError> {
        policy::require_frontend(ctx.caller.frontend())?;
        if !ctx.caller.profile_owner() {
            return Err(CommandError::ProfileOwnerRequired);
        }
        if ctx.caller.leaf() == "delete" && !ctx.flags.bool("yes") {
            return Err(CommandError::MissingRequiredFlag {
                command: ctx.path().iter().map(|s| s.to_string()).collect(),
                flag: "yes".into(),
            });
        }
        let name = if ctx.caller.leaf() == "list" {
            None
        } else {
            ctx.args.get("name")
        }
        .map(SettingsProfileName::parse)
        .transpose()
        .map_err(|e| CommandError::InvalidArgumentValue {
            command: vec![],
            argument: "name".into(),
            reason: e.to_string(),
        })?;
        Ok(Self {
            action: ctx.caller.leaf().into(),
            agent: ctx.flags.str("agent").map(AgentName::new).transpose()?,
            name,
            session: ctx.session.clone(),
            engines: ctx.engines.clone(),
        })
    }
}
#[async_trait]
impl Command for ProfileCommand {
    type Frontend = Box<dyn AgentsCommandFrontend>;
    type Outcome = ProfileOutcome;
    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let repo = ProfileRepoBinding::from_session(&self.session)?;
        let paths = DataPaths::from_env(self.session.env())?;
        if self.action == "list" {
            let out = ProfileOutcome::List {
                profiles: list_profiles(&paths, &repo, self.agent.as_ref())?,
            };
            frontend.show_profile_result(&out);
            return Ok(out);
        }
        let agent = self
            .agent
            .ok_or_else(|| CommandError::Other("--agent is required".into()))?;
        let name = self
            .name
            .ok_or_else(|| CommandError::Other("profile name is required".into()))?;
        let layout = ProfileLayout::new(&paths, &repo, &agent, &name);
        if !layout.exists() {
            return Err(not_found());
        }
        // Zero wait avoids blocking the async worker on a live command. Its
        // owner must cancel the command; this command cannot close a live lease.
        let lease = layout.try_lock(Duration::ZERO)?.ok_or_else(|| {
            CommandError::SettingsProfileBusy("still running; cancel or close its command".into())
        })?;
        let m = lease.read_manifest()?.ok_or_else(not_found)?;
        let out = if self.action == "delete" {
            if !m.state.admits_writer() {
                return Err(CommandError::SettingsProfileBusy(
                    "profile is active or closing".into(),
                ));
            }
            lease.remove_profile()?;
            ProfileOutcome::Delete {
                agent: agent.to_string(),
                name: name.to_string(),
                deleted: true,
            }
        } else {
            let already_inactive = m.state.admits_writer();
            let m = if already_inactive {
                m
            } else {
                let runtime =
                    policy::recorded_runtime(&self.session, &m, self.engines.runtime.clone());
                match self
                    .engines
                    .agent_engine
                    .resolve_definition(&self.session, &agent)
                    .ok()
                    .and_then(|def| def.profile_state.clone())
                {
                    Some(spec) => {
                        policy::reconcile(
                            &lease,
                            m,
                            policy::ReconcileMode::StopOwned,
                            runtime,
                            &spec,
                            TurnStateAtExit::Unknown,
                        )
                        .await?
                    }
                    None => {
                        policy::reconcile_retaining_previous(
                            &lease,
                            m,
                            policy::ReconcileMode::StopOwned,
                            runtime,
                        )
                        .await?
                    }
                }
            };
            let snapshot = if already_inactive {
                "unchanged"
            } else {
                match m.last_publication.as_ref().map(|p| &p.status) {
                    Some(PublicationStatus::RetainedPrevious) => "retained_previous",
                    Some(PublicationStatus::Published) if !m.state.admits_writer() => "unchanged",
                    Some(PublicationStatus::Published) => "published",
                    None => "unchanged",
                }
            };
            let failure = (!m.state.admits_writer()).then(|| {
                let reason = m.cleanup.as_ref().map(|c| c.reason);
                let code = match reason {
                    Some(CleanupReason::RuntimeUnavailable) => "runtime_unavailable",
                    Some(CleanupReason::LivenessUnknown) => "liveness_unknown",
                    Some(CleanupReason::SnapshotIo) => "snapshot_io",
                    Some(CleanupReason::SnapshotRejected) => "snapshot_rejected",
                    Some(CleanupReason::CleanupTimeout) => "cleanup_timeout",
                    _ => "cleanup_unconfirmed",
                };
                ProfileCloseFailure {
                    code: code.into(),
                    message: format!(
                        "profile remains closing{}",
                        reason.map(|r| format!(": {r:?}")).unwrap_or_default()
                    ),
                }
            });
            ProfileOutcome::Close {
                agent: agent.to_string(),
                name: name.to_string(),
                state: m.state,
                snapshot: snapshot.into(),
                cleanup_reason: m.cleanup.map(|c| c.reason),
                failure,
            }
        };
        frontend.show_profile_result(&out);
        Ok(out)
    }
}
fn not_found() -> CommandError {
    CommandError::InvalidArgumentValue {
        command: vec![],
        argument: "name".into(),
        reason: "profile not found".into(),
    }
}
