//! Cancellation semantics of the exec bridge, against the in-memory driver.
use super::*;
use crate::engine::container::builtin::{naming, testing::*};

fn sandbox(driver: &Arc<FakeDriver>, name: &str, token: &str) -> String {
    let spec = SandboxSpec {
        name: naming::sandbox_name_for(name),
        image: "agent:latest".into(),
        labels: naming::labels(name, token, naming::TEST_LAUNCH, &[]).unwrap(),
        mounts: Vec::new(),
        mount_owner: None,
        vcpus: 1,
        memory_mib: 256,
        root_disk_mib: 512,
        mount_quota_mib: 64,
        network: Default::default(),
    };
    let id = spec.name.clone();
    driver.create(spec).unwrap();
    id
}

#[test]
fn aborting_an_unconfirmed_observer_preserves_path_but_closes_descriptor() {
    use crate::engine::container::options::PromptFileGuard;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let file = tempfile::NamedTempFile::new_in(tmp.path()).unwrap();
    let path = file.path().to_owned();
    let guard = PromptFileGuard::new(file);
    let sibling = guard.clone();
    runtime.block_on(async {
        let (started, ready) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _files = ObservedPromptFiles(vec![guard]);
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
    });
    drop(runtime);
    assert!(
        path.exists(),
        "observer shutdown does not establish guest exit"
    );
    drop(sibling);
    assert!(
        path.exists(),
        "the last clone preserves the unknown guest's path"
    );
    assert_eq!(
        open_prompt_descriptors(&path),
        0,
        "no forgotten allocation or fd"
    );
}

fn open_prompt_descriptors(path: &Path) -> usize {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter(|target| {
                target == path
                    || target.to_string_lossy() == format!("{} (deleted)", path.display())
            })
            .count()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        0
    }
}

#[tokio::test]
async fn prompt_files_follow_confirmed_exit_or_stop_including_a_later_retry() {
    use crate::engine::container::options::PromptFileGuard;
    for (exited, failed_stop, owned) in [
        (false, false, true),
        (true, true, true),
        (false, true, true),
        (false, false, false),
    ] {
        let driver = FakeDriver::new();
        let id = sandbox(&driver, "awman-prompt", "owner");
        driver.state.lock().unwrap().fail_finish = failed_stop;
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_owned();
        let (mut frontend, _peer) = frontend(None);
        let io = frontend.take_io();
        let (control, controls) = recorder();
        let (broadcast, events) = tokio::sync::broadcast::channel(16);
        let mut execution = ExecBridge::new(ExecBridgeOptions {
            handle: AgentHandle {
                id,
                name: "awman-prompt".into(),
                image_tag: "fixture".into(),
                started_at: chrono::Utc::now(),
            },
            session: ExecSession {
                events,
                broadcast: broadcast.clone(),
                control,
            },
            cleanup: owned.then(|| CleanupTarget {
                driver: driver.clone(),
                owner: "owner".into(),
                remove_on_exit: true,
            }),
            attach_path: None,
            persistent_stdin: false,
            seeded_prompt: None,
            leases: vec![],
            prompt_files: vec![PromptFileGuard::new(file)],
            io,
        })
        .run(frontend)
        .unwrap();
        let cancel = execution.cancel_handle().unwrap();
        assert!(broadcast
            .send(if exited {
                ExecEvent::Exited(0)
            } else {
                ExecEvent::Failed
            })
            .is_ok());
        assert_eq!(execution.wait().await.is_ok(), exited);
        if !owned {
            assert!(
                path.exists(),
                "an attached guest is still live after stream failure"
            );
            cancel.cancel().unwrap();
            assert!(driver.finished().is_empty());
            assert!(!controls.lock().unwrap().contains(&Sent::Kill));
            assert!(broadcast.send(ExecEvent::Exited(0)).is_ok());
            settle("attached guest exit releases prompt", || !path.exists()).await;
        } else if !exited && failed_stop {
            assert!(path.exists(), "unknown liveness retains the prompt");
            #[cfg(target_os = "linux")]
            assert_eq!(open_prompt_descriptors(&path), 1);
            assert_eq!(driver.state.lock().unwrap().finish_attempts, 1);
            driver.state.lock().unwrap().fail_finish = false;
            cancel.cancel().unwrap();
            tokio::time::timeout(Duration::from_secs(12), async {
                while path.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("later owned stop releases retained prompt");
        } else {
            assert!(
                !path.exists(),
                "confirmed termination releases prompt before wait resolves"
            );
        }
        assert_eq!(
            open_prompt_descriptors(&path),
            0,
            "the descriptor is released too"
        );
    }
}

#[tokio::test]
async fn failed_grace_stop_allows_another_cancel() {
    let driver = FakeDriver::new();
    let id = sandbox(&driver, "awman-retry-stop", "owner");
    driver.state.lock().unwrap().fail_finish = true;
    let (control, _) = recorder();
    let (owner, _) = target(&driver, &id, "owner", true);
    let cancel = ExecCancelHandle::new(control, Some(owner), Duration::from_millis(10));
    cancel.cancel().unwrap();
    settle("failed stop releases cancellation request", || {
        driver.state.lock().unwrap().finish_attempts == 1
            && !cancel.requested.load(Ordering::Acquire)
    })
    .await;
    driver.state.lock().unwrap().fail_finish = false;
    cancel.cancel().unwrap();
    settle("retry stopped guest", || driver.finished().len() == 1).await;
}

fn target(
    driver: &Arc<FakeDriver>,
    id: &str,
    token: &str,
    remove: bool,
) -> (CancelTarget, Arc<AtomicBool>) {
    let finished = Arc::new(AtomicBool::new(false));
    (
        CancelTarget {
            driver: driver.clone(),
            id: id.into(),
            token: token.into(),
            remove,
            cleanup_claimed: finished.clone(),
            stop_result: Arc::new(tokio::sync::watch::channel(None).0),
        },
        finished,
    )
}

async fn settle(what: &str, condition: impl Fn() -> bool) {
    eventually(what, condition).await;
}

#[tokio::test]
async fn an_agent_that_ignores_interrupt_is_killed_and_removed_after_the_grace() {
    let driver = FakeDriver::new();
    let id = sandbox(&driver, "awman-stubborn", "a/1");
    let (control, log) = recorder();
    let (owner, _finished) = target(&driver, &id, "a/1", true);
    ExecCancelHandle::new(control, Some(owner), Duration::from_millis(100))
        .cancel()
        .unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        [Sent::Interrupt],
        "interrupt goes first"
    );
    settle("kill after the grace", || driver.finished().len() == 1).await;
    assert_eq!(*log.lock().unwrap(), [Sent::Interrupt, Sent::Kill]);
    assert_eq!(driver.finished()[0], (id.clone(), "a/1".into(), true));
    assert_eq!(driver.is_stopped(&id), None, "removed");
}

/// When the grace timer cleans up first, it claims cleanup: the exit path that
/// follows (the agent dying of the kill) sees the claim and does not run
/// `finish_owned` a second time, concurrently.
#[tokio::test]
async fn the_grace_timer_claims_cleanup_so_the_exit_path_skips_it() {
    let driver = FakeDriver::new();
    let id = sandbox(&driver, "awman-claimed", "a/1");
    let (control, _log) = recorder();
    let (owner, claimed) = target(&driver, &id, "a/1", true);
    ExecCancelHandle::new(control, Some(owner), Duration::from_millis(50))
        .cancel()
        .unwrap();
    settle("kill after the grace", || driver.finished().len() == 1).await;
    assert!(
        claimed.swap(true, Ordering::AcqRel),
        "the exit path must find cleanup already claimed"
    );
    assert_eq!(driver.finished().len(), 1);
}

#[tokio::test]
async fn keep_is_honoured_when_a_cancel_has_to_kill() {
    let driver = FakeDriver::new();
    let id = sandbox(&driver, "awman-kept", "a/1");
    let (control, _log) = recorder();
    let (owner, _finished) = target(&driver, &id, "a/1", false);
    ExecCancelHandle::new(control, Some(owner), Duration::from_millis(50))
        .cancel()
        .unwrap();
    settle("kill after the grace", || driver.finished().len() == 1).await;
    assert!(!driver.finished()[0].2);
    assert_eq!(driver.is_stopped(&id), Some(true), "stopped but preserved");
}

#[tokio::test]
async fn no_kill_when_the_exit_path_finishes_inside_the_grace() {
    let driver = FakeDriver::new();
    let id = sandbox(&driver, "awman-graceful", "a/1");
    let (control, log) = recorder();
    let (owner, finished) = target(&driver, &id, "a/1", true);
    ExecCancelHandle::new(control, Some(owner), Duration::from_millis(600))
        .cancel()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    // The exit path claims cleanup first.
    finished.store(true, Ordering::Release);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(*log.lock().unwrap(), [Sent::Interrupt]);
    assert!(driver.finished().is_empty(), "the exit path owns cleanup");
    assert_eq!(driver.is_stopped(&id), Some(false));
}

#[tokio::test]
async fn a_delayed_cancel_from_a_previous_launch_cannot_stop_the_replacement() {
    let driver = FakeDriver::new();
    // The replacement launch reused the name but has a new owner token.
    let id = sandbox(&driver, "awman-reused", "session/launch-2");
    let (control, log) = recorder();
    let (stale_owner, _finished) = target(&driver, &id, "session/launch-1", true);
    ExecCancelHandle::new(control, Some(stale_owner), Duration::from_millis(50))
        .cancel()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        driver.finished().is_empty(),
        "stale token must not authorise cleanup"
    );
    assert_eq!(driver.is_stopped(&id), Some(false));
    drop(log);
}

#[tokio::test]
async fn cancel_without_ownership_only_interrupts() {
    let (control, log) = recorder();
    ExecCancelHandle::new(control, None, Duration::from_millis(10))
        .cancel()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(*log.lock().unwrap(), [Sent::Interrupt]);
}

#[tokio::test]
async fn repeated_cancel_uses_one_interrupt_and_one_grace_task() {
    let driver = FakeDriver::new();
    let id = sandbox(&driver, "awman-repeated", "owner");
    let (control, log) = recorder();
    let (owner, _finished) = target(&driver, &id, "owner", true);
    let cancel = ExecCancelHandle::new(control, Some(owner), Duration::from_millis(30));
    cancel.cancel().unwrap();
    cancel.clone().cancel().unwrap();
    cancel.cancel().unwrap();
    settle("one cancellation cleanup", || driver.finished().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(*log.lock().unwrap(), [Sent::Interrupt, Sent::Kill]);
    assert_eq!(driver.finished().len(), 1);
}
