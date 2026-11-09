//! Each runtime keeps the invoking instance's executable search and endpoint.
#![cfg(unix)]

use awman::data::config::env::EnvSnapshot;
use awman::engine::{container::ContainerRuntime, git::GitEngine, host_cli};
use std::os::unix::fs::PermissionsExt;

#[test]
fn two_instances_use_their_own_docker_and_git_without_changing_process_path() {
    let root = tempfile::tempdir().unwrap();
    let mut runtimes = Vec::new();
    for (name, major) in [("first", 21), ("second", 22)] {
        let dir = root.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        let log = dir.join("docker.log");
        for (program, body) in [
            (
                "docker",
                "printf '%s:%s\\n' \"$DOCKER_HOST\" \"$1\" >> \"$PROBE_FILE\"\n".to_string(),
            ),
            ("git", format!("printf 'git version {major}.0.0\\n'\n")),
        ] {
            let executable = dir.join(program);
            std::fs::write(&executable, format!("#!/bin/sh\n{body}")).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let endpoint = format!("unix:///{name}.sock");
        let snapshot = EnvSnapshot::from_instance_env(
            vec![
                ("PATH".into(), dir.to_string_lossy().into_owned()),
                ("DOCKER_HOST".into(), endpoint.clone()),
                ("PROBE_FILE".into(), log.to_string_lossy().into_owned()),
            ],
            dir.clone(),
            vec!["awman".into()],
        );
        assert_eq!(host_cli::program(&snapshot, "docker"), dir.join("docker"));
        assert_eq!(host_cli::program(&snapshot, "git"), dir.join("git"));
        assert_eq!(
            GitEngine::with_env(snapshot.clone())
                .version_check()
                .unwrap()
                .major,
            major
        );
        let runtime = ContainerRuntime::docker_handle(snapshot.clone());
        assert!(std::sync::Arc::ptr_eq(
            &runtime,
            &ContainerRuntime::docker_handle(snapshot)
        ));
        runtimes.push((runtime, endpoint, log));
    }
    assert_ne!(runtimes[0].0.endpoint_key(), runtimes[1].0.endpoint_key());
    for (runtime, endpoint, log) in runtimes {
        assert!(runtime.is_available());
        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            format!("{endpoint}:info\n")
        );
    }
}
