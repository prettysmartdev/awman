//! Scriptable HTTP worker for implementers testing a WSP server.
use crate::{labels::WorkerLabels, *};
use serde_json::Value;
#[derive(Debug)]
pub struct MockWorker {
    pub context_changes: Option<fn(&Lease) -> ContextChanges>,
    pub workflow_context: Option<fn(&Lease) -> WorkflowContextResult>,
    pub client: Client,
    name: String,
    labels: WorkerLabels,
    features: Vec<String>,
    pub registration: Option<RegisterResponse>,
}
#[derive(Debug, Clone)]
pub enum MockOutcome {
    Succeed { result: Value, events: Vec<Value> },
    Release(ReleaseReason),
    Vanish,
}
#[derive(Debug, Clone)]
pub struct LeaseReport {
    pub lease: Lease,
    pub outcome: MockOutcome,
}
impl MockWorker {
    pub fn new(client: Client, name: &str, labels: WorkerLabels, features: Vec<String>) -> Self {
        Self {
            context_changes: None,
            workflow_context: None,
            client,
            name: name.into(),
            labels,
            features,
            registration: None,
        }
    }
    pub async fn register(&mut self) -> Result<RegisterResponse, Error> {
        let req = RegisterRequest::with_labels(
            self.name.clone(),
            self.labels.reported.clone(),
            self.labels.configured.clone(),
            1,
            self.features.clone(),
        )
        .map_err(|_| Error::InvalidResponse)?;
        let response = self.client.register(&req).await?;
        self.labels = self
            .labels
            .clone()
            .with_server(response.server_labels.clone())
            .map_err(|_| Error::InvalidResponse)?;
        self.registration = Some(response.clone());
        Ok(response)
    }
    pub async fn run_once(
        &mut self,
        mut outcome: MockOutcome,
    ) -> Result<Option<LeaseReport>, Error> {
        if self.registration.is_none() {
            self.register().await?;
        }
        let Some(lease) = self.client.claim(None, 0).await? else {
            return Ok(None);
        };
        if !matches!(outcome, MockOutcome::Vanish) {
            let heartbeat = self
                .client
                .heartbeat(
                    &lease.id,
                    &HeartbeatRequest {
                        state: HeartbeatState::Running,
                        step: None,
                        progress: None,
                    },
                )
                .await?;
            if heartbeat.cancel {
                outcome = MockOutcome::Release(ReleaseReason::Error);
                self.client
                    .release(
                        &lease.id,
                        &ReleaseRequest::<Value> {
                            context_changes: self.context_changes.map(|hook| hook(&lease)),
                            reason: ReleaseReason::Error,
                            message: heartbeat.cancel_reason,
                            result: None,
                        },
                    )
                    .await?;
            } else {
                match &outcome {
                    MockOutcome::Succeed { result, events } => {
                        self.client.events(&lease.id, events.clone()).await?;
                        self.client
                            .result(
                                &lease.id,
                                &LeaseResult {
                                    context_changes: self.context_changes.map(|hook| hook(&lease)),
                                    workflow_context: self
                                        .workflow_context
                                        .map(|hook| hook(&lease)),
                                    result: result.clone(),
                                    outputs: None,
                                    outputs_mounted: false,
                                    duration_ms: 0,
                                },
                            )
                            .await?;
                    }
                    MockOutcome::Release(reason) => {
                        self.client
                            .release(
                                &lease.id,
                                &ReleaseRequest::<Value> {
                                    context_changes: self.context_changes.map(|hook| hook(&lease)),
                                    reason: *reason,
                                    message: None,
                                    result: None,
                                },
                            )
                            .await?
                    }
                    MockOutcome::Vanish => unreachable!(),
                }
            }
        }
        Ok(Some(LeaseReport { lease, outcome }))
    }
}
