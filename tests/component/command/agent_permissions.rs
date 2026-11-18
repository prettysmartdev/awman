//! Explicit permission authorization survives API profile defaults.
use std::sync::Arc;

use awman::command::commands::api_server::event_bus::EventBus;
use awman::command::dispatch::{Dispatch, Engines};
use awman::data::session::{Session, SessionOpenOptions};
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use awman_test_support::fixtures::{ConfigTree, RepoBuilder};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn api_cursor_force_requires_explicit_yolo_and_honors_opt_out() {
    let s = crate::support::sandbox(
        "command::agent_permissions::api_cursor_force_requires_explicit_yolo_and_honors_opt_out",
    );
    ConfigTree::new()
        .global(json!({"runtime":{"kind":"docker"},"agents":{"auth":{"mode":"none","refresh":{"enabled":false}}}}))
        .write(&s, None);
    let repo = RepoBuilder::new(&s, "api-permissions")
        .file("Dockerfile.dev", "FROM scratch\n")
        .file(".awman/Dockerfile.cursor", "FROM scratch\n")
        .init();
    let docker = s.install_fake(crate::support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    docker.on(&["image", "inspect"]).exit(0);
    docker
        .on(&["image", "inspect", "--format", "{{.Id}}"])
        .stdout("sha256:fixture\n");
    const KEY: &str = "synthetic-cursor-api-publication-canary";
    docker
        .on(&["run"])
        .stdout(&format!(
            "{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"summary {KEY}\"}}\n"
        ))
        .stderr(&format!("diagnostic {KEY}\n"))
        .exit(0);
    docker
        .on(&["run", "--rm", "--network", "none"])
        .stdout("2026.11.17-f532c45\n");
    let mut env = s.env().clone();
    env.set("AWMAN_TEST_DOCKER", "1");
    env.set("CURSOR_API_KEY", KEY);
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
    for permission in [None, Some("--yolo"), Some("--yolo=false")] {
        let session = Session::open_at_git_root(
            repo.path().into(),
            repo.path().into(),
            SessionOpenOptions {
                env: Some(env.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let mut args = vec![
            "--agent".into(),
            "cursor".into(),
            "--overlay".into(),
            "env(CURSOR_API_KEY)".into(),
        ];
        if let Some(permission) = permission {
            args.push(permission.into());
        }
        args.push("review".into());
        let bus = EventBus::new(128);
        let mut log = bus.subscribe_log();
        // Match ApiFrontendFactory: completion belongs to the caller after
        // the asynchronous output drains, not to the frontend's Drop.
        let frontend =
            ApiDispatchFrontend::new("exec prompt", &args, bus.sender()).deferring_done();
        let mark = docker.calls().len();
        let result = Dispatch::new(
            frontend,
            Arc::new(tokio::sync::RwLock::new(session)),
            Engines::for_env(env.clone()).unwrap(),
        )
        .with_output_dir(s.tmp().join("command-output"))
        .run_command(&["exec", "prompt"])
        .await;
        let calls: Vec<_> = docker
            .calls()
            .into_iter()
            .skip(mark)
            .filter(|c| {
                c.argv.first().is_some_and(|a| a == "run")
                    && !c.argv.starts_with(&[
                        "run".into(),
                        "--rm".into(),
                        "--network".into(),
                        "none".into(),
                    ])
            })
            .collect();
        match permission {
            None => {
                assert!(result.unwrap_err().to_string().contains("explicit --yolo"));
                assert!(calls.is_empty());
            }
            Some(flag) => {
                result.unwrap();
                assert!(
                    bus.wait_output_drained(std::time::Duration::from_secs(3))
                        .await
                );
                bus.sender()
                    .emit(awman::data::execution_event::EventPayload::Done);
                let paths =
                    awman::data::fs::api_paths::ApiPaths::from_root(s.tmp().join("api-logs"));
                let command = if flag == "--yolo" {
                    "forced"
                } else {
                    "ordinary"
                };
                paths.prepare_command_dir("fixture", command).unwrap();
                let mut writer = awman::data::fs::api_command_log::CommandLogWriter::open_append(
                    &paths, "fixture", command,
                )
                .await
                .unwrap();
                while let Ok(event) = log.try_recv() {
                    assert!(!serde_json::to_string(&event).unwrap().contains(KEY));
                    writer.write_event(&event).await;
                }
                writer.flush().await;
                for path in [
                    paths.command_log_path("fixture", command),
                    paths.command_events_log_path("fixture", command),
                ] {
                    let text = std::fs::read_to_string(path).unwrap();
                    assert!(!text.contains(KEY), "{text}");
                    assert!(text.contains("summary ***"), "{text}");
                }
                assert_eq!(calls.len(), 1);
                assert!(calls[0]
                    .argv
                    .windows(2)
                    .any(|a| a == ["--output-format", "stream-json"]));
                assert!(!calls[0].argv.iter().any(|a| a.contains(KEY)));
                assert_eq!(
                    calls[0].argv.iter().any(|a| a == "--force"),
                    flag == "--yolo"
                );
            }
        }
    }
    s.finish();
}

#[cfg(unix)]
#[test]
fn version_probe_preserves_failure_even_when_the_binary_prints_the_pin() {
    use awman::engine::container::agent_version::AGENT_VERSION_PROBE_SCRIPT;
    let s = crate::support::sandbox(
        "command::agent_permissions::version_probe_preserves_failure_even_when_the_binary_prints_the_pin",
    );
    let fake = s.install_fake(crate::support::fake_cli_exe(), "version-fixture");
    for code in [1, 0] {
        fake.on(&["--disable-auto-update", "--version"])
            .stdout("2026.11.17-f532c45\nsecond line\n")
            .exit(code);
        let output = s
            .command("sh")
            .args(["-c", AGENT_VERSION_PROBE_SCRIPT, "sh"])
            .arg(s.bin().join("version-fixture"))
            .arg("--disable-auto-update")
            .run();
        assert_eq!(output.exit, awman_test_support::Exit::Code(code));
        if code == 0 {
            assert_eq!(output.stdout_text(), "2026.11.17-f532c45\n");
        }
    }
    s.finish();
}
