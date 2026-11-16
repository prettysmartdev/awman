//! WI 0152: `/v1/status` tells a client whether `--read-only-workspace` can be
//! honoured, without disturbing the WSP lease flag (`lease_read_only`).
use super::support::Api;
use crate::support;
use awman::command::daemon_api::{capabilities, keys};
use awman::data::workspace::READ_ONLY_WORKSPACE_CAPABILITY;
use awman::engine::agent_runtime::{Capabilities, ImageAcquisition};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_advertises_read_only_workspace_once_on_a_runtime_that_enforces_it() {
    let s = support::sandbox(
        "daemon::read_only_status::status_advertises_read_only_workspace_once_on_a_runtime_that_enforces_it",
    );
    let server = Api::start(&s).await;
    let key = keys::keys_create(&server.api, "reader").unwrap();
    s.redact(&key.key);
    let (code, status) = server.get("/v1/status", Some(&key.key)).await;
    assert_eq!(code, 200, "{status}");
    let advertised = status["capabilities"].as_array().unwrap();
    assert_eq!(
        advertised
            .iter()
            .filter(|c| **c == json!(READ_ONLY_WORKSPACE_CAPABILITY))
            .count(),
        1,
        "{status}"
    );
    // The lease flag is a different capability, present only while a pull
    // worker reports it: this daemon runs none.
    assert!(
        !advertised.iter().any(|c| *c == json!("lease_read_only")),
        "{status}"
    );
    server.shutdown().await;
    s.finish();
}

fn caps(arbitrary_host_mounts: bool) -> Capabilities {
    Capabilities {
        arbitrary_host_mounts,
        kit_declarative: !arbitrary_host_mounts,
        has_image_store: arbitrary_host_mounts,
        image_acquisition: if arbitrary_host_mounts {
            ImageAcquisition::Build
        } else {
            ImageAcquisition::Kit
        },
        per_launch_vm_settings: false,
        reports_resource_stats: false,
    }
}

#[test]
fn the_capability_follows_the_runtime_and_leaves_lease_read_only_unchanged() {
    let s = support::sandbox(
        "daemon::read_only_status::the_capability_follows_the_runtime_and_leaves_lease_read_only_unchanged",
    );
    let pull = capabilities::PULL_CAPABILITIES;
    let container = capabilities::advertised_for_runtime(&caps(true), pull);
    let sandbox = capabilities::advertised_for_runtime(&caps(false), pull);
    let has = |list: &[String], name: &str| list.iter().any(|c| c == name);

    assert!(has(&container, READ_ONLY_WORKSPACE_CAPABILITY));
    assert!(
        !has(&sandbox, READ_ONLY_WORKSPACE_CAPABILITY),
        "sbx cannot enforce it, so it must not advertise it"
    );
    // `lease_read_only` is a pull-worker capability, independent of the
    // runtime-derived one, and is still advertised exactly once.
    for list in [&container, &sandbox] {
        assert_eq!(list.iter().filter(|c| *c == "lease_read_only").count(), 1);
    }
    assert!(pull.contains(&"lease_read_only"));
    // The runtime-blind projection never claims it.
    assert!(!has(
        &capabilities::advertised(true, &[]),
        READ_ONLY_WORKSPACE_CAPABILITY
    ));
    s.finish();
}
