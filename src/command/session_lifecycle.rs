//! Owns admitted session setup independently of the request that created it.
//!
//! Admission, token registration and scheduling have one owner. Presentation
//! locks are acquired by the tracked task, so a disconnected caller cannot
//! abandon an admitted session while waiting to publish its setup bus.

use std::sync::Arc;

use crate::command::commands::api_server::admission::{
    AdmissionError, AdmittedSession, CommandSubmission, IdempotentAdmission, SessionSubmission,
};
use crate::command::commands::api_server::session_setup::SessionSetupBus;
use crate::command::daemon_api::worker::WorkerApi;
use crate::command::session_setup::{SessionSetup, SessionSetupPresenter};
use crate::data::daemon_state::CommandRequestContext;

/// A setup's shutdown handle. The worker's stop protocol calls `abort` and
/// then awaits each handle. For setup, stopping must cancel the operation and
/// join its finalization rather than dropping the future that owns cleanup.
pub struct SessionSetupTask {
    task: tokio::task::JoinHandle<()>,
    cancel: tokio_util::sync::CancellationToken,
}

impl SessionSetupTask {
    pub fn abort(&self) {
        self.cancel.cancel();
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl std::future::IntoFuture for SessionSetupTask {
    type Output = Result<(), tokio::task::JoinError>;
    type IntoFuture = tokio::task::JoinHandle<()>;

    fn into_future(self) -> Self::IntoFuture {
        self.task
    }
}

// Constructed before spawning, so aborting even an unpolled setup releases its
// registration and any credential whose setup never got to run its own guard.
struct SetupRegistration {
    registry: Arc<crate::command::commands::api_server::TokenRegistry>,
    secrets: Arc<crate::command::dispatch::ephemeral_secrets::EphemeralSecrets>,
    session_id: String,
    completed: bool,
}

impl Drop for SetupRegistration {
    fn drop(&mut self) {
        self.registry.unregister(&self.session_id);
        if !self.completed {
            self.secrets.remove_session_git_auth(&self.session_id);
        }
    }
}

pub struct SessionLifecycle;

impl SessionLifecycle {
    pub async fn create(
        api: &Arc<WorkerApi>,
        context: CommandRequestContext,
        request: SessionSubmission,
        command: Option<CommandSubmission>,
        close_on_finish: bool,
        journal: Option<&IdempotentAdmission>,
        present: impl FnOnce(&str, Arc<SessionSetupBus>) -> Box<dyn SessionSetupPresenter>
            + Send
            + 'static,
    ) -> Result<AdmittedSession, AdmissionError> {
        if !api.push_enabled() {
            return Err(AdmissionError::FeatureDisabled);
        }
        // Reserve task ownership before admission can commit. There must be no
        // suspension point between an accepted intent and its registered task.
        let (admitted, mut tasks) = api
            .admission()
            .with_request_context(context)
            .admit_session_owned(request, command, close_on_finish, journal, || async {
                let tasks = api.task_handles.lock().await;
                if api
                    .setup_stopping
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    return Err(AdmissionError::FeatureDisabled);
                }
                Ok(tasks)
            })
            .await?;
        let session_id = admitted.session_id.clone();
        let plan = admitted.plan.clone();
        let run = admitted.run.clone();
        let token = api.setup_cancels.register(&session_id);
        let mut registration = SetupRegistration {
            registry: api.setup_cancels.clone(),
            secrets: api.engines.ephemeral_secrets.clone(),
            session_id: session_id.clone(),
            completed: false,
        };
        let bus = Arc::new(SessionSetupBus::new(256));
        let api = Arc::clone(api);
        tasks.retain(|task| !task.is_finished());
        let cancel = token.clone();
        let task = tokio::spawn(async move {
            // Lists can expose the command before token registration. Carry a
            // cancellation already committed there into setup's abort path,
            // before waiting for any presentation lock.
            if let Some(run) = &run {
                if let Err(error) = api.lifecycle().reconcile_setup_cancel(run, &token) {
                    tracing::error!(%error, %session_id, "Failed to read early setup cancellation");
                }
            }
            // Publishing history may wait on an HTTP reader; cancellation and
            // shutdown must still be able to finish without that reader.
            tokio::select! {
                biased;
                _ = token.cancelled() => {}
                mut buses = api.setup_buses.lock() => {
                    buses.insert(session_id.clone(), bus.clone());
                }
            }
            use futures_util::FutureExt as _;
            let setup = std::panic::AssertUnwindSafe(async {
                let mut presenter = present(&session_id, bus.clone());
                SessionSetup::new(
                    session_id.clone(),
                    plan,
                    api.engines.clone(),
                    api.sessions.clone(),
                    bus.current_state.clone(),
                    api.store.clone(),
                    api.paths.clone(),
                )
                .with_cancellation(token)
                .run(presenter.as_mut())
                .await;
            })
            .catch_unwind()
            .await;
            if setup.is_err() {
                api.setup_cancels.unregister(&session_id);
                if let Err(error) = api.store.update_setup_status(
                    &session_id,
                    crate::data::session_setup_event::SessionSetupStatus::Failed,
                ) {
                    tracing::error!(%error, "failed to record setup panic");
                }
                if let Some(run) = &run {
                    let result = serde_json::json!({"error":"session setup panicked", "setup_error":{"stage":"setup","message":"session setup panicked","code":"setup_failed"}}).to_string();
                    if let Err(error) = api.lifecycle_store.fail_if_queued(
                        run.command_id.as_str(),
                        &chrono::Utc::now().to_rfc3339(),
                        &result,
                    ) {
                        tracing::error!(%error, %session_id, "failed to record panicked run");
                    }
                }
                tracing::error!(%session_id, "session setup panicked; closing owned session");
                if let Err(error) = api.lifecycle().close(&session_id).await {
                    tracing::error!(%error, %session_id, "failed to close panicked setup");
                }
            }
            // Hand cancellation ownership back before inspecting the final row:
            // either cancellation or this task will see and close a terminal run.
            registration.completed = true;
            drop(registration);
            if let Some(run) = run {
                for attempt in 0..3 {
                    match api
                        .lifecycle()
                        .finish_run_setup(&api.lifecycle_store, &run)
                        .await
                    {
                        Ok(()) => break,
                        Err(error) => {
                            tracing::error!(%error, %session_id, attempt, "Failed to reconcile run after setup");
                            if attempt == 2 {
                                if let Err(error) = api.lifecycle().close(&session_id).await {
                                    tracing::error!(%error, %session_id, "run setup finalization requires recovery");
                                }
                            } else {
                                tokio::task::yield_now().await;
                            }
                        }
                    }
                }
            }
            // History retention holds no daemon/store/credential ownership.
            let weak = Arc::downgrade(&api);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                loop {
                    // Never retain the daemon across a presentation-lock wait,
                    // including when retention expires during shutdown.
                    if let Some(api) = weak.upgrade() {
                        if let Ok(mut buses) = api.setup_buses.try_lock() {
                            buses.remove(&session_id);
                            return;
                        }
                    } else {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            });
        });
        tasks.push(SessionSetupTask { task, cancel });
        Ok(admitted)
    }
}
