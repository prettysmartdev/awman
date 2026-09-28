//! Cancellation semantics of the exec bridge, against the in-memory driver.
use super::*;
use crate::engine::container::builtin::{naming, testing::*};

fn sandbox(driver: &Arc<FakeDriver>, name: &str, token: &str) -> String {
    let spec = SandboxSpec {
        name: naming::sandbox_name_for(name),
        image: "agent:latest".into(),
        labels: naming::labels(name, token, &[]).unwrap(),
        mounts: Vec::new(),
        mount_owner: None,
        vcpus: 1,
        memory_mib: 256,
    };
    let id = spec.name.clone();
    driver.create(spec).unwrap();
    id
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
            finished: finished.clone(),
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
    cancel(control, Some(owner), Duration::from_millis(100)).unwrap();
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

#[tokio::test]
async fn keep_is_honoured_when_a_cancel_has_to_kill() {
    let driver = FakeDriver::new();
    let id = sandbox(&driver, "awman-kept", "a/1");
    let (control, _log) = recorder();
    let (owner, _finished) = target(&driver, &id, "a/1", false);
    cancel(control, Some(owner), Duration::from_millis(50)).unwrap();
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
    cancel(control, Some(owner), Duration::from_millis(600)).unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
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
    cancel(control, Some(stale_owner), Duration::from_millis(50)).unwrap();
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
    cancel(control, None, Duration::from_millis(10)).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(*log.lock().unwrap(), [Sent::Interrupt]);
}
