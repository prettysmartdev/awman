//! WI 0134 allowlist-root contract: `GET /v1/workdirs` and the 403/202
//! decisions of `POST /v1/sessions` for directories under `--workdir-root`.
//!
//! Loopback-only and not named `real_network_*`, so `make test-fast` runs them.
//! Fixtures are real `git init` repositories in temp dirs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use awman::command::dispatch::Engines;
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::api_paths::ApiPaths;
use awman::data::fs::auth_paths::AuthPathResolver;
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::frontend::api::routes::{build_router, AppState, AuthMode};

fn state(root: &Path, workdirs: Vec<PathBuf>, roots: Vec<PathBuf>) -> Arc<AppState> {
    let paths = ApiPaths::from_root(root);
    paths.ensure_root().expect("ensure_root");
    let store = SqliteSessionStore::open(paths.root()).expect("open sqlite");
    let auth_paths = AuthPathResolver::at_home(root);
    let runtime = Arc::new(ContainerRuntime::docker());
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let agent_engine = Arc::new(AgentEngine::new(overlay_engine.clone(), runtime.clone()));
    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine,
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, paths.clone())),
        agent_engine,
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(paths.root())),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
    };
    Arc::new(AppState {
        store: Arc::new(store),
        paths,
        workdirs,
        workdir_roots: roots,
        started_at: Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        auth_mode: AuthMode::Disabled,
        engines,
        sessions: Arc::new(awman::data::session_manager::SessionManager::in_memory(
            awman::data::config::env::EnvSnapshot::empty(),
        )),
        event_buses: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        setup_buses: tokio::sync::Mutex::new(HashMap::new()),
    })
}

async fn serve(
    state: Arc<AppState>,
) -> Option<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    let app = build_router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Some((addr, handle))
}

fn git_init(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let out = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .output()
        .expect("git init");
    assert!(out.status.success());
}

async fn create_local(addr: std::net::SocketAddr, workdir: &Path) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/sessions"))
        .json(&serde_json::json!({
            "session_type": "local",
            "workdir": workdir.display().to_string(),
        }))
        .send()
        .await
        .expect("create session");
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or_default())
}

#[tokio::test]
async fn workdirs_endpoint_lists_both_workdirs_and_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let exact_dir = tempfile::tempdir().unwrap();
    let exact = exact_dir.path().to_path_buf();
    let root = PathBuf::from("/srv/not-created-yet");
    let Some((addr, server)) =
        serve(state(tmp.path(), vec![exact.clone()], vec![root.clone()])).await
    else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let body: serde_json::Value = reqwest::get(format!("http://{addr}/v1/workdirs"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["workdirs"],
        serde_json::json!([exact.display().to_string()])
    );
    assert_eq!(
        body["roots"],
        serde_json::json!([root.display().to_string()])
    );

    server.abort();
}

#[tokio::test]
async fn workdirs_endpoint_roots_is_empty_array_when_none_configured() {
    let tmp = tempfile::tempdir().unwrap();
    let Some((addr, server)) = serve(state(tmp.path(), vec![], vec![])).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    let body: serde_json::Value = reqwest::get(format!("http://{addr}/v1/workdirs"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["workdirs"], serde_json::json!([]));
    assert_eq!(body["roots"], serde_json::json!([]));
    server.abort();
}

#[tokio::test]
async fn create_session_direct_child_git_root_is_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let child = root_path.join("repo");
    git_init(&child);
    let Some((addr, server)) = serve(state(tmp.path(), vec![], vec![root_path])).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let (status, body) = create_local(addr, &child).await;
    assert_eq!(status, 202, "got {body}");
    assert!(body["session_id"].as_str().is_some(), "got {body}");

    server.abort();
}

#[tokio::test]
async fn create_session_non_repo_child_and_grandchild_are_403_naming_the_rule() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    // Keep the configured root outside repositories. Roots that later become
    // enclosed by a repository are refused before the child rules are tested.
    let outer = base.path().canonicalize().unwrap().join("outer");
    std::fs::create_dir_all(&outer).unwrap();
    let root = outer.join("roots");
    let plain = root.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let repo = root.join("repo");
    git_init(&repo);
    let grandchild = repo.join("src");
    std::fs::create_dir_all(&grandchild).unwrap();

    let Some((addr, server)) = serve(state(tmp.path(), vec![], vec![root])).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    let (status, body) = create_local(addr, &plain).await;
    assert_eq!(status, 403, "got {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("not its own git root"),
        "got {body}"
    );

    let (status, body) = create_local(addr, &grandchild).await;
    assert_eq!(status, 403, "got {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("not a direct child"),
        "got {body}"
    );

    server.abort();
}

#[tokio::test]
async fn create_session_linked_worktree_child_is_403() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let base_path = base.path().canonicalize().unwrap();
    let main = base_path.join("main");
    git_init(&main);
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(&main)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    let root = base_path.join("roots");
    std::fs::create_dir_all(&root).unwrap();
    let linked = root.join("linked");
    git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "linked",
        linked.to_str().unwrap(),
    ]);

    let Some((addr, server)) = serve(state(tmp.path(), vec![], vec![root])).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    let (status, body) = create_local(addr, &linked).await;
    assert_eq!(status, 403, "got {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains(".git is not a real directory"),
        "got {body}"
    );
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn create_session_symlink_is_resolved_before_the_root_check() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let base_path = base.path().canonicalize().unwrap();
    let root = base_path.join("roots");
    let inside = root.join("repo");
    git_init(&inside);
    let outside = base_path.join("elsewhere");
    git_init(&outside);
    let ok_link = base_path.join("ok-link");
    let bad_link = root.join("bad-link");
    std::os::unix::fs::symlink(&inside, &ok_link).unwrap();
    std::os::unix::fs::symlink(&outside, &bad_link).unwrap();

    let Some((addr, server)) = serve(state(tmp.path(), vec![], vec![root])).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    assert_eq!(create_local(addr, &ok_link).await.0, 202);
    let (status, body) = create_local(addr, &bad_link).await;
    assert_eq!(status, 403, "got {body}");
    server.abort();
}

#[tokio::test]
async fn create_session_root_created_after_startup_works_without_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let root = base.path().canonicalize().unwrap().join("later");
    let Some((addr, server)) = serve(state(tmp.path(), vec![], vec![root.clone()])).await else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };

    // Root does not exist yet: a repo elsewhere is refused.
    let elsewhere = base.path().canonicalize().unwrap().join("elsewhere");
    git_init(&elsewhere);
    assert_eq!(create_local(addr, &elsewhere).await.0, 403);

    let child = root.join("repo");
    git_init(&child);
    let (status, body) = create_local(addr, &child).await;
    assert_eq!(status, 202, "got {body}");
    server.abort();
}

#[tokio::test]
async fn create_session_exact_workdir_entry_behaves_as_before() {
    let tmp = tempfile::tempdir().unwrap();
    let exact = tempfile::tempdir().unwrap();
    let exact_path = exact.path().canonicalize().unwrap(); // not a git repo
    let stranger = tempfile::tempdir().unwrap();
    let Some((addr, server)) = serve(state(tmp.path(), vec![exact_path.clone()], vec![])).await
    else {
        eprintln!("SKIP: cannot bind 127.0.0.1");
        return;
    };
    assert_eq!(create_local(addr, &exact_path).await.0, 202);
    let (status, body) = create_local(addr, &stranger.path().canonicalize().unwrap()).await;
    assert_eq!(status, 403);
    assert!(
        body["error"].as_str().unwrap_or("").contains("is not"),
        "got {body}"
    );
    server.abort();
}
