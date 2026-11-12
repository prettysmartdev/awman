use crate::support;
use awman::data::ids::LeaseId;
use awman::engine::agent_runtime::{Capabilities, ImageAcquisition};
use awman::engine::worker::lease_workspace::{
    remove_orphaned_lease_dirs, LeaseRuntimeSupport, LeaseWorkspace,
};
#[test]
fn private_lease_body_is_never_in_clone_and_active_owner_cannot_be_reaped() {
    let sandbox = support::sandbox(
        "worker::security::private_lease_body_is_never_in_clone_and_active_owner_cannot_be_reaped",
    );
    let root = sandbox.work().join("leases");
    let id = LeaseId::new();
    let workspace = LeaseWorkspace::prepare(&root, &id).unwrap();
    let body = workspace
        .inline_work_item("work", "secret-work-item-body")
        .unwrap();
    assert!(!body.path().starts_with(workspace.clone_dir()));
    assert_eq!(
        std::fs::read_to_string(body.path()).unwrap(),
        "secret-work-item-body"
    );
    assert!(LeaseWorkspace::prepare(&root, &id).is_err());
    assert_eq!(remove_orphaned_lease_dirs(&root, &[]), 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(body.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(workspace.private_dir())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    let path = body.path().to_path_buf();
    drop(body);
    assert!(!path.exists());
    let clone = workspace.clone_dir().to_path_buf();
    workspace.remove().unwrap();
    assert!(!clone.exists());
    sandbox.finish();
}
#[test]
fn sandbox_refuses_read_only_lease_and_never_advertises_it() {
    let sandbox = support::sandbox(
        "worker::security::sandbox_refuses_read_only_lease_and_never_advertises_it",
    );
    let caps = Capabilities {
        arbitrary_host_mounts: false,
        kit_declarative: true,
        has_image_store: false,
        image_acquisition: ImageAcquisition::Kit,
        per_launch_vm_settings: false,
        reports_resource_stats: false,
    };
    let mut work = awman_wsp::conformance::work_item("read-only");
    work.exec.read_only = true;
    let support = LeaseRuntimeSupport::from_capabilities(&caps);
    assert!(support.check(&work, "docker-sbx").is_err());
    assert!(!support
        .features()
        .contains(&awman_wsp::features::LEASE_READ_ONLY));
    assert!(!caps.mounts_outputs(true));
    sandbox.finish();
}

/// Guards: e6a2d429, e6322996, f5b724fc. A running child cannot keep a
/// released owner lock alive through a descriptor inherited at launch.
#[test]
fn regression_e6a2d429_e6322996_f5b724fc_child_does_not_retain_lease_owner_lock() {
    use awman_test_support::{Exit, Ready};
    use std::time::Duration;
    let sandbox=support::sandbox("worker::security::regression_e6a2d429_e6322996_f5b724fc_child_does_not_retain_lease_owner_lock");
    let root = sandbox.work().join("worker");
    std::fs::create_dir_all(&root).unwrap();
    let id = LeaseId::new();
    let owner = awman::data::worker::paths::try_lease_owner(&root, &id)
        .unwrap()
        .unwrap();
    let fake = sandbox.install_fake(support::fake_cli_exe(), "held-child");
    let ready = sandbox.tmp().join("ready");
    fake.on(&["hold"]).touch(&ready).hold_until_released();
    let mut child = sandbox
        .command(fake.exe())
        .arg("hold")
        .ready(Ready::FileExists(ready))
        .spawn();
    fake.wait_until_held(Duration::from_secs(5));
    drop(owner);
    let reacquired = awman::data::worker::paths::try_lease_owner(&root, &id).unwrap();
    assert!(
        reacquired.is_some(),
        "an exec'd child retained its parent's lease lock"
    );
    fake.release();
    assert_eq!(child.wait(Duration::from_secs(5)), Exit::Code(0));
    drop(reacquired);
    sandbox.finish();
}
