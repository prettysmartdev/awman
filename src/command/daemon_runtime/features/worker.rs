//! Worker shared handles and the supervised push pool.
use crate::command::commands::api_server::{
    cancel::TokenRegistry,
    queue_worker::{ApiCommandFrontendFactory, QueueWorker, QueueWorkerDeps},
};
use crate::command::daemon_api::worker::WorkerApi;
use crate::data::{config::daemon::WorkerSettings, daemon_state::*};
use crate::engine::{
    daemon::{core::Core, feature::*},
    error::EngineError,
};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
pub struct WorkerShared {
    pub api: Arc<WorkerApi>,
    pub command_cancels: Arc<TokenRegistry>,
}
impl std::ops::Deref for WorkerShared {
    type Target = WorkerApi;
    fn deref(&self) -> &WorkerApi {
        &self.api
    }
}
impl WorkerShared {
    pub fn workdirs(&self) -> (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>) {
        let config = self.live_config.read().unwrap();
        match config.as_ref() {
            Some(c) => (
                c.worker_push_work_dirs()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                c.worker_push_work_dir_roots()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            ),
            None => (self.api.workdirs.clone(), self.api.workdir_roots.clone()),
        }
    }
    pub fn cancel_all(&self) -> Result<Vec<String>, crate::command::error::CommandError> {
        let mut ids = Vec::new();
        for status in [
            crate::data::command_status::CommandStatus::Queued,
            crate::data::command_status::CommandStatus::Running,
        ] {
            for row in
                self.store
                    .list_commands_filtered(&crate::data::fs::api_db::CommandListFilter {
                        status: Some(status),
                        session_id: None,
                        labels: &[],
                        limit: i64::MAX,
                    })?
            {
                self.canceller.cancel(&row.id)?;
                ids.push(row.id);
            }
        }
        Ok(ids)
    }
}
pub struct WorkerFeature<F: ApiCommandFrontendFactory + 'static> {
    shared: Arc<WorkerShared>,
    factory: Arc<F>,
    settings_tx: watch::Sender<WorkerSettings>,
    stop: Mutex<Option<CancellationToken>>,
    done: Mutex<Option<CancellationToken>>,
    pool_abort: Mutex<Option<tokio::task::AbortHandle>>,
    aborts: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
}
impl<F: ApiCommandFrontendFactory + 'static> WorkerFeature<F> {
    pub fn new(shared: Arc<WorkerShared>, factory: F, settings: WorkerSettings) -> Arc<Self> {
        let (settings_tx, _) = watch::channel(settings);
        Arc::new(Self {
            shared,
            factory: Arc::new(factory),
            settings_tx,
            stop: Mutex::new(None),
            done: Mutex::new(None),
            pool_abort: Mutex::new(None),
            aborts: Arc::new(Mutex::new(Vec::new())),
        })
    }
    pub fn apply_settings(&self, s: WorkerSettings) {
        self.settings_tx.send_replace(s);
    }
}
#[async_trait::async_trait]
impl<F: ApiCommandFrontendFactory + 'static> DaemonFeature for WorkerFeature<F> {
    fn name(&self) -> FeatureName {
        FeatureName::Worker
    }
    async fn start(&self, core: &Core) -> Result<FeatureTask, EngineError> {
        let stop = CancellationToken::new();
        *self.stop.lock().unwrap() = Some(stop.clone());
        let done = CancellationToken::new();
        *self.done.lock().unwrap() = Some(done.clone());
        let done_guard = done.drop_guard();
        let shared = self.shared.clone();
        let factory = self.factory.clone();
        let mut settings = self.settings_tx.subscribe();
        let executor = core.executor().clone();
        let aborts = self.aborts.clone();
        let task = tokio::spawn(async move {
            let _done = done_guard;
            let make = || {
                QueueWorker::new(
                    format!("worker-{}", uuid::Uuid::new_v4()),
                    QueueWorkerDeps {
                        store: shared.store.clone(),
                        engines: shared.engines.clone(),
                        sessions: shared.sessions.clone(),
                        event_buses: shared.event_buses.clone(),
                        paths: shared.paths.clone(),
                        frontend_factory: factory.clone(),
                        lifecycle: shared.lifecycle(),
                        lifecycle_store: shared.lifecycle_store.clone(),
                        cancels: shared.command_cancels.clone(),
                    },
                )
                .with_executor(executor.clone())
            };
            crate::command::commands::api_server::queue_worker::finalize_completed_from_sidecars(
                &shared.store,
                &shared.paths,
            )?;
            shared
                .lifecycle_store
                .finalize_interrupted_cancellations(&chrono::Utc::now().to_rfc3339())?;
            for id in shared.store.recover_stale_commands(0)? {
                shared.command_cancels.unregister(&id);
                crate::command::commands::api_server::owned_runtime::stop_command_containers(
                    shared.engines.runtime.as_ref(),
                    &id,
                )?;
            }
            make()
                .recover_cancelled_cleanup()
                .await
                .map_err(|e| EngineError::Other(e.to_string()))?;
            let mut workers: Vec<(CancellationToken, WorkerTask)> = Vec::new();
            loop {
                if stop.is_cancelled() {
                    break;
                }
                aborts.lock().unwrap().retain(|task| !task.is_finished());
                // Worker panics are feature crashes, never silently lost capacity.
                let mut index = 0;
                while index < workers.len() {
                    if workers[index].1.is_finished() {
                        let (token, job) = workers.remove(index);
                        if let Err(e) = job.await {
                            for (stop, _) in &workers {
                                stop.cancel();
                            }
                            for (_, job) in workers {
                                let _ = job.await;
                            }
                            return Err(EngineError::Other(e.to_string()));
                        }
                        if !token.is_cancelled() {
                            return Err(EngineError::Other(
                                "queue worker exited unexpectedly".into(),
                            ));
                        }
                    } else {
                        index += 1;
                    }
                }
                let s = settings.borrow_and_update().clone();
                let target = if s.push_enabled {
                    s.max_concurrent_jobs
                } else {
                    0
                };
                let active = workers.iter().filter(|(t, _)| !t.is_cancelled()).count();
                for (token, _) in workers
                    .iter()
                    .rev()
                    .filter(|(t, _)| !t.is_cancelled())
                    .take(active.saturating_sub(target))
                {
                    token.cancel();
                }
                for _ in workers.len()..target {
                    let token = stop.child_token();
                    let worker = make();
                    let task = tokio::spawn(worker.run_until(token.clone()));
                    aborts.lock().unwrap().push(task.abort_handle());
                    workers.push((token, WorkerTask(task)));
                }
                tokio::select! { biased; _=stop.cancelled()=>break, result=settings.changed()=>if result.is_err() { break; }, _=tokio::time::sleep(std::time::Duration::from_millis(100))=>{} }
            }
            for (token, _) in &workers {
                token.cancel();
            }
            for (_, job) in workers {
                job.await.map_err(|e| EngineError::Other(e.to_string()))?;
            }
            Ok(())
        });
        *self.pool_abort.lock().unwrap() = Some(task.abort_handle());
        Ok(task)
    }
    async fn drain(&self) {
        if let Some(stop) = self.stop.lock().unwrap().take() {
            stop.cancel();
        }
        let done = self.done.lock().unwrap().clone();
        if let Some(done) = done {
            done.cancelled().await;
        }
    }
    async fn stop_now(&self) {
        if let Some(stop) = self.stop.lock().unwrap().take() {
            stop.cancel();
        }
        self.shared.setup_cancels.cancel_all();
        if let Err(e) = self.shared.cancel_all() {
            tracing::error!(%e,"failed to cancel worker jobs");
        }
        for id in self.shared.command_cancels.cancel_all() {
            if let Err(error) =
                crate::command::commands::api_server::owned_runtime::stop_command_containers(
                    self.shared.engines.runtime.as_ref(),
                    &id,
                )
            {
                tracing::error!(%error,%id,"failed to stop cancelled command containers");
            }
            self.shared.command_cancels.unregister(&id);
        }
        for task in self.aborts.lock().unwrap().drain(..) {
            task.abort();
        }
        for task in self.shared.task_handles.lock().await.iter() {
            task.abort();
        }
        if let Some(task) = self.pool_abort.lock().unwrap().take() {
            task.abort();
        }
        self.drain().await;
    }
    fn status(&self) -> FeatureStatus {
        let queued = self
            .shared
            .store
            .list_commands_filtered(&crate::data::fs::api_db::CommandListFilter {
                status: Some(crate::data::command_status::CommandStatus::Queued),
                session_id: None,
                labels: &[],
                limit: i64::MAX,
            })
            .unwrap_or_default();
        FeatureStatus {
            running: self.shared.store.count_running_commands().unwrap_or(0) as usize,
            queued: queued.len(),
            oldest_queued_age: queued
                .iter()
                .filter_map(|r| {
                    r.queued_at
                        .as_deref()
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                })
                .min()
                .and_then(|t| {
                    (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
                        .to_std()
                        .ok()
                }),
            detail: None,
        }
    }
    fn share(&self) -> usize {
        self.settings_tx.borrow().max_concurrent_jobs
    }
}

/// A panicking pool cannot detach children that keep claiming commands.
struct WorkerTask(tokio::task::JoinHandle<()>);
impl WorkerTask {
    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}
impl std::future::Future for WorkerTask {
    type Output = Result<(), tokio::task::JoinError>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}
impl Drop for WorkerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}
