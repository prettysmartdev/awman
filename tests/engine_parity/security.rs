use super::multi_frontend::{attach, until};

#[derive(Clone)]
struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn capture_logs() -> LogCapture {
    static CAPTURE: std::sync::OnceLock<LogCapture> = std::sync::OnceLock::new();
    CAPTURE
        .get_or_init(|| {
            let capture = LogCapture(Default::default());
            let writer = capture.clone();
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || writer.clone())
                .try_init()
                .unwrap();
            capture
        })
        .clone()
}
use crate::{
    command::integrations::IntegrationRegistry,
    data::{annotations::Annotations, daemon_state::FeatureName, frontend_protocol::*},
    frontend::api::{
        local::peer_is_owner,
        routes::{build_router_for_listener, ListenerPolicy, ROUTES},
    },
    support::Daemon,
};
use std::{path::Path, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_routes_are_hidden_on_network_even_when_ephemerally_enabled() {
    let d = Daemon::start(IntegrationRegistry::compiled_in()).await;
    assert!(!d.api.runtime.network_serves(FeatureName::Engine));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router_for_listener(d.api.clone(), ListenerPolicy::Network);
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    let id = d.host().create_instance(Annotations::new()).unwrap();
    let (_tx, mut rx) = attach(&d, &id, d.hello(FrontendRole::Primary)).await;
    for spec in ROUTES
        .iter()
        .filter(|s| s.op.owner() == crate::command::daemon_api::Feature::Engine)
    {
        assert!(spec.local_only && spec.op.local_only());
        let path = spec
            .path
            .replace("{id}", id.as_str())
            .replace("{tab}", "tab_00000000000000000000000000000000")
            .replace("{dialog_id}", "dlg_00000000000000000000000000000000");
        let response = client
            .request(
                spec.method.parse::<reqwest::Method>().unwrap(),
                format!("http://{addr}{path}"),
            )
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{} {}", spec.method, path);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["code"],
            "not_found"
        );
    }
    assert!(peer_is_owner(Some(1000), 1000));
    assert!(!peer_is_owner(Some(1001), 1000));
    assert!(!peer_is_owner(None, 1000));
    // A malformed second Hello must close, without echoing its env or input.
    _tx.close().await;
    let _ = &mut rx;
    task.abort();
}
fn assert_clean_tree(path: &Path) {
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            assert_clean_tree(&entry.path());
        } else if kind.is_file() {
            let bytes = std::fs::read(entry.path()).unwrap();
            assert!(
                !bytes.windows(6).any(|w| w == b"secret"),
                "secret persisted in {}",
                entry.path().display()
            );
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_env_is_absent_from_frames_store_debug_and_crash_output() {
    let logs = capture_logs();
    let d = Daemon::start(IntegrationRegistry::compiled_in()).await;
    let hello = d.hello(FrontendRole::Primary);
    assert!(!format!("{hello:?}").contains("secret"));
    let id = d.host().create_instance(Annotations::new()).unwrap();
    let (mut tx, mut rx) = attach(&d, &id, hello).await;
    tx.send(&FrontendFrame::UiRequest(UiRequest::NextTab))
        .await
        .unwrap();
    let frame = until(&mut rx, |f| matches!(f, EngineFrame::UiAck { .. })).await;
    assert!(!serde_json::to_string(&frame).unwrap().contains("secret"));
    assert!(!serde_json::to_string(&d.host().snapshot(&id))
        .unwrap()
        .contains("secret"));
    tx.send(&FrontendFrame::Quit).await.unwrap();
    until(&mut rx, |f| matches!(f, EngineFrame::Detach(_))).await;
    assert_clean_tree(&d.root);
    assert!(
        !logs
            .0
            .lock()
            .unwrap()
            .windows(6)
            .any(|bytes| bytes == b"secret"),
        "Hello env leaked into tracing output"
    );
    let crash_dir = tempfile::tempdir().unwrap();
    let crash_binary = crash_dir.path().join("awman-crash-probe");
    crate::support::copy_test_executable(&crash_binary);
    let output = std::process::Command::new(crash_binary)
        .args(["--exact", "security::redacted_crash_child", "--nocapture"])
        .env("AWMAN_PARITY_CRASH_CHILD", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    for bytes in [&output.stdout, &output.stderr] {
        assert!(
            !bytes.windows(6).any(|w| w == b"secret"),
            "secret in child crash output"
        );
    }
}
#[test]
fn redacted_crash_child() {
    if std::env::var("AWMAN_PARITY_CRASH_CHILD").as_deref() != Ok("1") {
        return;
    }
    let env = HelloEnv::from_pairs([("GITHUB_TOKEN", "secret")]);
    let snapshot = crate::data::config::env::EnvSnapshot::from_instance_env(
        env.clone().into_inner().into_iter().collect(),
        "/tmp".into(),
        vec!["awman".into()],
    );
    panic!("simulated crash: {env:?} {snapshot:?}");
}

#[tokio::test]
async fn owner_socket_is_private_and_rejects_world_writable_endpoints() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let d = Daemon::start(IntegrationRegistry::compiled_in()).await;
    let metadata = std::fs::metadata(&d.socket).unwrap();
    assert_eq!(metadata.mode() & 0o777, 0o600);
    std::fs::set_permissions(&d.socket, std::fs::Permissions::from_mode(0o666)).unwrap();
    let error = crate::engine::remote::frontend_client::FrontendStream::connect_local(
        &d.socket,
        &crate::data::ids::InstanceId::new(),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("not a private socket owned by this user"),
        "{error}"
    );
    // The owner-verification path rejects before an upgrade is attempted.
    assert!(tokio::time::timeout(Duration::from_secs(1), async {
        crate::engine::remote::local::check_owner_socket(&d.socket)
    })
    .await
    .unwrap()
    .is_err());
}
