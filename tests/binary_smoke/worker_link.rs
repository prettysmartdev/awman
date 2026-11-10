//! Link and unlink use the real command catalogue, config and private store.
use std::process::Command;

#[test]
fn static_worker_link_and_unlink_roundtrip() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let link = Command::new(crate::awman_bin())
        .env("AWMAN_HOME", &home)
        .current_dir(&repo)
        .args([
            "worker",
            "link",
            "https://work.example.com/base",
            "--name",
            "team",
            "--token",
            "refresh-secret",
            "--label",
            "pool=laptops",
        ])
        .output()
        .unwrap();
    assert!(
        link.status.success(),
        "{}",
        String::from_utf8_lossy(&link.stderr)
    );
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join("config.json")).unwrap()).unwrap();
    assert_eq!(config["workServers"]["team"]["labels"]["pool"], "laptops");
    assert!(!serde_json::to_string(&config)
        .unwrap()
        .contains("refresh-secret"));
    let token_file = home.join("data/work_servers/team.json");
    assert!(token_file.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o077,
            0
        );
    }
    let unlink = Command::new(crate::awman_bin())
        .env("AWMAN_HOME", &home)
        .current_dir(&repo)
        .args(["config", "unset", "workServers.team"])
        .output()
        .unwrap();
    assert!(
        unlink.status.success(),
        "{}",
        String::from_utf8_lossy(&unlink.stderr)
    );
    assert!(!token_file.exists());
}

#[test]
fn worker_link_reads_static_token_from_stdin() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let mut child = Command::new(crate::awman_bin())
        .env("AWMAN_HOME", &home)
        .current_dir(temp.path())
        .args([
            "worker",
            "link",
            "https://work.example.com",
            "--name",
            "stdin",
            "--token",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"from-stdin\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let token = std::fs::read_to_string(home.join("data/work_servers/stdin.json")).unwrap();
    assert!(token.contains("from-stdin"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("from-stdin"));
}
