//! Latency of the production daemon transport, measured and reported, never
//! asserted against a wall-clock budget (no timing assertion belongs in a
//! gate that can fail a candidate).
//!
//! The legacy engine-parity benchmark (submit and dialog p95 over the
//! in-process daemon fixture) is not re-homed here; see the hand-off note.
//! This measures what the boundary binary can reach without that fixture:
//! the status round trip of a real `awman daemon` over its owner-only Unix
//! socket.

use crate::support;
use awman::command::commands::daemon::{actions::Request, client::DaemonClient};
use awman::data::fs::DataPaths;
use awman_test_support::capability::Docker;
use awman_test_support::{Exit, Ready};
use std::time::{Duration, Instant};

const WARMUP: usize = 25;
const SAMPLES: usize = 300;

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn daemon_status_round_trip_latency_is_recorded() {
    let mut sandbox =
        support::soak_sandbox("soak::latency::daemon_status_round_trip_latency_is_recorded");
    let docker = sandbox.require::<Docker>();
    sandbox.grant(&docker);
    let env = support::env_snapshot(&sandbox);
    let paths = DataPaths::from_env(&env).expect("data paths").daemon();
    let mut daemon = support::awman(&sandbox)
        .args(["daemon", "start"])
        .ready(Ready::FileExists(paths.server_meta_file()))
        .spawn();
    let client = DaemonClient::connect(&env)
        .expect("a client for the running daemon")
        .with_timeout(Duration::from_secs(5));

    for _ in 0..WARMUP {
        client
            .call(&Request::get("/v1/status"))
            .await
            .expect("status");
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        client
            .call(&Request::get("/v1/status"))
            .await
            .expect("status");
        samples.push(started.elapsed());
    }
    samples.sort();
    let p50 = samples[SAMPLES / 2];
    let p95 = samples[(SAMPLES * 95).div_ceil(100) - 1];
    sandbox.note(
        "daemon status round trip",
        &format!("n={SAMPLES} p50={p50:?} p95={p95:?}"),
    );
    eprintln!("daemon status round trip: n={SAMPLES} p50={p50:?} p95={p95:?}");

    daemon.interrupt();
    assert_eq!(daemon.wait(Duration::from_secs(10)), Exit::Code(0));
    sandbox.finish();
}
