//! Provider failures pass through real command dispatch and outcome projection.
use awman::command::commands::api_server::event_bus::EventBus;
use awman::command::dispatch::{Dispatch, Engines};
use awman::data::session::{Session, SessionOpenOptions};
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use awman_test_support::fixtures::{ConfigTree, RepoBuilder};
use serde_json::json;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread")]
async fn provider_rejected_effort_keeps_tokens_and_fails_once_without_fallback() {
    let s = crate::support::sandbox("command::wait_failures::provider_rejected_effort_keeps_tokens_and_fails_once_without_fallback");
    ConfigTree::new().global(json!({"runtime":{"kind":"docker"},"agents":{"auth":{"mode":"none","refresh":{"enabled":false}},"flags":{"claude":["--configured-default"]}}})).write(&s, None);
    let repo = RepoBuilder::new(&s, "effort")
        .file("Dockerfile.dev", "FROM scratch\n")
        .file(".awman/Dockerfile.claude", "FROM scratch\n")
        .init();
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    docker.on(&["image", "inspect"]).exit(0);
    docker
        .on(&["run"])
        .stderr("provider: invalid effort value unsupported-effort\n")
        .exit(64);
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
    let session = Session::open_at_git_root(
        repo.path().into(),
        repo.path().into(),
        SessionOpenOptions {
            env: Some(env.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let args: Vec<String> = [
        "--agent",
        "claude",
        "--agent-flag",
        "--effort",
        "--agent-flag",
        "unsupported-effort",
        "review",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let bus = EventBus::new(32);
    let outcome = Dispatch::new(
        ApiDispatchFrontend::new("exec prompt", &args, bus.sender()).deferring_done(),
        Arc::new(tokio::sync::RwLock::new(session)),
        Engines::for_env(env).unwrap(),
    )
    .run_command(&["exec", "prompt"])
    .await
    .unwrap();
    assert_eq!(outcome.exit_code(), 64);
    assert_eq!(
        serde_json::to_value(&outcome).unwrap()["payload"]["exit_code"],
        64
    );
    let launches: Vec<_> = docker
        .calls()
        .into_iter()
        .filter(|c| c.argv.first().is_some_and(|v| v == "run"))
        .collect();
    assert_eq!(launches.len(), 1, "provider refusal must not retry");
    assert!(launches[0]
        .argv
        .windows(2)
        .any(|v| v == ["--effort", "unsupported-effort"]));
    assert!(!launches[0].argv.iter().any(|v| v == "--configured-default"));
    assert!(
        bus.wait_output_drained(std::time::Duration::from_secs(3))
            .await
    );
    s.finish();
}
