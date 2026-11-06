//! Real CLI surface checks for the joined config/resource API.
use awman::command::commands::daemon::actions::ACTIONS;
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
        vec!["daemon", "start", "--port", "9999"],
        vec!["daemon", "start", "--refresh-key"],
        vec!["daemon", "start", "--dangerously-skip-auth"],
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
fn the_api_group_and_squad_lifecycle_are_gone() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec!["api", "start"],
        vec!["api", "status"],
        vec!["squad", "start"],
        vec!["squad", "stop"],
        vec!["squad", "status"],
        vec!["squad", "logs"],
        vec!["daemon", "set", "x"],
        vec!["config", "prerequisite-grants", "list"],
    ] {
        let output = cli(home.path(), &args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}
fn failure(output: Output) -> String {
    assert!(
        !output.status.success(),
        "expected failure; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stderr).unwrap()
}
#[test]
fn daemon_api_list_contains_every_client_action_and_route() {
    let home = tempfile::tempdir().unwrap();
    let output = success(cli(home.path(), &["daemon", "api", "--list"]));
    for action in ACTIONS {
        assert!(
            output.lines().any(|line| line.starts_with(action.name)
                && line.contains(&format!("→ {} {}", action.method, action.path))),
            "missing {} {}:\n{output}",
            action.name,
            action.path
        );
    }
    assert!(output.contains("keys.create name=…"), "{output}");
}
#[test]
fn without_a_daemon_status_says_so_and_requests_are_refused() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(
        success(cli(home.path(), &["daemon", "status"])).trim(),
        "awman daemon is not running"
    );
    let json: Value =
        serde_json::from_str(&success(cli(home.path(), &["daemon", "status", "--json"]))).unwrap();
    assert_eq!(json, json!({"running": false}));
    for args in [
        vec!["daemon", "api", "--raw", "GET", "/v1/status"],
        vec!["daemon", "api", "features.list"],
        vec!["daemon", "stop"],
        vec!["squad", "list"],
    ] {
        let error = failure(cli(home.path(), &args));
        assert!(error.contains("awman daemon start"), "{args:?}: {error}");
    }
}
#[test]
fn invalid_actions_are_refused_locally_without_a_daemon() {
    let home = tempfile::tempdir().unwrap();
    for (args, expected) in [
        (vec!["daemon", "api", "nope.list"], "unknown action"),
        (
            vec!["daemon", "api", "features.get", "feature=workers"],
            "invalid feature",
        ),
        (
            vec!["daemon", "api", "keys.create", "name=Bad Name"],
            "invalid name",
        ),
        (
            vec!["daemon", "api", "keys.list", "colour=red"],
            "unknown argument",
        ),
        (
            vec!["daemon", "api", "jobs.cancel", "id=task_x"],
            "jobs.cancel needs",
        ),
        (
            vec!["daemon", "api", "--raw", "FETCH", "/v1/status"],
            "--raw method",
        ),
        (
            vec!["daemon", "api", "--raw", "GET", "/v1/x", "--data", "{bad"],
            "invalid --data JSON",
        ),
    ] {
        let error = failure(cli(home.path(), &args));
        assert!(error.contains(expected), "{args:?}: {error}");
        assert!(!error.contains("awman daemon start"), "{args:?}: {error}");
    }
}
#[test]
fn daemon_start_refuses_without_an_enabled_feature() {
    let home = tempfile::tempdir().unwrap();
    let error = failure(cli(home.path(), &["daemon", "start"]));
    assert!(error.contains("no daemon feature is enabled"), "{error}");
    let error = failure(cli(home.path(), &["daemon", "start", "--background"]));
    assert!(error.contains("no daemon feature is enabled"), "{error}");
}
#[test]
fn config_set_without_a_daemon_persists_for_the_next_start() {
    let home = tempfile::tempdir().unwrap();
    for key in ["daemon.maxConcurrentJobs", "daemon.network.listen"] {
        let value = if key.ends_with("listen") {
            "127.0.0.1:9988"
        } else {
            "3"
        };
        let out = success(cli(home.path(), &["config", "set", key, value, "--global"]));
        assert!(out.contains("takes effect on next start"), "{key}: {out}");
    }
    let config = std::fs::read_to_string(home.path().join("awman/config.json")).unwrap();
    let error = failure(cli(
        home.path(),
        &["config", "set", "daemon.network.listen", "nope", "--global"],
    ));
    assert!(!error.is_empty());
    assert_eq!(
        std::fs::read_to_string(home.path().join("awman/config.json")).unwrap(),
        config,
        "an invalid value is not persisted"
    );
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

/// One foreground daemon (engine feature only, so no container work), driven
/// through every CLI verb over its owner-only local socket.
#[cfg(unix)]
#[tokio::test]
async fn the_cli_drives_a_running_daemon_over_its_local_socket() {
    use std::io::Write;
    use std::time::{Duration, Instant};
    let fixture = api_process::ApiHome::new();
    let home = fixture.home();
    let data = home.join("awman");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("config.json"),
        json!({"daemon":{"features":{"engine":{"enabled":true}}}}).to_string(),
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        home.parent().unwrap().join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_awman"))
            .args(args)
            .current_dir(&home)
            .env_clear()
            .env("PATH", &path)
            .env("HOME", &home)
            .env("AWMAN_HOME", &data)
            .env("AWMAN_TEST_ISOLATION", "1")
            .env("FAKE_DOCKER_CONTROL", fixture.control())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let log_path = home.join("daemon.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_awman"))
        .args(["daemon", "start"])
        .current_dir(&home)
        .env_clear()
        .env("PATH", &path)
        .env("HOME", &home)
        .env("AWMAN_HOME", &data)
        .env("AWMAN_TEST_ISOLATION", "1")
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
    let mut daemon = Stop(child);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = run(&["daemon", "status", "--json"]);
        if status.status.success()
            && serde_json::from_slice::<Value>(&status.stdout).unwrap()["running"] == true
        {
            break;
        }
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        assert!(Instant::now() < deadline, "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // A second start is refused while this one runs.
    let error = failure(run(&["daemon", "start"]));
    assert!(error.contains("already running"), "{error}");

    let text = success(run(&["daemon", "status"]));
    assert!(
        text.starts_with(&format!("awman daemon  pid {}", daemon.0.id())),
        "{text}"
    );
    assert!(text.contains("engine  on"), "{text}");
    assert!(text.contains("squad   off"), "{text}");
    assert!(text.contains("local    unix socket"), "{text}");
    assert!(text.contains("network  off"), "{text}");
    let status: Value =
        serde_json::from_str(&success(run(&["daemon", "status", "--json"]))).unwrap();
    assert_eq!(status["pid"], daemon.0.id());
    assert_eq!(status["features"]["engine"]["state"], "on");

    // Actions equal their --raw forms.
    let features: Value =
        serde_json::from_str(&success(run(&["daemon", "api", "features.list", "--json"]))).unwrap();
    let raw: Value = serde_json::from_str(&success(run(&[
        "daemon",
        "api",
        "--raw",
        "GET",
        "/v1/features",
        "--json",
    ])))
    .unwrap();
    assert_eq!(
        features["items"].as_array().map(Vec::len),
        raw["items"].as_array().map(Vec::len)
    );
    let engine: Value = serde_json::from_str(&success(run(&[
        "daemon",
        "api",
        "features.get",
        "feature=engine",
        "--json",
    ])))
    .unwrap();
    assert_eq!(engine["state"], "on");

    // keys.*: the key is shown once, lists never carry it, delete revokes.
    let created: Value = serde_json::from_str(&success(run(&[
        "daemon",
        "api",
        "keys.create",
        "name=ci",
        "--json",
    ])))
    .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let key = created["key"].as_str().unwrap().to_string();
    assert!(id.starts_with("key_"), "{created}");
    let listed = success(run(&["daemon", "api", "keys.list", "--json"]));
    assert!(listed.contains(&id) && !listed.contains(&key), "{listed}");
    let body_file = home.join("key.json");
    std::fs::write(&body_file, r#"{"name":"ci"}"#).unwrap();
    let error = failure(run(&[
        "daemon",
        "api",
        "--raw",
        "POST",
        "/v1/keys",
        "--data",
        &format!("@{}", body_file.display()),
    ]));
    assert!(
        error.contains("409"),
        "a duplicate name is a conflict: {error}"
    );
    success(run(&["daemon", "api", "keys.delete", &format!("id={id}")]));
    let listed = success(run(&["daemon", "api", "keys.list", "--json"]));
    assert!(!listed.contains(&id), "{listed}");

    // --raw: 4xx exits nonzero with the body on stderr; --data - reads stdin.
    let error = failure(run(&["daemon", "api", "--raw", "GET", "/v1/nope"]));
    assert!(
        error.contains("404") && error.contains("not_found"),
        "{error}"
    );
    let mut pipe = Command::new(env!("CARGO_BIN_EXE_awman"))
        .args([
            "daemon", "api", "--raw", "POST", "/v1/keys", "--data", "-", "--json",
        ])
        .current_dir(&home)
        .env_clear()
        .env("PATH", &path)
        .env("HOME", &home)
        .env("AWMAN_HOME", &data)
        .env("AWMAN_TEST_ISOLATION", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    pipe.stdin
        .take()
        .unwrap()
        .write_all(br#"{"name":"from-stdin"}"#)
        .unwrap();
    let created: Value = serde_json::from_str(&success(pipe.wait_with_output().unwrap())).unwrap();
    assert_eq!(created["name"], "from-stdin");

    // config set: live keys apply, non-live keys ask for a restart.
    let out = success(run(&[
        "config",
        "set",
        "daemon.maxConcurrentJobs",
        "3",
        "--global",
    ]));
    assert!(out.contains("applied to the running daemon"), "{out}");
    let config: Value = serde_json::from_str(&success(run(&[
        "daemon",
        "api",
        "--raw",
        "GET",
        "/v1/config",
        "--json",
    ])))
    .unwrap();
    assert_eq!(config["config"]["daemon"]["maxConcurrentJobs"], 3);
    let out = success(run(&[
        "config",
        "set",
        "daemon.network.listen",
        "127.0.0.1:9911",
        "--global",
    ]));
    assert!(out.contains("restart the daemon to apply"), "{out}");

    // logs prints the daemon's own log; stop ends it.
    let out = success(run(&["daemon", "stop"]));
    assert!(out.contains("stopped"), "{out}");
    let deadline = Instant::now() + Duration::from_secs(40);
    while daemon.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "daemon did not exit after stop");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        success(run(&["daemon", "status"])).trim(),
        "awman daemon is not running"
    );
}
