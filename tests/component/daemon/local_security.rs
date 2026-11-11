use crate::support::{
    self,
    backend::{Backend, BackendKind},
};
use awman::command::commands::daemon::{actions::Request, client::DaemonClient};
use awman::data::fs::DataPaths;
use std::os::unix::fs::PermissionsExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_endpoint_is_private_and_tampered_metadata_cannot_redirect_clients() {
    let sandbox = support::sandbox("daemon::local_security::owner_endpoint_is_private_and_tampered_metadata_cannot_redirect_clients");
    let backend = Backend::start(BackendKind::Daemon, &sandbox).await;
    let env = support::env_snapshot(&sandbox);
    let paths = DataPaths::from_env(&env).unwrap().daemon();
    assert_eq!(
        std::fs::metadata(backend.endpoint())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(backend.endpoint().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let status = backend
        .client()
        .call(&Request::get("/v1/status"))
        .await
        .unwrap();
    assert!(status["listeners"]["network"]
        .as_object()
        .unwrap()
        .values()
        .all(|l| l["bound"].is_null()));
    let file = paths.server_meta_file();
    let original = std::fs::read(&file).unwrap();
    let metadata: serde_json::Value = serde_json::from_slice(&original).unwrap();
    for (key, value) in [
        (
            "address",
            serde_json::json!(sandbox.sock_dir().join("foreign.sock")),
        ),
        ("kind", serde_json::json!("tcp")),
    ] {
        let mut wrong = metadata.clone();
        wrong["transport"][key] = value;
        std::fs::write(&file, serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert!(DaemonClient::discover(&env).is_err(), "{wrong}");
    }
    std::fs::remove_file(&file).unwrap();
    assert!(DaemonClient::discover(&env).unwrap().is_none());
    std::fs::write(&file, original).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        DaemonClient::connect(&env)
            .unwrap()
            .call(&Request::get("/v1/status"))
            .await
            .unwrap()["pid"],
        status["pid"]
    );
    backend.shutdown().await;
    sandbox.finish();
}
