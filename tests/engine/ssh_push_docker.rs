//! Real SSH transport and identity regression. Opt in with AWMAN_TEST_DOCKER=1.
use awman::{
    data::{
        config::env::Env,
        fs::auth_paths::AuthPathResolver,
        workflow_prerequisites::{GitRemoteUrl, PrerequisiteStepKind},
        workflow_state::PhaseKind,
    },
    engine::{
        container::{background::BackgroundLaunchRequest, ContainerRuntime},
        overlay::{prerequisite_grants::GrantTarget, OverlayEngine},
        workflow::{
            prerequisite_grants::{GitIdentityGrant, IdentitySource, PrerequisiteGrants, SshGrant},
            step_commands::{
                setup_step_to_shell_with, teardown_step_to_shell_with, StepCommandOptions,
            },
        },
    },
};
use std::{
    collections::HashMap,
    process::{Command, Stdio},
};

fn docker(args: &[&str]) -> std::process::Output {
    Command::new(awman::engine::host_cli::program("docker"))
        .args(args)
        .output()
        .unwrap()
}
struct Fixture {
    image: String,
    server: String,
    network: String,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.server]);
        let _ = docker(&["network", "rm", &self.network]);
        let _ = docker(&["image", "rm", "-f", &self.image]);
    }
}
#[test]
fn docker_selected_key_pushes_and_setup_clone_uses_same_key_while_identity_commits() {
    if !Env::from_process().test_docker() {
        return;
    }
    assert!(
        crate::helpers::docker_available(),
        "Docker opt-in requires a reachable daemon"
    );
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .arg(&repo)
        .status()
        .unwrap()
        .success());
    std::fs::write(repo.join("file"), "fixture").unwrap();
    let home = dirs::home_dir().unwrap();
    let ssh = home.join(".ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    let key = ssh.join(format!("id_wi0149_{}", uuid::Uuid::new_v4().simple()));
    struct Key(std::path::PathBuf);
    impl Drop for Key {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(self.0.with_extension("pub"));
        }
    }
    let _key = Key(key.clone());
    assert!(Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .unwrap()
        .success());
    let id = uuid::Uuid::new_v4().simple().to_string();
    let fixture = Fixture {
        image: format!("awman-ssh-{id}"),
        server: format!("awman-sshd-{id}"),
        network: format!("awman-ssh-net-{id}"),
    };
    let uid = nix::unistd::geteuid().as_raw();
    std::fs::write(temp.path().join("Dockerfile"), format!("FROM alpine:3.21\nRUN apk add --no-cache git openssh && adduser -D git && echo 'git:fixture' | chpasswd && ssh-keygen -A && mkdir -p /srv /home/client/.ssh /run/sshd && git init --bare /srv/repo.git && chown -R git:git /srv/repo.git && chown -R {uid} /home/client\nRUN id -nu {uid} >/dev/null 2>&1 || adduser -D -u {uid} -h /home/client client\nENV HOME=/home/client\nUSER {uid}\nCMD [\"sleep\",\"infinity\"]\n")).unwrap();
    let built = docker(&[
        "build",
        "-q",
        "-t",
        &fixture.image,
        temp.path().to_str().unwrap(),
    ]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(docker(&["network", "create", &fixture.network])
        .status
        .success());
    let public_mount = format!(
        "{}:/fixtures/key.pub:ro",
        key.with_extension("pub").display()
    );
    assert!(docker(&[
        "run",
        "-d",
        "--name",
        &fixture.server,
        "--network",
        &fixture.network,
        "--network-alias",
        "sshd",
        "--user",
        "0",
        "-v",
        &public_mount,
        &fixture.image,
        "/usr/sbin/sshd",
        "-D",
        "-e",
        "-o",
        "PasswordAuthentication=no",
        "-o",
        "StrictModes=no",
        "-o",
        "AuthorizedKeysFile=/fixtures/key.pub"
    ])
    .status
    .success());
    // ContainerRuntime's background contract intentionally has no network knob.
    // Use the exact engine-generated transport in a fixture client on this network.
    let engine = OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(&home));
    let runtime = ContainerRuntime::docker();
    let mut grants = PrerequisiteGrants {
        identity: Some(GitIdentityGrant {
            name: "Granted Name".into(),
            email: "granted@example.test".into(),
            source: IdentitySource::Host,
        }),
        ssh: Some(SshGrant::Key {
            key_path: key.clone(),
            known_hosts: None,
            passphrase_protected: Some(false),
        }),
        ..Default::default()
    };
    let remote = GitRemoteUrl::parse("ssh://git@sshd/srv/repo.git");
    grants.ssh_destinations.insert(remote.redacted.clone());
    let target = |kind| GrantTarget {
        git_source: None,
        repo: None,
        step_kind: kind,
        phase: PhaseKind::Teardown,
        container_home: Some("/home/client"),
        remote: Some(&remote),
    };
    let identity = engine
        .prerequisite_grant_overlays(&target(PrerequisiteStepKind::CommitChanges), &grants)
        .unwrap();
    let env: HashMap<_, _> = identity.env_literals.into_iter().collect();
    let commit = runtime
        .start_background(&BackgroundLaunchRequest {
            secret_env: &[],
            name: None,
            image: &fixture.image,
            workdir: &repo,
            env: &env,
            overlays: &[],
            builtin_launch: None,
        })
        .unwrap();
    let committed = commit
        .exec(
            "git add . && git commit -m fixture && git log -1 --format='%an <%ae>'",
            None,
        )
        .unwrap();
    assert_eq!(committed.exit_code, 0, "{}", committed.stderr);
    assert!(committed
        .stdout
        .contains("Granted Name <granted@example.test>"));
    drop(commit);
    let overlay = engine
        .prerequisite_grant_overlays(&target(PrerequisiteStepKind::PushBranch), &grants)
        .unwrap();
    let options = StepCommandOptions {
        push_auth: overlay.push_auth,
        remote_url: Some("ssh://git@sshd/srv/repo.git".into()),
    };
    // Publishing accepts branch names, not refspecs.
    let branch = Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let branch_name = String::from_utf8(branch.stdout).unwrap().trim().to_string();
    let push = awman::data::workflow_definition::TeardownStep::PushBranch {
        remote: None,
        branch: Some(branch_name.clone()),
    };
    let (shell, step_env) = teardown_step_to_shell_with(&push, &options);
    let run = |shell: &str, step_env: Option<&HashMap<String, String>>, mounts: bool| {
        let mut command = Command::new(awman::engine::host_cli::program("docker"));
        command
            .args(["run", "--rm", "--network", &fixture.network, "-w"])
            .arg(&repo)
            .arg("-v")
            .arg(format!("{0}:{0}", repo.display()));
        if mounts {
            for mount in &overlay.overlays {
                command.arg("-v").arg(format!(
                    "{}:{}:ro",
                    mount.host_path.display(),
                    mount.container_path.display()
                ));
            }
        }
        if let Some(env) = step_env {
            for (key, value) in env {
                command.arg("-e").arg(key).env(key, value);
            }
        }
        command
            .args([&fixture.image, "sh", "-c", shell])
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    assert!(
        !run(&shell, step_env.as_ref(), false).status.success(),
        "push without the key must fail"
    );
    let pushed = run(&shell, step_env.as_ref(), true);
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    let clone = awman::data::workflow_definition::SetupStep::CloneRepo {
        url: "ssh://git@sshd/srv/repo.git".into(),
        branch: Some(branch_name),
        into: Some("fetched".into()),
        conflict_mode: Default::default(),
    };
    let (shell, env) = setup_step_to_shell_with(&clone, &options);
    let cloned = run(&shell, env.as_ref(), true);
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("fetched/file")).unwrap(),
        "fixture"
    );
}
