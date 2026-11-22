//! Interactive admission to the daemon's shared engine capacity.
use std::sync::Arc;

use crate::command::commands::config::DaemonReloader;
use crate::command::dispatch::CommandFrontend;
use crate::command::error::CommandError;
use crate::data::config::config_json::set_config_field;
use crate::data::config::file::{global_config_path, ConfigFile, FileKind};
use crate::data::daemon_state::FeatureName;
use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::data::message::UserMessageSink;
use crate::data::session::Session;
use crate::engine::daemon::executor::{ExecutorSnapshot, Permit, SharedExecutor};

use super::frontend::InstanceCommandFrontend;

const HOST_KEY: &str = "daemon.maxConcurrentJobs";
const ENGINE_KEY: &str = "daemon.features.engine.maxConcurrentJobs";

/// Called on the command worker, never on a Tokio worker or the UI thread.
pub(super) async fn acquire(
    executor: &Arc<SharedExecutor>,
    frontend: &InstanceCommandFrontend,
    session: &Session,
    reloader: &dyn DaemonReloader,
) -> Result<Permit, CommandError> {
    loop {
        if frontend.command_cancel.is_cancelled() {
            return Err(CommandError::Aborted);
        }
        // Reserve atomically: a tab count or a separate preflight check can
        // miss other instances and race with another command's admission.
        if let Some(permit) = executor.try_acquire(FeatureName::Engine) {
            return Ok(permit);
        }
        if frontend.is_cli() || !frontend.input_available() {
            return wait(executor, frontend).await;
        }
        let snapshot = executor.snapshot();
        let engine = &snapshot.per_feature[&FeatureName::Engine];
        let running: usize = snapshot.per_feature.values().map(|s| s.running).sum();
        let changes = increases(&snapshot);
        // A slot may have opened between try_acquire and snapshot.
        if changes.is_empty() {
            continue;
        }
        let settings = changes
            .iter()
            .map(|(key, value)| format!("{key} = {value}"))
            .collect::<Vec<_>>()
            .join("\n");
        let response = frontend.ask_dialog(DialogRequest::Custom {
            title: "Engine capacity reached".into(),
            body: format!(
                "This command has not started.\nEngine jobs: {} / {}\nAll daemon jobs: {} / {}\n\nIncrease capacity and continue?\nThese limits will be saved globally and applied now:\n{settings}\n\nOr wait for a job to finish, or cancel this command.",
                engine.running, engine.share, running, snapshot.cap,
            ),
            keys: vec![
                ('i', "Increase and continue".into()),
                ('w', "Wait for a slot".into()),
                ('c', "Cancel".into()),
            ],
        })?;
        match response {
            DialogResponse::Char('i') => {
                increase(session, reloader, &changes)?;
                let applied = executor.snapshot();
                if changes.iter().any(|(key, value)| {
                    let actual = if *key == HOST_KEY {
                        applied.cap
                    } else {
                        applied.per_feature[&FeatureName::Engine].share
                    };
                    actual < *value
                }) {
                    return Err(CommandError::Other(
                        "Capacity was saved, but the running engine still has a lower limit. Check daemon environment or command-line overrides.".into(),
                    ));
                }
                // Recheck admission; another command may have taken the slot.
            }
            DialogResponse::Char('w') => return wait(executor, frontend).await,
            _ => return Err(CommandError::Aborted),
        }
    }
}

async fn wait(
    executor: &Arc<SharedExecutor>,
    frontend: &InstanceCommandFrontend,
) -> Result<Permit, CommandError> {
    frontend.messages.clone().info(
        "Waiting for engine capacity; a running job must finish before this command can start.",
    );
    tokio::select! {
        biased;
        _ = frontend.command_cancel.cancelled() => Err(CommandError::Aborted),
        permit = executor.acquire(FeatureName::Engine) => Ok(permit),
    }
}

fn increases(snapshot: &ExecutorSnapshot) -> Vec<(&'static str, usize)> {
    let mut changes = Vec::new();
    if snapshot
        .per_feature
        .values()
        .map(|s| s.running)
        .sum::<usize>()
        >= snapshot.cap
    {
        changes.push((HOST_KEY, snapshot.cap.saturating_add(1)));
    }
    let engine = &snapshot.per_feature[&FeatureName::Engine];
    if engine.running >= engine.share {
        changes.push((ENGINE_KEY, engine.share.saturating_add(1)));
    }
    changes
}

fn increase(
    session: &Session,
    reloader: &dyn DaemonReloader,
    changes: &[(&str, usize)],
) -> Result<(), CommandError> {
    let daemon = reloader.running(session).ok_or_else(|| {
        CommandError::Other("Cannot increase capacity: the running daemon was not found.".into())
    })?;
    let path = global_config_path(session.env())?;
    let mut doc = ConfigFile::load(&path, FileKind::Global)?.to_value();
    for (key, value) in changes {
        // Preserve a larger setting saved while the modal was open.
        let saved = key
            .split('.')
            .try_fold(&doc, |v, part| v.get(part))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        set_config_field(&mut doc, key, serde_json::json!(saved.max(*value as u64)));
    }
    // Both bottlenecks change in one validated, atomic file write, then use
    // the same live reload as `config set` (no extra executor permit).
    ConfigFile::from_value(doc, FileKind::Global)?.save(&path, FileKind::Global)?;
    reloader.reload(&daemon).map_err(|error| {
        CommandError::Other(format!(
            "Capacity was saved to {}, but could not be applied: {error}",
            path.display(),
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::config::{ReloadReply, RunningDaemon};
    use crate::command::dispatch::{catalogue::CommandCatalogue, parsed_input::parse};
    use crate::command::instance::{feeds::TabCommandFeeds, frontend::DialogChannels};
    use crate::data::config::env::EnvSnapshot;
    use crate::data::session::SessionOpenOptions;
    use crate::engine::agent_runtime::frontend::AgentIo;
    use awman_test_support::{TestCase, TestSandbox};
    use std::sync::mpsc;

    struct Fixture {
        frontend: InstanceCommandFrontend,
        requests: std::sync::Mutex<mpsc::Receiver<DialogRequest>>,
        answers: mpsc::Sender<DialogResponse>,
        session: Session,
        executor: Arc<SharedExecutor>,
        reload_error: bool,
        sandbox: TestSandbox,
    }

    impl Fixture {
        fn new(id: &'static str, host: usize, engine: usize) -> Self {
            let sandbox = TestSandbox::for_test(TestCase::component(id));
            let env = EnvSnapshot::from_full_env(
                sandbox
                    .env()
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.to_string_lossy().into_owned(),
                            v.to_string_lossy().into_owned(),
                        )
                    })
                    .collect(),
            );
            let session = Session::open_at_directory_root(
                sandbox.work(),
                SessionOpenOptions {
                    env: Some(env),
                    ..Default::default()
                },
            )
            .unwrap();
            let (request_tx, requests) = mpsc::channel();
            let (answers, response_rx) = mpsc::channel();
            let (stdout, _) = tokio::sync::mpsc::unbounded_channel();
            let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
            let frontend = InstanceCommandFrontend::new(
                parse("ready", CommandCatalogue::get()).unwrap(),
                DialogChannels::new(request_tx, response_rx),
                AgentIo {
                    stdout: stdout.clone(),
                    stderr: stdout,
                    stdin_tx,
                    stdin_rx,
                    resize: None,
                    initial_size: None,
                },
                TabCommandFeeds::new(),
            );
            let executor = SharedExecutor::new(host);
            executor.set_share(FeatureName::Engine, engine);
            Self {
                frontend,
                requests: std::sync::Mutex::new(requests),
                answers,
                session,
                executor,
                reload_error: false,
                sandbox,
            }
        }
        async fn acquire(&self) -> Result<Permit, CommandError> {
            acquire(&self.executor, &self.frontend, &self.session, self).await
        }
        fn config_path(&self) -> std::path::PathBuf {
            global_config_path(self.session.env()).unwrap()
        }
        fn assert_prompt(&self) {
            let DialogRequest::Custom { title, keys, .. } =
                self.requests.lock().unwrap().try_recv().unwrap()
            else {
                panic!("expected capacity modal")
            };
            assert_eq!(title, "Engine capacity reached");
            assert_eq!(
                keys.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
                ['i', 'w', 'c']
            );
        }
    }
    impl DaemonReloader for Fixture {
        fn running(&self, _: &Session) -> Option<RunningDaemon> {
            Some(RunningDaemon {
                pid: 1,
                local_endpoint: self.sandbox.sock_dir().join("daemon"),
            })
        }
        fn reload(&self, _: &RunningDaemon) -> Result<ReloadReply, String> {
            if self.reload_error {
                return Err("reload failed".into());
            }
            let config = ConfigFile::load(&self.config_path(), FileKind::Global).unwrap();
            let daemon = config.daemon.unwrap();
            if let Some(cap) = daemon.max_concurrent_jobs {
                self.executor.set_cap(cap);
            }
            if let Some(cap) = daemon
                .features
                .and_then(|f| f.engine)
                .and_then(|e| e.max_concurrent_jobs)
            {
                self.executor.set_share(FeatureName::Engine, cap);
            }
            Ok(ReloadReply::default())
        }
    }

    #[tokio::test]
    async fn defaults_allow_eight_jobs_and_explicit_limits_are_preserved() {
        let f = Fixture::new("command::instance::capacity::tests::defaults_allow_eight_jobs_and_explicit_limits_are_preserved", 8, 8);
        let defaults = f.session.effective_config().daemon();
        let fallback =
            crate::data::config::daemon::DaemonSettings::from_merged(&Default::default());
        assert_eq!(
            (
                defaults.max_concurrent_jobs,
                defaults.engine.max_concurrent_jobs
            ),
            (8, 8)
        );
        assert_eq!(
            (
                fallback.max_concurrent_jobs,
                fallback.engine.max_concurrent_jobs
            ),
            (8, 8)
        );
        let mut permits = Vec::new();
        for _ in 0..8 {
            permits.push(f.acquire().await.unwrap());
        }
        assert!(f.requests.lock().unwrap().try_recv().is_err());
        f.answers.send(DialogResponse::Dismissed).unwrap();
        assert!(matches!(f.acquire().await, Err(CommandError::Aborted)));
        f.assert_prompt();
        assert!(!f.config_path().exists());
        let explicit = ConfigFile::from_value(
            serde_json::json!({"daemon": {
                "maxConcurrentJobs": 2, "features": {"engine": {"maxConcurrentJobs": 1}}
            }}),
            FileKind::Global,
        )
        .unwrap();
        let settings =
            crate::data::config::daemon::DaemonSettings::from_merged(&explicit.daemon.unwrap());
        assert_eq!(
            (
                settings.max_concurrent_jobs,
                settings.engine.max_concurrent_jobs
            ),
            (2, 1)
        );
    }

    #[tokio::test]
    async fn increase_saves_only_bottlenecks_and_admits_the_waiting_command() {
        for (host, engine, busy_feature) in [
            (1, 1, FeatureName::Engine),
            (2, 1, FeatureName::Engine),
            (1, 2, FeatureName::Worker),
        ] {
            let f = Fixture::new("command::instance::capacity::tests::increase_saves_only_bottlenecks_and_admits_the_waiting_command", host, engine);
            ConfigFile::from_value(
                serde_json::json!({"daemon": {"verboseSetup": true}}),
                FileKind::Global,
            )
            .unwrap()
            .save(&f.config_path(), FileKind::Global)
            .unwrap();
            f.executor.set_share(FeatureName::Worker, 1);
            let held = f.executor.try_acquire(busy_feature).unwrap();
            f.answers.send(DialogResponse::Char('i')).unwrap();
            let admitted = f.acquire().await.unwrap();
            f.assert_prompt();
            let snapshot = f.executor.snapshot();
            assert_eq!(snapshot.cap, 2);
            assert_eq!(snapshot.per_feature[&FeatureName::Engine].share, 2);
            assert_eq!(
                snapshot
                    .per_feature
                    .values()
                    .map(|s| s.running)
                    .sum::<usize>(),
                2
            );
            let doc = ConfigFile::load(&f.config_path(), FileKind::Global)
                .unwrap()
                .to_value();
            assert_eq!(doc["daemon"]["verboseSetup"], true);
            assert_eq!(
                doc["daemon"]["maxConcurrentJobs"].as_u64(),
                (host == 1).then_some(2)
            );
            assert_eq!(
                doc["daemon"]["features"]["engine"]["maxConcurrentJobs"].as_u64(),
                (engine == 1).then_some(2)
            );
            drop((held, admitted));
            assert_eq!(
                f.executor.snapshot().per_feature[&FeatureName::Engine].running,
                0
            );
        }
    }

    #[tokio::test]
    async fn wait_resumes_on_release_and_cancellation_removes_the_waiter() {
        for cancel in [false, true] {
            let f = Fixture::new("command::instance::capacity::tests::wait_resumes_on_release_and_cancellation_removes_the_waiter", 1, 1);
            let held = f.executor.try_acquire(FeatureName::Engine).unwrap();
            f.answers.send(DialogResponse::Char('w')).unwrap();
            let pending = f.acquire();
            tokio::pin!(pending);
            assert!(futures_util::poll!(&mut pending).is_pending());
            f.assert_prompt();
            assert_eq!(
                f.executor.snapshot().per_feature[&FeatureName::Engine].waiting,
                1
            );
            if cancel {
                f.frontend.command_cancel.cancel();
                assert!(matches!(pending.await, Err(CommandError::Aborted)));
                drop(held);
            } else {
                drop(held);
                drop(pending.await.unwrap());
            }
            let slots = &f.executor.snapshot().per_feature[&FeatureName::Engine];
            assert_eq!((slots.running, slots.waiting), (0, 0));
            assert!(!f.config_path().exists());
        }
    }

    #[tokio::test]
    async fn cancel_and_reload_failure_never_start_the_command() {
        for response in [
            DialogResponse::Char('c'),
            DialogResponse::Dismissed,
            DialogResponse::Char('i'),
        ] {
            let mut f = Fixture::new("command::instance::capacity::tests::cancel_and_reload_failure_never_start_the_command", 1, 1);
            f.reload_error = true;
            let _held = f.executor.try_acquire(FeatureName::Engine).unwrap();
            let increasing = matches!(response, DialogResponse::Char('i'));
            f.answers.send(response).unwrap();
            let result = f.acquire().await;
            if increasing {
                assert!(
                    matches!(result, Err(CommandError::Other(ref text)) if text.contains("could not be applied"))
                );
            } else {
                assert!(matches!(result, Err(CommandError::Aborted)));
            }
            f.assert_prompt();
            let snapshot = f.executor.snapshot();
            assert_eq!(snapshot.cap, 1);
            assert_eq!(snapshot.per_feature[&FeatureName::Engine].running, 1);
            assert_eq!(snapshot.per_feature[&FeatureName::Engine].waiting, 0);
            assert_eq!(f.config_path().exists(), increasing);
        }
    }
}
