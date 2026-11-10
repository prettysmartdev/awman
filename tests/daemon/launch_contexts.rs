//! Owner CLI capture, browser visibility and private metadata persistence.
use crate::{api_process::ApiHome, support::*};
use serde_json::{json, Value};
use std::path::Path;
fn capture(home: &ApiHome, root: &Path, args: &[&str]) -> std::process::Output {
    home.command(&[("CONTEXT_SECRET", "planted-context-secret")], args)
        .current_dir(root)
        .output()
        .unwrap()
}
fn value(out: std::process::Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_context_owner_capture_refresh_default_and_restart() {
    let home = ApiHome::new();
    home.write_config(json!({"daemon":{"features":{"engine":{"enabled":true}}}}));
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join("sub")).unwrap();
    let mut server = home.start().await;
    let c = value(capture(
        &home,
        repo.path(),
        &[
            "daemon",
            "api",
            "launch_contexts.create",
            "default=true",
            "--json",
        ],
    ));
    let id = c["id"].as_str().unwrap();
    assert_eq!(c["root"], json!(repo.path().canonicalize().unwrap()));
    assert!(c.get("env").is_none());
    let list = value(home.cli(&["daemon", "api", "launch_contexts.list", "--json"]));
    assert_eq!(list["default_id"], id);
    assert_eq!(list["items"][0]["env_state"], "live");
    assert!(!list.to_string().contains("planted-context-secret"));
    let second = value(capture(
        &home,
        &repo.path().join("sub"),
        &[
            "daemon",
            "api",
            "launch_contexts.create",
            "label=sub",
            "--json",
        ],
    ));
    let duplicate = capture(
        &home,
        repo.path(),
        &["daemon", "api", "launch_contexts.create", "--json"],
    );
    assert!(!duplicate.status.success());
    let refresh = format!("id={id}");
    assert!(!capture(
        &home,
        &repo.path().join("sub"),
        &["daemon", "api", "launch_contexts.refresh", &refresh]
    )
    .status
    .success());
    assert!(capture(
        &home,
        repo.path(),
        &["daemon", "api", "launch_contexts.refresh", &refresh]
    )
    .status
    .success());
    let file = home
        .home()
        .join(".local/share/awman/data/daemon/launch_contexts.json");
    let stored = std::fs::read_to_string(&file).unwrap();
    assert!(!stored.contains("CONTEXT_SECRET"));
    assert!(!stored.contains("planted-context-secret"));
    server.kill();
    drop(server);
    let server = home.start().await;
    let list = value(home.cli(&["daemon", "api", "launch_contexts.list", "--json"]));
    assert!(list["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["env_state"] == "stale"));
    assert!(home
        .cli(&["daemon", "api", "launch_contexts.delete", &refresh])
        .status
        .success());
    let list = value(home.cli(&["daemon", "api", "launch_contexts.list", "--json"]));
    assert_eq!(list["default_id"], second["id"]);
    let (status, _) = http(
        &server,
        reqwest::Method::POST,
        "/v1/engine/launch_contexts",
        None,
        Some(json!({"root":home.home(),"env":{}})),
    )
    .await;
    assert_eq!(status, 403);
}
