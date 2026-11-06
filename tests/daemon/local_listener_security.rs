//! The local listener is the only way in without a key, so it must be private:
//! no TCP socket, an owner-only socket, and a tampered sidecar grants nothing.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use serde_json::Value;

use crate::api_process::ApiHome;

fn daemon_dir(home: &ApiHome) -> PathBuf {
    home.home().join(".local/share/awman/data/daemon")
}

fn status_json(home: &ApiHome) -> Value {
    serde_json::from_str(&home.cli_ok(&["daemon", "status", "--json"])).unwrap()
}

/// Inodes of the TCP sockets a pid has in LISTEN state (Linux only).
#[cfg(target_os = "linux")]
fn tcp_listeners(pid: u32) -> Vec<String> {
    let mut inodes = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let text = std::fs::read_to_string(table).unwrap_or_default();
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // st == 0A is LISTEN; field 9 is the socket inode.
            if fields.get(3) == Some(&"0A") {
                inodes.push((fields[1].to_string(), fields[9].to_string()));
            }
        }
    }
    let mut owned = Vec::new();
    if let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path()) {
                let target = target.to_string_lossy().into_owned();
                for (addr, inode) in &inodes {
                    if target == format!("socket:[{inode}]") {
                        owned.push(addr.clone());
                    }
                }
            }
        }
    }
    owned
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_opens_no_tcp_listener_while_the_network_listener_is_off() {
    let home = ApiHome::new();
    let _server = home.start().await;
    let pid = status_json(&home)["pid"].as_u64().unwrap() as u32;
    let listeners = tcp_listeners(pid);
    assert!(
        listeners.is_empty(),
        "the daemon (pid {pid}) must not listen on TCP: {listeners:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_local_socket_and_its_directory_are_owner_only() {
    let home = ApiHome::new();
    let _server = home.start().await;
    let status = status_json(&home);
    let address = status["listeners"]["local"]["address"].as_str().unwrap();
    let socket = std::fs::metadata(address).unwrap();
    assert_eq!(socket.permissions().mode() & 0o077, 0, "{address}");
    let dir = std::fs::metadata(std::path::Path::new(address).parent().unwrap()).unwrap();
    assert_eq!(dir.permissions().mode() & 0o077, 0, "socket dir");
    let state = std::fs::metadata(daemon_dir(&home)).unwrap();
    assert_eq!(state.permissions().mode() & 0o077, 0, "daemon state dir");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tampered_or_missing_sidecar_grants_nothing() {
    let home = ApiHome::new();
    let _server = home.start().await;
    let sidecar = daemon_dir(&home).join("server.json");
    let original = std::fs::read_to_string(&sidecar).unwrap();
    assert!(
        !original.to_lowercase().contains("key"),
        "the sidecar carries no secrets: {original}"
    );

    // Pointing it at another endpoint is refused, never followed.
    let mut tampered: Value = serde_json::from_str(&original).unwrap();
    tampered["transport"]["address"] = "/tmp/not-the-daemons-socket".into();
    std::fs::write(&sidecar, tampered.to_string()).unwrap();
    let error = home.cli_err(&["daemon", "api", "features.list"]);
    assert!(
        error.contains("refusing") || error.contains("unexpected endpoint"),
        "{error}"
    );

    // Garbage is refused too.
    std::fs::write(&sidecar, "not json").unwrap();
    assert!(!home
        .cli(&["daemon", "api", "features.list"])
        .status
        .success());

    // Missing means "no daemon to follow", not a fallback to anything else.
    std::fs::remove_file(&sidecar).unwrap();
    let error = home.cli_err(&["daemon", "api", "features.list"]);
    assert!(error.contains("not running"), "{error}");

    std::fs::write(&sidecar, original).unwrap();
    std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o600)).unwrap();
    home.cli_ok(&["daemon", "api", "features.list"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_sidecar_for_a_dead_daemon_is_ignored_and_the_next_start_recovers() {
    let home = ApiHome::new();
    let mut server = home.start().await;
    server.kill(); // like a crash: pidfile, sidecar and socket stay behind
    assert!(home.cli_ok(&["daemon", "status"]).contains("not running"));
    let mut again = home.start().await;
    assert_eq!(again.get("/v1/status").await["status"], "ok");
    again.kill();
}
