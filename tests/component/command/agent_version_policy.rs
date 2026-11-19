//! Version observations warn; only a confirmed OpenCode breaking major refuses.
use awman::command::commands::api_server::event_bus::EventBus;
use awman::command::dispatch::{Dispatch, Engines};
use awman::data::session::{Session, SessionOpenOptions};
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use awman_test_support::fixtures::{ConfigTree, RepoBuilder};
use serde_json::json;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread")]
async fn rebuilt_images_launch_with_new_unknown_missing_and_failed_version_metadata() {
    let s = crate::support::sandbox("command::agent_version_policy::rebuilt_images_launch_with_new_unknown_missing_and_failed_version_metadata");
    ConfigTree::new().global(json!({"runtime":{"kind":"docker"},"agents":{"auth":{"mode":"none","refresh":{"enabled":false}}}})).write(&s, None);
    let repo = RepoBuilder::new(&s, "version-policy")
        .file("Dockerfile.dev", "FROM scratch\n")
        .file(".awman/Dockerfile.opencode", "FROM scratch\n")
        .file(".awman/Dockerfile.opencode-v2", "FROM scratch\n")
        .file(".awman/Dockerfile.cursor", "FROM scratch\n")
        .init();
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    docker.on(&["image", "inspect"]).exit(0);
    docker.on(&["run"]).exit(0);
    let mut env = s.env().clone();
    env.set("AWMAN_TEST_DOCKER", "1");
    let env = awman::data::config::env::EnvSnapshot::from_full_env(
        env.iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect(),
    );
    for (agent, version, probe_exit, launches) in [
        ("opencode", "1.999.999", 0, true),
        ("opencode-v2", "2.999.999", 0, true),
        ("opencode", "nightly", 0, true),
        ("opencode-v2", "v2.3", 0, true),
        ("opencode", "", 0, true),
        ("opencode-v2", "", 127, true),
        ("cursor", "new-unrecognized-build", 0, true),
        ("opencode", "2.0.0", 0, false),
        ("opencode-v2", "1.0.0", 0, false),
    ] {
        docker
            .on(&["run", "--rm", "--network", "none"])
            .stdout(version)
            .exit(probe_exit);
        let mark = docker.calls().len();
        // A new runtime is a rebuilt/unknown image with no prior qualification cache.
        let session = Session::open_at_git_root(
            repo.path().into(),
            repo.path().into(),
            SessionOpenOptions {
                env: Some(env.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let args: Vec<String> = ["--agent", agent, "--yolo", "review"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let bus = EventBus::new(32);
        let result = Dispatch::new(
            ApiDispatchFrontend::new("exec prompt", &args, bus.sender()).deferring_done(),
            Arc::new(tokio::sync::RwLock::new(session)),
            Engines::for_env(env.clone()).unwrap(),
        )
        .run_command(&["exec", "prompt"])
        .await;
        let calls: Vec<_> = docker
            .calls()
            .into_iter()
            .skip(mark)
            .filter(|c| {
                c.argv.first().is_some_and(|v| v == "run")
                    && !c.argv.starts_with(&[
                        "run".into(),
                        "--rm".into(),
                        "--network".into(),
                        "none".into(),
                    ])
            })
            .collect();
        if launches {
            assert_eq!(result.unwrap().exit_code(), 0, "{agent}/{version}");
            assert_eq!(calls.len(), 1);
        } else {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expected opencode"));
            assert!(calls.is_empty());
        }
        assert!(
            bus.wait_output_drained(std::time::Duration::from_secs(3))
                .await
        );
    }
    s.finish();
}
