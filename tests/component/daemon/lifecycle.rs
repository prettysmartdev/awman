use crate::support::{
    self,
    backend::{Backend, BackendKind},
};
use awman::command::commands::daemon::actions::Request;
use std::time::Duration;

/// Guards: 585680c3, f23e75a4, 9eac9cda. Readiness requires an authenticated request to
/// the exact owned endpoint. Two simultaneous daemons must stay disjoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_585680c3_f23e75a4_9eac9cda_two_ready_daemons_have_distinct_state() {
    let a = support::sandbox(
        "daemon::lifecycle::regression_585680c3_f23e75a4_9eac9cda_two_ready_daemons_have_distinct_state",
    );
    let b =
        support::sandbox("daemon::lifecycle::regression_585680c3_f23e75a4_9eac9cda_second_owner");
    for sandbox in [&a, &b] {
        let docker = sandbox.install_fake(support::fake_cli_exe(), "docker");
        awman_test_support::fakes::docker_cli_baseline(&docker);
    }
    let first = Backend::start(BackendKind::Daemon, &a).await;
    let second = Backend::start(BackendKind::Daemon, &b).await;
    assert_ne!(first.endpoint(), second.endpoint());
    assert_ne!(first.pid(), second.pid());
    for (sandbox, backend) in [(&a, &first), (&b, &second)] {
        let status = support::awman(sandbox)
            .args(["daemon", "status"])
            .envs(&backend.env())
            .deadline(Duration::from_secs(10))
            .run();
        status.assert_success();
        let status = backend
            .client()
            .call(&Request::get("/v1/status"))
            .await
            .unwrap();
        assert_eq!(status["pid"].as_u64(), backend.pid().map(u64::from));
    }
    first.shutdown().await;
    let status = second
        .client()
        .call(&Request::get("/v1/status"))
        .await
        .unwrap();
    assert_eq!(status["pid"].as_u64(), second.pid().map(u64::from));
    second.shutdown().await;
    a.finish();
    b.finish();
}
