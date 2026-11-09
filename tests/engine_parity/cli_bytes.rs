use crate::{
    api_process::{AgentMode, ApiHome},
    command::integrations::IntegrationRegistry,
    support::Daemon,
};

fn container_commands(home: &ApiHome) -> String {
    let mut log = std::fs::read_to_string(home.control().join("docker.log")).unwrap();
    // Only allocated identities vary. Preserve every command, flag, image,
    // mount destination and meaningful argument in the comparison.
    for prefix in [
        " --name ",
        "--label awman.session=",
        "awman-claude-dir-minimal-",
        "awman-claude-minimal-",
    ] {
        let mut scan = 0;
        while let Some(found) = log[scan..].find(prefix) {
            let start = scan + found + prefix.len();
            let end = start
                + log[start..]
                    .find(|c: char| c.is_whitespace() || c == '/' || c == ':')
                    .unwrap_or(log.len() - start);
            log.replace_range(start..end, "<allocated-id>");
            scan = start + "<allocated-id>".len();
        }
    }
    log
}
fn git_state(repo: &std::path::Path) -> Vec<Vec<u8>> {
    [
        vec!["status", "--porcelain"],
        vec!["worktree", "list", "--porcelain"],
        vec!["show-ref"],
    ]
    .into_iter()
    .map(|args| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success());
        out.stdout
    })
    .collect()
}
fn remove_other_worktrees(repo: &std::path::Path) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    for path in String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
    {
        if std::path::Path::new(path).canonicalize().unwrap() != repo.canonicalize().unwrap() {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["worktree", "remove", "--force", path])
                .status()
                .unwrap()
                .success());
        }
    }
}
use std::{process::Output, time::Duration};

#[test]
fn shipped_cli_bytes_and_exit_codes_match_both_engines() {
    // Discovery deliberately verifies /proc/comm. Keep that production check:
    // run this test in a named copy of the test executable, never bypass auth.
    let dir = tempfile::Builder::new()
        .prefix("cli-")
        .tempdir_in("/tmp")
        .unwrap();
    let binary = dir.path().join("awman-parity");
    crate::support::copy_test_executable(&binary);
    let output = std::process::Command::new(binary)
        .args(["--exact", "cli_bytes::relay_child", "--nocapture"])
        .env("AWMAN_PARITY_CLI_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_child() {
    if std::env::var("AWMAN_PARITY_CLI_CHILD").as_deref() != Ok("1") {
        return;
    }
    let d = Daemon::start(IntegrationRegistry::compiled_in()).await;
    let home = ApiHome::new();
    // The fake agent emits empty lines, embedded CRs and unterminated tails
    // independently on both pipes. The same script and fixture serve both runs.
    std::fs::write(home.control().join("mode"), "parity-bytes").unwrap();
    let command = home.command(&[], &[]);
    let path = command
        .get_envs()
        .find(|(k, _)| *k == "PATH")
        .unwrap()
        .1
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let fake = path.split(':').next().unwrap().to_owned() + "/docker";
    let script=std::fs::read_to_string(&fake).unwrap().replace("    mode=\"$(cat \"$ctl/mode\" 2>/dev/null)\"","    mode=\"$(cat \"$ctl/mode\" 2>/dev/null)\"\n    if [ \"$mode\" = parity-bytes ]; then printf 'one\\n\\nthree\\rtail'; printf 'err-one\\n\\nerr-tail' >&2; exit 0; fi");
    assert!(script.contains("err-tail"));
    std::fs::write(fake, script).unwrap();
    let repo = d.root.join("repo");
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&repo)
        .status()
        .unwrap();
    assert!(init.success());
    std::fs::write(repo.join(".gitignore"), ".awman/\n").unwrap();
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "-c",
            "user.name=Parity",
            "-c",
            "user.email=parity@example.invalid",
            "add",
            ".",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "-c",
            "user.name=Parity",
            "-c",
            "user.email=parity@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["branch", "parity"])
        .status()
        .unwrap()
        .success());
    async fn run(home: &ApiHome, d: &Daemon, backend: &str, args: &[&str]) -> Output {
        std::fs::write(home.control().join("docker.log"), "").unwrap();
        let mut command = home.command(&[("AWMAN_HOME", d.root.to_str().unwrap())], &[]);
        command
            .current_dir(d.root.join("repo"))
            .args(["--set", &format!("tui.backend={backend}")])
            .args(args);
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::task::spawn_blocking(move || command.output().unwrap()),
        )
        .await
        .expect("CLI deadline")
        .unwrap();
        let operations = std::fs::read_to_string(home.control().join("docker.log")).unwrap();
        let git = std::process::Command::new("git")
            .arg("-C")
            .arg(d.root.join("repo"))
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        let worktrees = std::process::Command::new("git")
            .arg("-C")
            .arg(d.root.join("repo"))
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        crate::support::save_artifact(
            &format!(
                "cli-{}-{}-{}",
                backend,
                if args.contains(&"--branch") {
                    "prompt-worktree"
                } else {
                    args[1]
                },
                output.status.code().unwrap_or(-1)
            ),
            serde_json::json!({"command":args,"container_commands":operations,"stdout":output.stdout,"stderr":output.stderr,"exit_code":output.status.code(),"git_status":git.stdout,"worktrees":worktrees.stdout}),
        );
        output
    }
    for (args, exit) in [
        (vec!["exec", "prompt", "--headless", "fixture"], 0),
        (vec!["exec", "workflow", "missing.toml", "--headless"], 4),
        (
            vec![
                "exec",
                "prompt",
                "--headless",
                "--branch",
                "parity",
                "fixture",
            ],
            0,
        ),
    ] {
        let builtin = run(&home, &d, "builtin", &args).await;
        let expected_commands = container_commands(&home);
        let expected_git = git_state(&repo);
        if args.contains(&"--branch") {
            remove_other_worktrees(&repo);
        }
        let daemon = run(&home, &d, "daemon", &args).await;
        assert_eq!(
            container_commands(&home),
            expected_commands,
            "container command arguments for {args:?}"
        );
        assert_eq!(
            git_state(&repo),
            expected_git,
            "Git/worktree effects for {args:?}"
        );
        assert_eq!(
            daemon.stdout,
            builtin.stdout,
            "stdout for {args:?}; daemon stderr: {}",
            String::from_utf8_lossy(&daemon.stderr)
        );
        assert_eq!(daemon.stderr, builtin.stderr, "stderr for {args:?}");
        assert_eq!(
            daemon.status.code(),
            builtin.status.code(),
            "exit for {args:?}"
        );
        if args[1] == "prompt" {
            assert_eq!(
                builtin.status.code(),
                Some(exit),
                "{}",
                String::from_utf8_lossy(&builtin.stderr)
            );
            assert_eq!(builtin.stdout, b"one\n\nthree\rtail");
            assert!(builtin
                .stderr
                .windows(b"err-one\n\nerr-tail".len())
                .any(|w| w == b"err-one\n\nerr-tail"));
        }
    }
    home.set_agent(AgentMode::Fail);
    let args = ["exec", "prompt", "--headless", "fixture"];
    let builtin = run(&home, &d, "builtin", &args).await;
    let expected_commands = container_commands(&home);
    let expected_git = git_state(&repo);
    let edit = std::fs::read(repo.join("agent-edit.txt")).unwrap();
    std::fs::remove_file(repo.join("agent-edit.txt")).unwrap();
    let daemon = run(&home, &d, "daemon", &args).await;
    assert_eq!(container_commands(&home), expected_commands);
    assert_eq!(git_state(&repo), expected_git);
    assert_eq!(std::fs::read(repo.join("agent-edit.txt")).unwrap(), edit);
    assert_eq!(daemon.stdout, builtin.stdout);
    assert_eq!(daemon.stderr, builtin.stderr);
    assert_eq!(daemon.status.code(), builtin.status.code());
    assert_eq!(builtin.status.code(), Some(3));
}
