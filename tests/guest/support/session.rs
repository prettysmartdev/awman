//! Actual executable sessions; synthetic agents exist only as guest mounts.
use super::*;
use awman_test_support::fixtures::RepoBuilder;
use awman_test_support::Exit;

pub fn prepare(s: &TestSandbox, archive: &Path, agent: &str, body: &str) -> PathBuf {
    let repo = RepoBuilder::new(
        s,
        &format!(
            "project-{}",
            s.root()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_lowercase()
        ),
    )
    .file("Dockerfile.dev", "FROM scratch\n")
    .file(
        &format!(".awman/Dockerfile.{agent}"),
        "FROM awman-repo:latest\n",
    )
    .init();
    let synthetic = s.root().join("synthetic-agent");
    std::fs::write(&synthetic, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&synthetic, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let name = match agent {
        "antigravity" => "agy",
        other => other,
    };
    std::fs::write(
        repo.path().join(".awman/config.json"),
        serde_json::to_vec(&serde_json::json!({
            "agents":{"default":agent, "auth":{"mode":"none","refresh":{"enabled":false}}},
            "overlays":[format!("dir({}:/usr/local/bin/{name}:ro)", synthetic.display())]
        }))
        .unwrap(),
    )
    .unwrap();
    let descriptor = awman::engine::agent::first_party::first_party(agent).unwrap();
    let ping = descriptor.ping_argv.as_ref().unwrap();
    let fake = s.install_fake(fake_cli_exe(), &ping[0]);
    let args: Vec<&str> = ping[1..].iter().map(String::as_str).collect();
    fake.on(&args).stdout("synthetic-ready-ping\n");
    configure(s, archive);
    repo.path().to_path_buf()
}
pub fn configure(s: &TestSandbox, archive: &Path) {
    std::fs::write(s.awman_home().join("config.json"), serde_json::to_vec(&serde_json::json!({
        "runtime":{"kind":"builtin", "builtin":{"stateDir":state(s), "imageSource":{"type":"archive", "path":archive}, "network":{"mode":"none"}}}
    })).unwrap()).unwrap();
}
pub fn exec(
    s: &TestSandbox,
    exe: &Path,
    repo: &Path,
    agent: &str,
    prompt: &str,
) -> awman_test_support::process::Output {
    s.command(exe)
        .current_dir(repo)
        .env("AWMAN_TEST_BUILTIN", "1")
        .args(["exec", "prompt", "-n", "--agent", agent, prompt])
        .run()
}
pub fn boot(s: &TestSandbox, exe: &Path, archive: &Path) {
    let repo = prepare(s, archive, "claude", "#!/bin/sh\nIFS= read -r prompt\nprintf 'GUEST_UID=%s\\n' \"$(id -u)\"\nprintf 'GUEST_PROMPT=%s\\n' \"$prompt\"\necho guest-stderr >&2\nexit 37\n");
    ready(s, exe, &repo);
    let out = exec(s, exe, &repo, "claude", "boot-proof");
    assert_eq!(out.exit, Exit::Code(37), "{out:?}");
    assert!(out.stdout_text().contains("GUEST_UID=1234"), "{out:?}");
    assert!(
        out.stdout_text().contains("GUEST_PROMPT=boot-proof"),
        "{out:?}"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("guest-stderr"));
}
pub fn ready(s: &TestSandbox, exe: &Path, repo: &Path) {
    let out = s
        .command(exe)
        .current_dir(repo)
        .env("AWMAN_TEST_BUILTIN", "1")
        .args(["ready", "--json"])
        .run();
    out.assert_success();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["ready"], true, "{out:?}");
}

/// Probe configuration syntax using only config commands in a private home.
/// Published predecessors can predate the runtime.kind schema. This adapts test
/// input; VM state itself is always created and upgraded by the real binaries.
pub fn previous_schema(
    s: &TestSandbox,
    exe: &Path,
    repo: &Path,
    archive: &Path,
) -> Option<Vec<u8>> {
    let probe = s.root().join("previous-config-probe");
    std::fs::create_dir_all(&probe).unwrap();
    let command = || {
        s.command(exe)
            .current_dir(s.work())
            .env("AWMAN_HOME", &probe)
    };
    if command()
        .args(["config", "get", "runtime.kind"])
        .run()
        .exit
        .success()
    {
        return None;
    }
    let runtime = ["builtin-experimental", "builtin"].into_iter().find(|kind| {
        command().args(["config", "set", "runtime", kind, "--global"]).run().exit.success()
    }).expect("BLOCKED[previous-schema]: published predecessor accepts neither supported builtin config spelling");
    let repo_config = repo.join(".awman/config.json");
    let saved = std::fs::read(&repo_config).unwrap();
    let current: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    std::fs::write(
        &repo_config,
        serde_json::to_vec(&serde_json::json!({
            "agent":"claude", "auth":"none", "autoAgentAuthAccepted":true,
            "overlays":current["overlays"]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(s.awman_home().join("config.json"), serde_json::to_vec(&serde_json::json!({
        "runtime":runtime, "default_agent":"claude", "auth":"none", "autoAgentAuthAccepted":true,
        "builtin":{"stateDir":state(s), "imageSource":{"type":"archive", "path":archive}, "network":{"mode":"none"}}
    })).unwrap()).unwrap();
    Some(saved)
}

pub fn restore_schema(s: &TestSandbox, repo: &Path, archive: &Path, saved: Option<Vec<u8>>) {
    if let Some(saved) = saved {
        std::fs::write(repo.join(".awman/config.json"), saved).unwrap();
    }
    configure(s, archive);
}
