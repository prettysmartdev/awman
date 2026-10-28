//! Per-launch builtin settings (WI 0126 A1): one process-wide backend, many
//! sessions, each launch sized and confined by its own session's settings.
use super::lifecycle_tests::{background, options, Rig};
use super::*;
use crate::data::config::builtin_network::{BuiltinNetworkConfig, NetworkMode};
use std::collections::HashMap;

/// Launch settings as a repo narrowing `{"mode":"public"}` to `network`
/// would produce.
fn narrowed(vcpus: u8, memory_mib: u32, network: &str) -> BuiltinLaunchSettings {
    let repo: BuiltinNetworkConfig = serde_json::from_str(network).unwrap();
    BuiltinLaunchSettings {
        vcpus,
        memory_mib,
        network: BuiltinNetworkConfig::resolve(None, Some(&repo)).unwrap(),
        repo_state_dir: None,
        ..BuiltinLaunchSettings::default()
    }
}

fn allowlist() -> BuiltinLaunchSettings {
    BuiltinLaunchSettings {
        root_disk_mib: 1024,
        mount_quota_mib: 256,
        ..narrowed(
            1,
            512,
            r#"{"mode":"allowlist","allow":["api.example.com"]}"#,
        )
    }
}

fn offline() -> BuiltinLaunchSettings {
    narrowed(3, 1024, r#"{"mode":"none"}"#)
}

fn with_launch(name: &str, launch: &BuiltinLaunchSettings) -> ResolvedContainerOptions {
    ResolvedContainerOptions {
        builtin_launch: Some(Box::new(launch.clone())),
        ..options(name)
    }
}

#[test]
fn plan_sizes_and_confines_each_launch_by_its_own_settings() {
    let rig = Rig::new("process");
    let (a, b) = (allowlist(), offline());
    assert_ne!(a.network.mode, b.network.mode);
    let plan_a = rig.backend.plan(with_launch("awman-a", &a)).unwrap();
    let plan_b = rig.backend.plan(with_launch("awman-b", &b)).unwrap();
    assert_eq!((plan_a.spec.vcpus, plan_a.spec.memory_mib), (1, 512));
    assert_eq!((plan_b.spec.vcpus, plan_b.spec.memory_mib), (3, 1024));
    assert_eq!(
        (plan_a.spec.root_disk_mib, plan_a.spec.mount_quota_mib),
        (1024, 256)
    );
    assert_eq!(
        (plan_b.spec.root_disk_mib, plan_b.spec.mount_quota_mib),
        (30720, 30720),
        "unset disk settings fall back to the runtime defaults"
    );
    assert_eq!(
        plan_a.spec.network.policy_json(),
        network::compile(&a.network).policy_json()
    );
    assert_eq!(
        plan_b.spec.network.policy_json(),
        network::compile(&b.network).policy_json()
    );
    assert!(plan_a.spec.network.enabled && plan_a.spec.network.strict);
    assert!(!plan_b.spec.network.enabled, "mode none has no device");

    // A launch's own CPU/memory limits still override the session defaults.
    let explicit = ResolvedContainerOptions {
        cpu: Some(crate::engine::container::options::CpuLimit(2.0)),
        memory: Some(crate::engine::container::options::MemoryLimit(2048)),
        ..with_launch("awman-explicit", &a)
    };
    let plan = rig.backend.plan(explicit).unwrap();
    assert_eq!((plan.spec.vcpus, plan.spec.memory_mib), (2, 2048));
    assert_eq!(
        plan.spec.network.policy_json(),
        network::compile(&a.network).policy_json(),
        "explicit limits never change the network policy"
    );
}

#[test]
fn start_background_sizes_and_confines_each_launch_by_its_own_settings() {
    let rig = Rig::new("process");
    let work = tempfile::tempdir_in("/tmp").unwrap();
    let env = HashMap::new();
    for launch in [allowlist(), offline()] {
        let id = rig
            .backend
            .start_background(&BackgroundLaunchRequest {
                secret_env: &[],
                builtin_launch: Some(&launch),
                ..background("agent:latest", work.path(), &env, &[])
            })
            .unwrap();
        let spec = rig.driver.spec(&id);
        assert_eq!(
            (spec.vcpus, spec.memory_mib),
            (launch.vcpus, launch.memory_mib)
        );
        assert_eq!(
            (spec.root_disk_mib, spec.mount_quota_mib),
            (launch.root_disk_mib, launch.mount_quota_mib)
        );
        assert_eq!(
            spec.network.policy_json(),
            network::compile(&launch.network).policy_json()
        );
        assert_eq!(
            spec.network.enabled,
            launch.network.mode != NetworkMode::None
        );
    }
}

#[test]
fn a_launch_without_session_settings_fails_closed_on_every_path() {
    let rig = Rig::new("process");
    let bare = ResolvedContainerOptions {
        builtin_launch: None,
        ..options("awman-bare")
    };
    assert!(matches!(
        rig.backend.build(bare).err().unwrap(),
        EngineError::MissingRequiredOption(message) if message.contains("builtin launch settings")
    ));
    let work = tempfile::tempdir_in("/tmp").unwrap();
    let env = HashMap::new();
    let error = rig
        .backend
        .start_background(&BackgroundLaunchRequest {
            secret_env: &[],
            builtin_launch: None,
            ..background("agent:latest", work.path(), &env, &[])
        })
        .unwrap_err();
    assert!(
        matches!(error, EngineError::MissingRequiredOption(_)),
        "{error:?}"
    );
    assert_eq!(rig.driver.creates(), 0, "nothing was created");
}

#[test]
fn a_repo_state_dir_other_than_the_process_one_fails_that_launch_only() {
    let rig = Rig::new("process");
    let process_dir = rig.backend.settings.state_dir.clone();
    let foreign = BuiltinLaunchSettings {
        repo_state_dir: Some("/somewhere/else".into()),
        ..BuiltinLaunchSettings::default()
    };
    match rig
        .backend
        .build(with_launch("awman-foreign", &foreign))
        .err()
        .unwrap()
    {
        EngineError::BuiltinStateDirMismatch { process, session } => {
            assert_eq!(process, process_dir);
            assert_eq!(session, std::path::PathBuf::from("/somewhere/else"));
        }
        other => panic!("expected BuiltinStateDirMismatch, got {other:?}"),
    }
    let message = EngineError::BuiltinStateDirMismatch {
        process: process_dir.clone(),
        session: "/somewhere/else".into(),
    }
    .to_string();
    assert!(message.contains("/somewhere/else"), "{message}");
    assert!(
        message.contains(&process_dir.display().to_string()),
        "{message}"
    );

    let work = tempfile::tempdir_in("/tmp").unwrap();
    let env = HashMap::new();
    assert!(matches!(
        rig.backend.start_background(&BackgroundLaunchRequest {
            secret_env: &[],
            builtin_launch: Some(&foreign),
            ..background("agent:latest", work.path(), &env, &[])
        }),
        Err(EngineError::BuiltinStateDirMismatch { .. })
    ));
    assert_eq!(rig.driver.creates(), 0);

    // The same directory, spelled as configured or through a symlinked
    // parent, is accepted; so is a session that names none.
    let alias = tempfile::tempdir_in("/tmp").unwrap();
    let link = alias.path().join("link");
    std::os::unix::fs::symlink(process_dir.parent().unwrap(), &link).unwrap();
    for same in [
        process_dir.clone(),
        link.join(process_dir.file_name().unwrap()),
    ] {
        let equal = BuiltinLaunchSettings {
            repo_state_dir: Some(same.clone()),
            ..BuiltinLaunchSettings::default()
        };
        assert!(
            rig.backend.plan(with_launch("awman-same", &equal)).is_ok(),
            "{} names the process's directory",
            same.display()
        );
    }
    assert!(rig.backend.plan(options("awman-other-session")).is_ok());
}
