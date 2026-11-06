//! Typed squad resource operations. Resource IDs are resolved here, never as task names.
use super::DaemonApi;
use crate::command::commands::squad::gateway::{
    CreateTask, EnvCoverage, EnvPush, EnvPushResponse, TaskGateway, UpdateTask,
};
use crate::command::error::CommandError;
use crate::data::fs::task_store::{Run, Task, TaskStatus};
use crate::data::ids::{RunId, TaskId};
use crate::data::workflow_state::WorkflowState;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum SquadError {
    #[error("task not found")]
    TaskNotFound,
    #[error("run not found")]
    RunNotFound,
    #[error("task already exists")]
    TaskExists,
    #[error("task is paused; resume it before starting a run")]
    TaskPaused,
    #[error("no workflow for this task")]
    NoWorkflow,
    #[error(transparent)]
    Command(#[from] CommandError),
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
    #[serde(flatten)]
    pub update: UpdateTask,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCreated {
    pub id: RunId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCanceled {
    pub id: RunId,
    pub status: String,
}

fn gateway(api: &DaemonApi) -> &dyn TaskGateway {
    api.squad.as_deref().expect("feature gate checked squad")
}
pub async fn tasks_list(api: &DaemonApi, name: Option<&str>) -> Result<Vec<Task>, SquadError> {
    Ok(gateway(api)
        .list()
        .await?
        .into_iter()
        .filter(|t| name.is_none_or(|n| n == t.name))
        .collect())
}
pub async fn tasks_get(api: &DaemonApi, id: &TaskId) -> Result<Task, SquadError> {
    gateway(api)
        .list()
        .await?
        .into_iter()
        .find(|t| &t.id == id)
        .ok_or(SquadError::TaskNotFound)
}
pub async fn tasks_create(api: &DaemonApi, mut request: CreateTask) -> Result<Task, SquadError> {
    if let Some(policy) = &mut request.prerequisites {
        admit_policy(api, policy)?;
    }
    if gateway(api)
        .list()
        .await?
        .iter()
        .any(|t| t.name == request.name)
    {
        return Err(SquadError::TaskExists);
    }
    Ok(gateway(api).create(request).await?)
}
pub async fn tasks_update(
    api: &DaemonApi,
    id: &TaskId,
    mut request: TaskPatch,
) -> Result<Task, SquadError> {
    if let Some(policy) = &mut request.update.prerequisites {
        admit_policy(api, policy)?;
    }
    let task = tasks_get(api, id).await?;
    if !request.update.is_empty() {
        gateway(api).update(&task.name, request.update).await?;
    }
    if let Some(paused) = request.paused {
        gateway(api)
            .set_status(
                &task.name,
                if paused {
                    TaskStatus::Paused
                } else {
                    TaskStatus::Active
                },
            )
            .await?;
    }
    Ok(gateway(api).get(&task.name).await?)
}
pub async fn tasks_delete(api: &DaemonApi, id: &TaskId) -> Result<(), SquadError> {
    let task = tasks_get(api, id).await?;
    Ok(gateway(api).delete(&task.name).await?)
}
pub async fn tasks_workflow(api: &DaemonApi, id: &TaskId) -> Result<WorkflowState, SquadError> {
    let task = tasks_get(api, id).await?;
    gateway(api)
        .workflow_state(&task.name)
        .await?
        .ok_or(SquadError::NoWorkflow)
}
pub async fn runs_list(
    api: &DaemonApi,
    id: &TaskId,
    status: Option<&str>,
) -> Result<Vec<Run>, SquadError> {
    let task = tasks_get(api, id).await?;
    Ok(gateway(api)
        .runs(&task.name, usize::MAX)
        .await?
        .into_iter()
        .filter(|r| {
            status.is_none_or(|s| {
                serde_json::to_value(r.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .as_deref()
                    == Some(s)
            })
        })
        .collect())
}
pub async fn runs_create(api: &DaemonApi, id: &TaskId) -> Result<RunCreated, SquadError> {
    let task = tasks_get(api, id).await?;
    if task.status == TaskStatus::Paused {
        return Err(SquadError::TaskPaused);
    }
    Ok(RunCreated {
        id: gateway(api).create_run(&task.name).await?,
    })
}
pub async fn runs_cancel(api: &DaemonApi, id: &RunId) -> Result<RunCanceled, SquadError> {
    let mut found = None;
    for task in gateway(api).list().await? {
        if let Some(run) = gateway(api)
            .runs(&task.name, usize::MAX)
            .await?
            .into_iter()
            .find(|r| &r.id == id)
        {
            found = Some(run);
            break;
        }
    }
    let run = found.ok_or(SquadError::RunNotFound)?;
    let status = if run.status == crate::data::fs::task_store::RunStatus::Running {
        gateway(api).cancel_run_id(id).await?;
        "canceling".into()
    } else {
        serde_json::to_value(run.status)
            .expect("run status")
            .as_str()
            .expect("status string")
            .into()
    };
    Ok(RunCanceled {
        id: id.clone(),
        status,
    })
}
pub async fn env_get(api: &DaemonApi) -> Result<EnvCoverage, CommandError> {
    gateway(api).env_coverage().await
}
pub async fn env_put(api: &DaemonApi, request: EnvPush) -> Result<EnvPushResponse, CommandError> {
    gateway(api).push_env(request).await
}
pub async fn env_clear(api: &DaemonApi) -> Result<bool, CommandError> {
    gateway(api).clear_env_store().await
}

/// Wire projection while the scheduler retains seconds internally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResponse(pub Task);
impl Serialize for TaskResponse {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut value = serde_json::to_value(&self.0).map_err(serde::ser::Error::custom)?;
        let o = value.as_object_mut().expect("task object");
        let secs = o.remove("interval_secs").expect("interval");
        o.insert(
            "interval_ms".into(),
            serde_json::json!(secs.as_u64().unwrap() * 1000),
        );
        for key in [
            "created_at",
            "updated_at",
            "backoff_until",
            "last_run_at",
            "trigger_requested_at",
        ] {
            if let Some(raw) = o.get(key).and_then(|v| v.as_str()) {
                let at = chrono::DateTime::parse_from_rfc3339(raw)
                    .map_err(serde::ser::Error::custom)?
                    .with_timezone(&chrono::Utc);
                o.insert(
                    key.into(),
                    serde_json::json!(crate::data::wire::format_timestamp(at)),
                );
            }
        }
        value.serialize(s)
    }
}
impl<'de> Deserialize<'de> for TaskResponse {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(d)?;
        let ms = value
            .as_object_mut()
            .and_then(|o| o.remove("interval_ms"))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| serde::de::Error::custom("missing interval_ms"))?;
        if ms % 1000 != 0 {
            return Err(serde::de::Error::custom(
                "interval_ms must be a multiple of 1000 milliseconds (the scheduler interval has one-second granularity)",
            ));
        }
        value["interval_secs"] = serde_json::json!(ms / 1000);
        serde_json::from_value(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// Treat every HTTP policy as remote input regardless of the source tags it carries.
/// Credential commands are operator config; clients select allowed sources, never install argv.
fn admit_policy(
    api: &DaemonApi,
    policy: &mut crate::data::config::workflows::PrerequisitePolicy,
) -> Result<(), CommandError> {
    use crate::command::commands::workflow_prerequisites::check_flag_widening;
    use crate::command::dispatch::catalogue::FrontendKind;
    use crate::data::config::flags::{IdentityFlag, PrerequisiteFlags, SshFlag, TokenFlag};
    use crate::data::config::workflows::{CredentialUse, PolicySource, PrerequisiteResolveContext};
    let global = api
        .config
        .lock()
        .expect("config mutex")
        .prerequisite_policy(PrerequisiteResolveContext {
            remote_session: false,
        });
    let refuse = || CommandError::PrerequisiteWideningRefused {
        flag: "remote task prerequisite policy".into(),
    };
    let mut flags = PrerequisiteFlags {
        unresolved: policy.unresolved,
        ..Default::default()
    };
    if policy.git_identity.use_ == CredentialUse::Allow {
        flags.identity = Some(match &policy.git_identity.fixed {
            Some(id) => IdentityFlag::Fixed {
                name: id.name.clone(),
                email: id.email.clone(),
            },
            None => IdentityFlag::Host,
        });
    }
    if policy.ssh.use_ == CredentialUse::Allow {
        flags.ssh = Some(if policy.ssh.whole_dir {
            SshFlag::All
        } else if let Some(path) = &policy.ssh.key {
            SshFlag::Key(path.clone())
        } else {
            return Err(refuse());
        });
    }
    check_flag_widening(FrontendKind::Api, &flags, &global)?;
    if let Some(acquire) = &policy.github_token.acquire {
        if global.github_token.acquire.as_ref() != Some(acquire) {
            return Err(refuse());
        }
    }
    if policy.github_token.use_ == CredentialUse::Allow {
        if policy.github_token.env_vars.is_empty() && policy.github_token.acquire.is_none() {
            return Err(refuse());
        }
        for name in &policy.github_token.env_vars {
            let flags = PrerequisiteFlags {
                token: Some(TokenFlag::Env(name.clone())),
                ..Default::default()
            };
            check_flag_widening(FrontendKind::Api, &flags, &global)?;
        }
        if policy.github_token.acquire.is_some() {
            check_flag_widening(
                FrontendKind::Api,
                &PrerequisiteFlags {
                    token: Some(TokenFlag::Acquire),
                    ..Default::default()
                },
                &global,
            )?;
        }
    }
    policy.git_identity.source = PolicySource::RemoteTask;
    policy.ssh.source = PolicySource::RemoteTask;
    policy.github_token.source = PolicySource::RemoteTask;
    policy.allow_api_client_grants = false;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResponse(pub Run);
impl Serialize for RunResponse {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut value = serde_json::to_value(&self.0).map_err(serde::ser::Error::custom)?;
        if let Some(fields) = value.as_object_mut() {
            for key in ["started_at", "finished_at"] {
                if let Some(raw) = fields.get(key).and_then(|v| v.as_str()) {
                    let at = chrono::DateTime::parse_from_rfc3339(raw)
                        .map_err(serde::ser::Error::custom)?
                        .with_timezone(&chrono::Utc);
                    fields.insert(
                        key.into(),
                        serde_json::json!(crate::data::wire::format_timestamp(at)),
                    );
                }
            }
        }
        value.serialize(s)
    }
}
impl<'de> Deserialize<'de> for RunResponse {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Run::deserialize(d).map(Self)
    }
}

/// CLI outcomes and HTTP resources share the same wire projection.
pub mod task_wire {
    use super::*;
    pub fn serialize<S: serde::Serializer>(task: &Task, s: S) -> Result<S::Ok, S::Error> {
        TaskResponse(task.clone()).serialize(s)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Task, D::Error> {
        TaskResponse::deserialize(d).map(|v| v.0)
    }
}
pub fn task_list_wire<S: serde::Serializer>(tasks: &[Task], s: S) -> Result<S::Ok, S::Error> {
    crate::data::wire::List {
        items: tasks.iter().cloned().map(TaskResponse).collect::<Vec<_>>(),
        next_cursor: None,
    }
    .serialize(s)
}
pub mod run_list_wire {
    use super::*;
    pub fn serialize<S: serde::Serializer>(runs: &[Run], s: S) -> Result<S::Ok, S::Error> {
        crate::data::wire::List {
            items: runs.iter().cloned().map(RunResponse).collect::<Vec<_>>(),
            next_cursor: None,
        }
        .serialize(s)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Run>, D::Error> {
        crate::data::wire::List::<RunResponse>::deserialize(d)
            .map(|v| v.items.into_iter().map(|v| v.0).collect())
    }
}
