//! Real CLI surface checks for the joined config/resource API.
use awman::command::commands::daemon::actions::{route, ACTIONS};
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn cli(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_awman"))
        .args(args)
        .current_dir(home)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("AWMAN_HOME", home.join("awman"))
        .env(
            "XDG_RUNTIME_DIR",
            std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default(),
        )
        .env("AWMAN_TEST_ISOLATION", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
#[test]
fn removed_flags_are_clap_unknown_arguments() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec![
            "remote",
            "session",
            "start",
            "--remote-addr",
            "https://host",
        ],
        vec!["remote", "session", "kill", "--api-key", "secret"],
        vec!["remote", "exec", "workflow", "--api-key", "secret"],
        vec!["exec", "workflow", "--max-concurrent", "2"],
        vec!["chat", "--launch-mode", "acp"],
        vec!["chat", "--non-interactive"],
        vec!["api", "start", "--port", "9999"],
        vec!["api", "start", "--workdirs", "/tmp"],
        vec!["api", "start", "--workdir-root", "/tmp"],
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("unexpected argument"), "{args:?}: {error}");
        assert!(!error.contains("has been removed"), "{error}");
    }
}
#[test]
fn set_reaches_effective_config_for_only_one_invocation() {
    let home = tempfile::tempdir().unwrap();
    success(cli(
        home.path(),
        &[
            "config",
            "set",
            "workflows.maxConcurrentSteps",
            "4",
            "--global",
        ],
    ));
    let get = |args: &[&str]| -> Value {
        serde_json::from_str(&success(cli(home.path(), args))).unwrap()
    };
    let overridden = get(&[
        "--set",
        "workflows.maxConcurrentSteps=2",
        "config",
        "get",
        "workflows.maxConcurrentSteps",
        "--json",
    ]);
    assert_eq!(overridden["effective_value"], "2");
    assert_eq!(overridden["source"], "set");
    let next = get(&["config", "get", "workflows.maxConcurrentSteps", "--json"]);
    assert_eq!(next["effective_value"], "4");
    assert_eq!(next["source"], "global");
}
#[test]
fn daemon_api_list_contains_every_client_action_and_route() {
    let home = tempfile::tempdir().unwrap();
    let output = success(cli(home.path(), &["daemon", "api", "--list"]));
    for action in ACTIONS {
        let (method, path) = route(action.op);
        assert!(
            output.lines().any(|line| line.starts_with(action.name)
                && line.ends_with(&format!("→ {method} {path}"))),
            "missing {} {path}",
            action.name
        );
    }
}
#[tokio::test]
async fn remote_set_is_sent_with_invocation_argv() {
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/worker/commands")).respond_with(ResponseTemplate::new(202).set_body_json(json!({"id":"cmd_22222222222242228222222222222222","session_id":"ses_11111111111141118111111111111111","status":"queued"}))).mount(&server).await;
    Mock::given(method("GET"))
        .and(path(
            "/v1/worker/commands/cmd_22222222222242228222222222222222",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status":"queued"})))
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("awman")).unwrap();
    std::fs::write(
        home.path().join("awman/config.json"),
        json!({"remote":{"targets":{"test":{"url":server.uri()}}}}).to_string(),
    )
    .unwrap();
    for settings in [true, false] {
        let mut args = vec!["--remote", "test"];
        if settings {
            args.extend(["--set", "workflows.maxConcurrentSteps=2"]);
        }
        args.extend([
            "remote",
            "exec",
            "prompt",
            "--session",
            "ses_11111111111141118111111111111111",
            "hi",
        ]);
        success(cli(home.path(), &args));
    }
    let requests = server.received_requests().await.unwrap();
    let commands: Vec<Value> = requests
        .iter()
        .filter(|r| r.method == "POST")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(commands.len(), 2);
    let first: Vec<String> = serde_json::from_value(commands[0]["args"].clone()).unwrap();
    assert!(
        first
            .windows(2)
            .any(|p| p == ["--set", "workflows.maxConcurrentSteps=2"]),
        "{first:?}"
    );
    let next: Vec<String> = serde_json::from_value(commands[1]["args"].clone()).unwrap();
    assert!(
        !next
            .iter()
            .any(|arg| arg.contains("--set") || arg.contains("maxConcurrentSteps")),
        "{next:?}"
    );
}

#[cfg(unix)]
#[path = "../helpers/api_process.rs"]
mod api_process;

#[cfg(unix)]
#[tokio::test]
async fn owner_socket_needs_no_bearer_and_config_set_reloads_the_daemon() {
    use std::io::Write;
    use std::time::{Duration, Instant};
    let fixture = api_process::ApiHome::new();
    let home = fixture.home();
    let data = home.join("awman");
    let path = format!(
        "{}:{}",
        home.parent().unwrap().join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let log_path = home.join("server.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_awman"))
        .args([
            "api",
            "start",
            "--set",
            "daemon.network.listen=127.0.0.1:0",
            "--dangerously-skip-tls",
        ])
        .current_dir(&home)
        .env_clear()
        .env("PATH", path)
        .env("HOME", &home)
        .env("AWMAN_HOME", &data)
        .env("AWMAN_TEST_ISOLATION", "1")
        .env("TMPDIR", std::env::temp_dir())
        .env("FAKE_DOCKER_CONTROL", fixture.control())
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    struct Stop(std::process::Child);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut server = Stop(child);
    let endpoint = awman::data::fs::daemon_paths::DaemonPaths::new(data.join("api"), "api_key")
        .local_endpoint();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !endpoint.exists() {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        assert!(Instant::now() < deadline, "local endpoint never appeared");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let status: Value =
        serde_json::from_str(&success(cli(&home, &["daemon", "api", "status", "--json"]))).unwrap();
    assert_eq!(status["pid"], server.0.id());
    let listeners = status["listeners"].as_array().unwrap();
    assert!(listeners.iter().any(|listener| listener["kind"] == "local"));
    assert!(listeners.iter().any(|listener| listener["kind"] == "ipc"));
    let edited = success(cli(
        &home,
        &["config", "set", "daemon.maxConcurrentJobs", "3", "--global"],
    ));
    assert!(edited.contains("applied to the running daemon"), "{edited}");
    let config: Value = serde_json::from_str(&success(cli(
        &home,
        &["daemon", "api", "--raw", "GET", "/v1/config", "--json"],
    )))
    .unwrap();
    assert_eq!(config["config"]["daemon"]["maxConcurrentJobs"], 3);
    assert_eq!(
        config["config"]["daemon"]["network"]["listen"], "127.0.0.1:0",
        "reload preserves startup --set"
    );
    let body_file = home.join("body.json");
    std::fs::write(&body_file, "{}").unwrap();
    for input in ["{}".to_string(), format!("@{}", body_file.display())] {
        success(cli(
            &home,
            &[
                "daemon",
                "api",
                "--raw",
                "POST",
                "/v1/config/reload",
                "--data",
                &input,
                "--json",
            ],
        ));
    }
    let mut pipe = Command::new(env!("CARGO_BIN_EXE_awman"))
        .args([
            "daemon",
            "api",
            "--raw",
            "POST",
            "/v1/config/reload",
            "--data",
            "-",
            "--json",
        ])
        .current_dir(&home)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("AWMAN_HOME", &data)
        .env("AWMAN_TEST_ISOLATION", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    pipe.stdin.take().unwrap().write_all(b"{}").unwrap();
    success(pipe.wait_with_output().unwrap());
    assert!(
        !data.join("api/api_key").exists(),
        "no plaintext bearer is stored"
    );
}
