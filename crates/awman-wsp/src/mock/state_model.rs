//! Exhaustive short histories against a reference lease state machine.
use super::*;
use chrono::TimeZone;

#[test]
fn lease_state_model_exclusive_claim_expiry_completion_and_stale_epoch() {
    // 5^5 histories, including expiry at the boundary, retry, release and result.
    for seed in 0..3125 {
        let server = MockWorkServer::new();
        let mut now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let worker = server
            .register(
                RegisterRequest {
                    name: "worker".into(),
                    labels: BTreeMap::new(),
                    capacity: 1,
                    features: vec![],
                },
                BTreeMap::new(),
            )
            .worker_id;
        server
            .enqueue(crate::conformance::work_item("item"), now)
            .unwrap();
        let mut active: Option<(Lease, DateTime<Utc>)> = None;
        let mut stale: Vec<Lease> = vec![];
        let mut complete = false;
        let mut last_epoch = 0;
        let mut history = seed;
        for step in 0..5 {
            let action = history % 5;
            history /= 5;
            if active.as_ref().is_some_and(|(_, until)| *until <= now) {
                stale.push(active.take().unwrap().0);
            }
            match action {
                0 => {
                    let got = server
                        .claim(&worker, None, now, Duration::seconds(10))
                        .unwrap();
                    assert_eq!(
                        got.is_some(),
                        active.is_none() && !complete,
                        "seed {seed}, step {step}"
                    );
                    if let Some(lease) = got {
                        assert!(lease.epoch > last_epoch, "seed {seed}");
                        last_epoch = lease.epoch;
                        active = Some((lease, now + Duration::seconds(10)));
                    }
                }
                1 => now += Duration::seconds(10),
                2 => {
                    if let Some((lease, until)) = &mut active {
                        let beat = server
                            .heartbeat(
                                &lease.id,
                                lease.epoch,
                                now,
                                Duration::seconds(10),
                                &HeartbeatRequest {
                                    state: HeartbeatState::Running,
                                    step: None,
                                    progress: None,
                                },
                            )
                            .unwrap();
                        *until = now + Duration::seconds(10);
                        assert_eq!(beat.expires_at, *until);
                    }
                }
                3 => {
                    if let Some((lease, _)) = active.take() {
                        server
                            .result(
                                &lease.id,
                                lease.epoch,
                                now,
                                LeaseResult {
                                    result: serde_json::json!({}),
                                    outputs: None,
                                    outputs_mounted: false,
                                    duration_ms: 0,
                                },
                            )
                            .unwrap();
                        complete = true;
                        stale.push(lease);
                    }
                }
                4 => {
                    if let Some((lease, _)) = active.take() {
                        server
                            .release(
                                &lease.id,
                                lease.epoch,
                                now,
                                ReleaseRequest {
                                    reason: ReleaseReason::Shutdown,
                                    message: None,
                                    result: None,
                                },
                            )
                            .unwrap();
                        stale.push(lease);
                    }
                }
                _ => unreachable!(),
            }
            for old in &stale {
                assert_eq!(
                    server
                        .events(&old.id, old.epoch, now, vec![])
                        .unwrap_err()
                        .code,
                    "lease_lost",
                    "seed {seed}"
                );
            }
        }
    }
}
