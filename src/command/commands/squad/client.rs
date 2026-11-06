//! `DaemonSquadClient`: squad task commands as `/v1/squad/*` resource requests
//! over the daemon's owner-only local listener. Task names are resolved
//! through the task list's `name` filter.
use super::gateway::{
    CreateTask, DaemonStatus, EnvCoverage, EnvPush, EnvPushResponse, TaskGateway, UpdateTask,
};
use crate::command::commands::daemon::actions::Request;
use crate::command::commands::daemon::client::DaemonClient;
use crate::command::daemon_api::squad::TaskResponse;
use crate::command::error::CommandError;
use crate::data::fs::task_store::{Run, Task, TaskStatus};
use crate::data::ids::RunId;
use crate::data::wire::List;
use crate::data::workflow_state::WorkflowState;
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

pub struct DaemonSquadClient {
    daemon: DaemonClient,
}

impl DaemonSquadClient {
    pub fn new(daemon: DaemonClient) -> Self {
        Self { daemon }
    }
    pub fn daemon(&self) -> &DaemonClient {
        &self.daemon
    }
    async fn request<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &[&str],
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<T, CommandError> {
        let mut request = Request::new(method.as_str(), format!("/v1/{}", path.join("/")), body);
        for (key, value) in query {
            request = request.with_query(key, value.clone());
        }
        let value = self.daemon.call(&request).await?;
        serde_json::from_value(value)
            .map_err(|e| CommandError::Other(format!("invalid squad response: {e}")))
    }
    async fn all<T: DeserializeOwned>(
        &self,
        path: &[&str],
        mut query: Vec<(&str, String)>,
    ) -> Result<Vec<T>, CommandError> {
        let mut items = Vec::new();
        loop {
            let page: List<T> = self
                .request(reqwest::Method::GET, path, &query, None)
                .await?;
            items.extend(page.items);
            match page.next_cursor {
                Some(c) => {
                    query.retain(|(k, _)| *k != "cursor");
                    query.push(("cursor", c));
                }
                None => return Ok(items),
            }
        }
    }
    async fn resolve(&self, name: &str) -> Result<Task, CommandError> {
        let mut tasks: Vec<TaskResponse> = self
            .all(&["squad", "tasks"], vec![("name", name.into())])
            .await?;
        tasks
            .pop()
            .map(|v| v.0)
            .ok_or_else(|| CommandError::Other(format!("task {name:?} was not found")))
    }
}
#[async_trait]
impl TaskGateway for DaemonSquadClient {
    async fn create(&self, req: CreateTask) -> Result<Task, CommandError> {
        let task: TaskResponse = self
            .request(
                reqwest::Method::POST,
                &["squad", "tasks"],
                &[],
                Some(serde_json::to_value(req).expect("task request")),
            )
            .await?;
        Ok(task.0)
    }
    async fn update(&self, name: &str, req: UpdateTask) -> Result<Task, CommandError> {
        let task = self.resolve(name).await?;
        let task: TaskResponse = self
            .request(
                reqwest::Method::PATCH,
                &["squad", "tasks", task.id.as_str()],
                &[],
                Some(serde_json::to_value(req).expect("task patch")),
            )
            .await?;
        Ok(task.0)
    }
    async fn list(&self) -> Result<Vec<Task>, CommandError> {
        let tasks: Vec<TaskResponse> = self.all(&["squad", "tasks"], vec![]).await?;
        Ok(tasks.into_iter().map(|v| v.0).collect())
    }
    async fn get(&self, name: &str) -> Result<Task, CommandError> {
        let task = self.resolve(name).await?;
        let task: TaskResponse = self
            .request(
                reqwest::Method::GET,
                &["squad", "tasks", task.id.as_str()],
                &[],
                None,
            )
            .await?;
        Ok(task.0)
    }
    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError> {
        let task = self.resolve(name).await?;
        let mut runs = self
            .all(&["squad", "tasks", task.id.as_str(), "runs"], vec![])
            .await?;
        runs.truncate(limit);
        Ok(runs)
    }
    async fn set_status(&self, name: &str, status: TaskStatus) -> Result<(), CommandError> {
        let task = self.resolve(name).await?;
        let _: TaskResponse = self
            .request(
                reqwest::Method::PATCH,
                &["squad", "tasks", task.id.as_str()],
                &[],
                Some(json!({"paused":status==TaskStatus::Paused})),
            )
            .await?;
        Ok(())
    }
    async fn trigger(&self, name: &str) -> Result<(), CommandError> {
        self.create_run(name).await.map(|_| ())
    }
    async fn create_run(&self, name: &str) -> Result<RunId, CommandError> {
        let task = self.resolve(name).await?;
        let response: crate::command::daemon_api::squad::RunCreated = self
            .request(
                reqwest::Method::POST,
                &["squad", "tasks", task.id.as_str(), "runs"],
                &[],
                Some(json!({})),
            )
            .await?;
        Ok(response.id)
    }
    async fn cancel(&self, name: &str) -> Result<(), CommandError> {
        let task = self.resolve(name).await?;
        let runs: Vec<Run> = self
            .all(
                &["squad", "tasks", task.id.as_str(), "runs"],
                vec![("status", "running".into())],
            )
            .await?;
        let run = runs
            .first()
            .ok_or_else(|| CommandError::Other(format!("task {name:?} has no run in progress")))?;
        self.cancel_run_id(&run.id).await
    }
    async fn cancel_run_id(&self, id: &RunId) -> Result<(), CommandError> {
        let _: Value = self
            .request(
                reqwest::Method::POST,
                &["squad", "runs", id.as_str(), "cancel"],
                &[],
                Some(json!({})),
            )
            .await?;
        Ok(())
    }
    async fn delete(&self, name: &str) -> Result<(), CommandError> {
        let task = self.resolve(name).await?;
        let _: Value = self
            .request(
                reqwest::Method::DELETE,
                &["squad", "tasks", task.id.as_str()],
                &[],
                None,
            )
            .await?;
        Ok(())
    }
    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        let status: crate::command::daemon_api::core::StatusResponse = self
            .request(reqwest::Method::GET, &["status"], &[], None)
            .await?;
        let record = status
            .features
            .get("squad")
            .ok_or_else(|| CommandError::Other("squad detail missing in daemon status".into()))?;
        let detail = record.detail.clone().unwrap_or_default();
        Ok(DaemonStatus {
            running: record.state != crate::data::daemon_state::FeatureState::Off,
            pid: Some(status.pid),
            bound_addr: None,
            task_count: detail["task_count"].as_u64().unwrap_or(0) as usize,
            active_count: detail["active_count"].as_u64().unwrap_or(0) as usize,
            last_tick: serde_json::from_value(detail["scheduler_last_tick_at"].clone())
                .unwrap_or(None),
            in_flight: record.running,
            config_error: detail["config_error"].as_str().map(str::to_string),
            env_persistence: serde_json::from_value(detail["env_persistence"].clone()).ok(),
            unmet_env: serde_json::from_value(detail["unmet_env"].clone()).unwrap_or_default(),
        })
    }
    async fn env_coverage(&self) -> Result<EnvCoverage, CommandError> {
        self.request(reqwest::Method::GET, &["squad", "env"], &[], None)
            .await
    }
    async fn push_env(&self, push: EnvPush) -> Result<EnvPushResponse, CommandError> {
        self.request(
            reqwest::Method::PUT,
            &["squad", "env"],
            &[],
            Some(serde_json::to_value(push).expect("env push serializes")),
        )
        .await
    }
    async fn clear_env_store(&self) -> Result<bool, CommandError> {
        let v: Value = self
            .request(reqwest::Method::DELETE, &["squad", "env"], &[], None)
            .await?;
        Ok(v["cleared"].as_bool().unwrap_or(false))
    }
    async fn workflow_state(&self, name: &str) -> Result<Option<WorkflowState>, CommandError> {
        let task = self.resolve(name).await?;
        let response = self
            .request(
                reqwest::Method::GET,
                &["squad", "tasks", task.id.as_str(), "workflow"],
                &[],
                None,
            )
            .await;
        match response {
            Ok(v) => Ok(Some(v)),
            Err(CommandError::DaemonHttpStatus { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
