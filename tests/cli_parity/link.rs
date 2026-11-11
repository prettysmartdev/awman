//! One catalogue command, two authorities, with no worker/web alias.
use std::{
    path::Path,
    process::{Command, Output, Stdio},
};
fn cli(home: &Path, args: &[&str]) -> Output {
    Command::new(awman_test_support::cargo_bin!("awman"))
        .args(args)
        .current_dir(home)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("AWMAN_HOME", home.join("data"))
        .env("AWMAN_TEST_ISOLATION", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}
#[test]
fn link_help_classification_and_removed_worker_spelling() {
    let home = tempfile::tempdir().unwrap();
    let out = cli(home.path(), &["link", "--help"]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    for word in ["target", "--name", "--label", "--token"] {
        assert!(help.contains(word), "{help}");
    }
    for args in [
        vec!["link", "bad"],
        vec!["link", "web-K7PM-3Q9X", "--name", "bad"],
        vec!["link", "web-K7PM-3Q9X", "--yes"],
        vec!["worker", "link", "https://server.example"],
        vec!["web", "link"],
    ] {
        let out = cli(home.path(), &args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = cli(home.path(), &["link", "web-K7PM-3Q9X"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("interactive terminal"));
    let cat = awman::command::dispatch::catalogue::CommandCatalogue::get();
    assert!(cat.lookup(&["link"]).is_some());
    assert!(cat.lookup(&["worker", "link"]).is_none());
}
#[test]
fn static_server_token_writes_only_work_server_credentials() {
    let home = tempfile::tempdir().unwrap();
    let out = cli(
        home.path(),
        &[
            "link",
            "https://example.com",
            "--name",
            "ci",
            "--label",
            "os=linux",
            "--token",
            "server-refresh",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stored = home.path().join("data/data/work_servers/ci.json");
    let body: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(stored).unwrap()).unwrap();
    assert_eq!(body["refresh_token"], "server-refresh");
    assert!(!home
        .path()
        .join("data/data/daemon/browser_grants.json")
        .exists());
    let config: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("data/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(config["workServers"]["ci"]["labels"]["os"], "linux");
    let out = cli(
        home.path(),
        &[
            "link",
            "https://example.com",
            "--token",
            "awb_browser_grant",
        ],
    );
    assert!(!out.status.success());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_server_device_flow_and_static_token() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/link")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"user_code":"K7PM-3Q9X","verification_uri":format!("{}/verify",server.uri()),"link_id":"remote-link","interval_ms":10,"expires_in":30}))).mount(&server).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    Mock::given(method("POST"))
        .and(path("/v1/link/remote-link/token"))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(if count.fetch_add(1, Ordering::SeqCst) == 0 {
                serde_json::json!({"error":"slow_down"})
            } else {
                serde_json::json!({"refresh_token":"remote-refresh"})
            })
        })
        .mount(&server)
        .await;
    let url = server.uri();
    let home = tempfile::tempdir().unwrap();
    let h = home.path().to_path_buf();
    let start = std::time::Instant::now();
    let output = tokio::task::spawn_blocking(move || cli(&h, &["link", &url, "--name", "remote"]))
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(start.elapsed() >= std::time::Duration::from_secs(5));
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.contains("K7PM-3Q9X"));
    assert!(output.contains("/verify"));
    assert!(!output.contains("remote-refresh"));
    assert!(!home
        .path()
        .join("data/data/daemon/browser_grants.json")
        .exists());
}

#[cfg(unix)]
use super::api_process;
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_code_requires_a_tty_and_shows_origin_listener_scope() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use serde_json::{json, Value};
    use std::{
        io::{Read, Write},
        sync::mpsc,
        time::Duration,
    };
    let home = api_process::ApiHome::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    home.write_config(json!({"daemon":{"features":{"engine":{"enabled":true}},"network":{"listeners":{"loopback":{"enabled":true,"listen":addr.to_string()}}}}}));
    let server = home.start().await;
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    for approve in [true, false] {
        let link: Value = http
            .post(format!("http://{addr}/v1/browser-links"))
            .header("Origin", "https://awman.prettysmart.dev")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let code = link["code"].as_str().unwrap();
        let command = home.command(&[], &["link", code]);
        let mut cb = CommandBuilder::new(command.get_program());
        cb.args(command.get_args());
        cb.env_clear();
        for (k, v) in command.get_envs() {
            if let Some(v) = v {
                cb.env(k, v);
            }
        }
        cb.cwd(home.home());
        let output = tokio::task::spawn_blocking(move || {
            let pair = native_pty_system()
                .openpty(PtySize {
                    rows: 40,
                    cols: 120,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            let mut reader = pair.master.try_clone_reader().unwrap();
            let mut writer = pair.master.take_writer().unwrap();
            let mut child = pair.slave.spawn_command(cb).unwrap();
            drop(pair.slave);
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let mut b = [0; 2048];
                loop {
                    match reader.read(&mut b) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send(b[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            let mut text = String::new();
            loop {
                let bytes = rx
                    .recv_timeout(Duration::from_secs(20))
                    .expect("approval prompt");
                text.push_str(&String::from_utf8_lossy(&bytes));
                if text.contains("Approve this browser?") {
                    break;
                }
            }
            writer
                .write_all(if approve { b"y\n" } else { b"n\n" })
                .unwrap();
            let status = child.wait().unwrap();
            assert!(status.success(), "{text}");
            for bytes in rx {
                text.push_str(&String::from_utf8_lossy(&bytes));
            }
            text
        })
        .await
        .unwrap();
        assert!(output.contains("https://awman.prettysmart.dev"), "{output}");
        assert!(
            output.contains(&format!("loopback ({addr}, loopback_http)")),
            "{output}"
        );
        assert!(output.contains("scope:    engine"));
        assert!(output.contains(if approve { "Approved." } else { "Denied." }));
        let polled: Value = http
            .post(format!(
                "http://{addr}/v1/browser-links/{}/poll",
                link["id"].as_str().unwrap()
            ))
            .header("Origin", "https://awman.prettysmart.dev")
            .json(&json!({"polling_secret":link["polling_secret"]}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            polled["status"],
            if approve { "approved" } else { "denied" }
        );
    }
    assert!(!home
        .home()
        .join(".local/share/awman/data/work_servers")
        .exists());
    let config: Value = serde_json::from_str(
        &std::fs::read_to_string(home.home().join(".config/awman/config.json")).unwrap(),
    )
    .unwrap();
    assert!(config.get("workServers").is_none());
    drop(server);
}
