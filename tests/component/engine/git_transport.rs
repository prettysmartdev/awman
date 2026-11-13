//! Smart HTTP identity and failure parity for push-session and lease transports.
use crate::support;
use awman::data::config::env::EnvSnapshot;
use awman::data::git_transport::{GitTransportOp, SafeRemote};
use awman::data::message::RecordingMessageSink;
use awman::engine::error::EngineError;
use awman::engine::git::lease_transport::{
    HelperProgram, LeaseCredentialHelper, LeaseCredentials, LeaseTransport, LeaseUrlPolicy,
};
use awman::engine::git::transport::{CredentialSource, HostGitTransport, TransportGuard};
use awman::engine::git::GitEngine;
use awman_test_support::capability::{LocalGit, Loopback};
use awman_test_support::fakes::FakeGitHttp;
use awman_test_support::fixtures::RepoBuilder;
use awman_test_support::TestSandbox;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const GOOD: &str = "test-secret-correct-password";
const STALE: &str = "test-secret-stale-password";
fn sandbox(id: &'static str) -> TestSandbox {
    let s = support::sandbox(id);
    s.require::<LocalGit>();
    s.require::<Loopback>();
    s.redact(GOOD);
    s.redact(STALE);
    s
}
fn guard() -> TransportGuard {
    TransportGuard::new(CancellationToken::new())
}
fn git(s: &TestSandbox, cwd: &Path, args: &[&str]) {
    s.command("git")
        .args(args)
        .current_dir(cwd)
        .run()
        .assert_success();
}
/// A global helper reads a private fixture; no password appears in helper argv.
fn config(s: &TestSandbox, name: &str, user: &str, password: &str) -> std::path::PathBuf {
    let credential = s.root().join(format!("{name}.credential"));
    std::fs::write(
        &credential,
        format!("username={user}\npassword={password}\n"),
    )
    .unwrap();
    let config = s.root().join(format!("{name}.gitconfig"));
    let helper = format!("!cat '{}'", credential.display());
    s.command("git")
        .args(["config", "--file"])
        .arg(&config)
        .args(["credential.helper", &helper])
        .run()
        .assert_success();
    config
}
fn engine(s: &TestSandbox, global: &Path, system: &Path) -> GitEngine {
    let mut values: std::collections::BTreeMap<String, String> = s
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    values.insert("GIT_CONFIG_SYSTEM".into(), system.display().to_string());
    values.insert("GIT_CONFIG_GLOBAL".into(), global.display().to_string());
    // Prove the builder, rather than the test sandbox, disables system config.
    values.remove("GIT_CONFIG_NOSYSTEM");
    GitEngine::with_env(EnvSnapshot::from_full_env(values))
}
fn no_secret(sink: &RecordingMessageSink, error: &str, repo: &Path) {
    let messages = format!("{:?}", sink.all());
    let config = std::fs::read_to_string(repo.join(".git/config")).unwrap_or_default();
    for secret in [GOOD, STALE] {
        assert!(!messages.contains(secret));
        assert!(!error.contains(secret));
        assert!(!config.contains(secret));
    }
}

#[test]
fn system_helper_is_ignored_and_global_helper_authenticates_clone_fetch_ls_remote_and_push() {
    let s = sandbox("engine::git_transport::system_helper_is_ignored_and_global_helper_authenticates_clone_fetch_ls_remote_and_push");
    let seed = RepoBuilder::new(&s, "seed").file("a", "a").init();
    seed.bare_remote("origin");
    let server = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("correct", GOOD)
        .start();
    let url = server.url("origin.git");
    let global = config(&s, "global", "correct", GOOD);
    let system = config(&s, "system", "stale", STALE);
    let g = engine(&s, &global, &system);
    let transport = HostGitTransport::user(CredentialSource::Ambient);
    let clone = s.work().join("clone");
    let mut sink = RecordingMessageSink::new();
    g.clone_with_transport(&url, None, &clone, &transport, &mut sink, &guard())
        .unwrap();
    seed.create_branch("late");
    seed.write("late", "late");
    seed.commit("late");
    seed.push("origin", "late");
    g.fetch_missing_merge_refs(
        &clone,
        &url,
        &["late".into()],
        &transport,
        &mut sink,
        &guard(),
    )
    .unwrap();
    assert!(g.remote_ref_sha(&clone, "origin", "late").is_some());
    assert!(g
        .tag_only_merge_ref(&url, &["late".into()], &transport, &guard())
        .unwrap()
        .is_none());
    std::fs::write(clone.join("result"), "result").unwrap();
    let push = g
        .isolate_remote_push_with_transport(&clone, &url, transport)
        .unwrap();
    push.commit_leftovers("result", &mut sink).unwrap();
    assert!(push.origin_sha("main", &mut sink).unwrap().is_some());
    let head = push.push("main", &mut sink).unwrap();
    assert_eq!(push.origin_sha("main", &mut sink).unwrap(), Some(head));
    let users: Vec<_> = server.requests().into_iter().flatten().collect();
    assert!(users.len() >= 6);
    assert!(users.iter().all(|u| u == "correct"));
    no_secret(&sink, "", &clone);
}

#[test]
fn lease_transport_ignores_system_and_global_helpers_for_clone_fetch_ls_remote_and_push() {
    let s = sandbox("engine::git_transport::lease_transport_ignores_system_and_global_helpers_for_clone_fetch_ls_remote_and_push");
    let seed = RepoBuilder::new(&s, "seed").file("a", "a").init();
    seed.bare_remote("origin");
    let server = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("correct", GOOD)
        .start();
    let url = server.url("origin.git");
    let stale = config(&s, "stale", "stale", STALE);
    let g = engine(&s, &stale, &stale);
    let credentials = LeaseCredentials::from_wsp(Some(&awman_wsp::Credentials {
        kind: awman_wsp::CredentialKind::Basic,
        username: Some("correct".into()),
        token: Some(GOOD.into()),
        expires_at: None,
    }))
    .unwrap();
    let helper = LeaseCredentialHelper::install(
        &s.work().join("credentials"),
        &url,
        &credentials,
        HelperProgram::awman(support::awman_exe().to_path_buf()),
        LeaseUrlPolicy::default(),
    )
    .unwrap();
    let lease = LeaseTransport::with_helper(Arc::new(helper), LeaseUrlPolicy::default());
    let transport = lease.host_transport();
    let clone = s.work().join("clone");
    let mut sink = RecordingMessageSink::new();
    let cancel = CancellationToken::new();
    g.clone_with_lease_transport(&url, None, &clone, &lease, &mut sink, &cancel)
        .unwrap();
    seed.create_branch("late");
    seed.write("late", "late");
    seed.commit("late");
    seed.push("origin", "late");
    g.fetch_missing_merge_refs(
        &clone,
        &url,
        &["late".into()],
        &transport,
        &mut sink,
        &guard(),
    )
    .unwrap();
    assert!(g
        .ls_remote_with_lease_transport(&url, &lease, &mut sink, &cancel)
        .unwrap()
        .contains_key("refs/heads/late"));
    std::fs::write(clone.join("result"), "result").unwrap();
    let push = g
        .isolate_remote_push_with_transport(&clone, &url, transport)
        .unwrap();
    push.commit_leftovers("result", &mut sink).unwrap();
    push.push("main", &mut sink).unwrap();
    assert!(push.origin_sha("main", &mut sink).unwrap().is_some());
    let users: Vec<_> = server.requests().into_iter().flatten().collect();
    assert!(!users.is_empty());
    assert!(users.iter().all(|u| u == "correct"));
    no_secret(&sink, "", &clone);
}

#[test]
fn auth_failure_at_each_operation_is_git_auth_with_operation_and_safe_remote() {
    let s = sandbox("engine::git_transport::auth_failure_at_each_operation_is_git_auth_with_operation_and_safe_remote");
    let repo = RepoBuilder::new(&s, "local").file("a", "a").init();
    repo.bare_remote("origin");
    let server = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("correct", GOOD)
        .start();
    let url = server.url("origin.git");
    let wrong = config(&s, "wrong", "wrong", STALE);
    let g = engine(&s, &wrong, &wrong);
    let t = HostGitTransport::user(CredentialSource::Ambient);
    let safe = SafeRemote::from_url(&url);
    let mut sink = RecordingMessageSink::new();
    for op in [
        GitTransportOp::Clone,
        GitTransportOp::Fetch,
        GitTransportOp::LsRemote,
        GitTransportOp::Push,
    ] {
        let error = match op {
            GitTransportOp::Clone => g
                .clone_with_transport(
                    &url,
                    None,
                    &s.work().join("failed-clone"),
                    &t,
                    &mut sink,
                    &guard(),
                )
                .unwrap_err(),
            GitTransportOp::Fetch => g
                .fetch_with_transport(
                    repo.path(),
                    &url,
                    &["refs/heads/main".into()],
                    &t,
                    &mut sink,
                    &guard(),
                )
                .unwrap_err(),
            GitTransportOp::LsRemote => g
                .ls_remote_with_transport(&url, &t, &mut sink, &guard())
                .unwrap_err(),
            GitTransportOp::Push => g
                .isolate_remote_push_with_transport(repo.path(), &url, t.clone())
                .unwrap()
                .push("main", &mut sink)
                .unwrap_err(),
        };
        assert!(
            matches!(&error, EngineError::GitAuth { operation, remote, .. } if operation == op.as_str() && remote == safe.as_str()),
            "{error}"
        );
        assert!(error.to_string().contains(op.as_str()));
        assert!(error.to_string().contains(safe.as_str()));
        assert_eq!(
            awman::command::session_git_transport::git_failure_code(&error),
            Some(("git_auth", "git_auth_failed"))
        );
        no_secret(&sink, &error.to_string(), repo.path());
        let command = t.command(g.env(), repo.path(), "origin").unwrap();
        let argv = format!("{:?}", command.get_args().collect::<Vec<_>>());
        assert!(!argv.contains(GOOD));
        assert!(!argv.contains(STALE));
    }
}

#[test]
fn a_refusing_user_helper_is_git_auth_without_system_fallback() {
    let s = sandbox(
        "engine::git_transport::a_refusing_user_helper_is_git_auth_without_system_fallback",
    );
    let repo = RepoBuilder::new(&s, "seed").file("a", "a").init();
    repo.bare_remote("origin");
    let server = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("correct", GOOD)
        .start();
    let system = config(&s, "system", "correct", GOOD);
    let global = s.root().join("refuse.config");
    let g = engine(&s, &global, &system);
    for helper in ["!exit 1", "!echo quit=true"] {
        std::fs::write(&global, format!("[credential]\n helper = {helper}\n")).unwrap();
        let error = g
            .ls_remote_with_transport(
                &server.url("origin.git"),
                &HostGitTransport::user(CredentialSource::Ambient),
                &mut RecordingMessageSink::new(),
                &guard(),
            )
            .unwrap_err();
        assert!(matches!(error, EngineError::GitAuth { .. }), "{error}");
    }
    assert!(server.requests().into_iter().all(|u| u.is_none()));
}

#[test]
fn non_fast_forward_and_hook_rejection_keep_git_rejected() {
    let s = sandbox("engine::git_transport::non_fast_forward_and_hook_rejection_keep_git_rejected");
    let seed = RepoBuilder::new(&s, "seed").file("a", "a").init();
    let bare = seed.bare_remote("origin");
    let server = FakeGitHttp::start(&s, &s.root().join("remotes"));
    let url = server.url("origin.git");
    let g = GitEngine::with_env(support::env_snapshot(&s));
    let t = HostGitTransport::user(CredentialSource::Ambient);
    let clone = s.work().join("clone");
    let mut sink = RecordingMessageSink::new();
    g.clone_with_transport(&url, None, &clone, &t, &mut sink, &guard())
        .unwrap();
    seed.write("upstream", "advance");
    seed.commit("upstream");
    seed.push("origin", "main");
    std::fs::write(clone.join("result"), "local").unwrap();
    let push = g
        .isolate_remote_push_with_transport(&clone, &url, t.clone())
        .unwrap();
    push.commit_leftovers("local", &mut sink).unwrap();
    let error = push.push("main", &mut sink).unwrap_err();
    assert!(matches!(&error, EngineError::GitRejected { .. }), "{error}");
    assert_eq!(
        awman::command::session_git_transport::git_failure_code(&error),
        Some(("push", "push_failed"))
    );
    // A real receive-pack policy rejects a new branch independently of history.
    git(
        &s,
        &bare,
        &["config", "receive.denyCurrentBranch", "refuse"],
    );
    git(&s, &bare, &["config", "core.bare", "false"]);
    seed.write("new", "change");
    seed.commit("change");
    let error = g
        .isolate_remote_push_with_transport(seed.path(), &url, t)
        .unwrap()
        .push("main", &mut sink)
        .unwrap_err();
    assert!(matches!(&error, EngineError::GitRejected { .. }), "{error}");
}

#[test]
fn url_with_embedded_credentials_is_refused_before_git_runs() {
    let s =
        sandbox("engine::git_transport::url_with_embedded_credentials_is_refused_before_git_runs");
    let g = GitEngine::with_env(support::env_snapshot(&s));
    let dest = s.work().join("must-not-exist");
    let mut sink = RecordingMessageSink::new();
    let error = g
        .clone_with_transport(
            &format!("https://user:{GOOD}@example.invalid/r?{STALE}"),
            None,
            &dest,
            &HostGitTransport::user(CredentialSource::Ambient),
            &mut sink,
            &guard(),
        )
        .unwrap_err();
    assert!(!dest.exists());
    assert!(sink.all().is_empty());
    assert!(!error.to_string().contains(GOOD));
    assert!(!error.to_string().contains(STALE));
}

#[test]
fn every_operation_of_a_transport_has_the_same_config_env_and_helper_args() {
    let s = sandbox("engine::git_transport::every_operation_of_a_transport_has_the_same_config_env_and_helper_args");
    let repo = RepoBuilder::new(&s, "repo").file("a", "a").init();
    let g = GitEngine::with_env(support::env_snapshot(&s));
    for t in [
        HostGitTransport::user(CredentialSource::Ambient),
        HostGitTransport::isolated(CredentialSource::Anonymous, false),
    ] {
        let expected = t.command(g.env(), s.work().as_path(), "origin").unwrap();
        let expected_args: Vec<_> = expected.get_args().map(|a| a.to_os_string()).collect();
        for op in [
            GitTransportOp::Clone,
            GitTransportOp::Fetch,
            GitTransportOp::LsRemote,
            GitTransportOp::Push,
        ] {
            let command = t.command(g.env(), s.work().as_path(), "origin").unwrap();
            assert_eq!(command.get_args().collect::<Vec<_>>(), expected_args);
            assert_eq!(
                command.get_envs().collect::<Vec<_>>(),
                expected.get_envs().collect::<Vec<_>>(),
                "{}",
                op.as_str()
            );
        }
        let push = g
            .isolate_remote_push_with_transport(repo.path(), "https://example.invalid/r", t)
            .unwrap();
        let push_command = push.command(true).unwrap();
        let push_args: Vec<_> = push_command.get_args().collect();
        assert!(push_args.starts_with(&expected.get_args().collect::<Vec<_>>()));
        assert!(push_command
            .get_envs()
            .any(|(k, v)| k == "GIT_CONFIG_NOSYSTEM" && v == Some("1".as_ref())));
        for setting in [
            "core.hooksPath=/dev/null",
            "core.fsmonitor=false",
            "tag.gpgSign=false",
            "protocol.ext.allow=never",
            "maintenance.auto=false",
            "credential.interactive=false",
        ] {
            assert!(push_args.iter().any(|a| *a == setting));
        }
    }
}

#[cfg(unix)]
#[test]
fn hook_rejection_does_not_echo_remote_stderr_or_become_auth_failure() {
    use std::os::unix::fs::PermissionsExt;
    let s = sandbox(
        "engine::git_transport::hook_rejection_does_not_echo_remote_stderr_or_become_auth_failure",
    );
    let seed = RepoBuilder::new(&s, "seed").file("a", "a").init();
    let bare = seed.bare_remote("origin");
    let hook = bare.join("hooks/pre-receive");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\necho 'authentication failed {GOOD}' >&2\nexit 1\n"),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(
        &s,
        &bare,
        &[
            "config",
            "core.hooksPath",
            hook.parent().unwrap().to_str().unwrap(),
        ],
    );
    seed.write("result", "result");
    seed.commit("result");
    let server = FakeGitHttp::start(&s, &s.root().join("remotes"));
    let g = GitEngine::with_env(support::env_snapshot(&s));
    let mut sink = RecordingMessageSink::new();
    let error = g
        .isolate_remote_push_with_transport(
            seed.path(),
            &server.url("origin.git"),
            HostGitTransport::user(CredentialSource::Ambient),
        )
        .unwrap()
        .push("main", &mut sink)
        .unwrap_err();
    assert!(matches!(&error, EngineError::GitRejected { .. }), "{error}");
    assert_eq!(
        awman::command::session_git_transport::git_failure_code(&error),
        Some(("push", "push_failed"))
    );
    no_secret(&sink, &error.to_string(), seed.path());
}

#[cfg(unix)]
#[test]
fn actual_clone_fetch_ls_remote_and_push_launches_share_transport_policy() {
    use std::os::unix::fs::PermissionsExt;
    let s = sandbox("engine::git_transport::actual_clone_fetch_ls_remote_and_push_launches_share_transport_policy");
    let seed = RepoBuilder::new(&s, "seed").file("a", "a").init();
    seed.bare_remote("origin");
    let http = FakeGitHttp::start(&s, &s.root().join("remotes"));
    let url = http.url("origin.git");
    let log = s.root().join("git-launches");
    let bin = s.root().join("bin-recording");
    std::fs::create_dir(&bin).unwrap();
    let real = s
        .command("sh")
        .args(["-c", "command -v git"])
        .run()
        .stdout_text();
    let script = bin.join("git");
    std::fs::write(&script, format!("#!/bin/sh\nprintf '%s\\0' \"$@\" >> '{}'\nprintf 'ENV:%s:%s\\n' \"$GIT_CONFIG_NOSYSTEM\" \"$GIT_TERMINAL_PROMPT\" >> '{}'\nexec '{}' \"$@\"\n", log.display(), log.display(), real.trim())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut env: std::collections::BTreeMap<String, String> = s
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    env.insert(
        "PATH".into(),
        format!("{}:{}", bin.display(), env.get("PATH").unwrap()),
    );
    let g = GitEngine::with_env(EnvSnapshot::from_full_env(env));
    let t = HostGitTransport::user(CredentialSource::Ambient);
    let clone = s.work().join("recorded-clone");
    let mut sink = RecordingMessageSink::new();
    g.clone_with_transport(&url, None, &clone, &t, &mut sink, &guard())
        .unwrap();
    g.fetch_with_transport(&clone, &url, &[], &t, &mut sink, &guard())
        .unwrap();
    g.ls_remote_with_transport(&url, &t, &mut sink, &guard())
        .unwrap();
    let push = g
        .isolate_remote_push_with_transport(&clone, &url, t)
        .unwrap();
    push.push("main", &mut sink).unwrap();
    let bytes = std::fs::read(log).unwrap();
    let launches: Vec<Vec<String>> = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split(|b| *b == 0)
                .map(|v| String::from_utf8(v.to_vec()).unwrap())
                .collect()
        })
        .collect();
    let mut operations = Vec::new();
    for args in launches {
        let op = args
            .iter()
            .position(|a| matches!(a.as_str(), "clone" | "fetch" | "ls-remote" | "push"));
        if let Some(at) = op {
            operations.push(args[at].clone());
            for setting in [
                "credential.interactive=false",
                "core.hooksPath=/dev/null",
                "core.fsmonitor=false",
                "http.version=HTTP/1.1",
                "http.extraHeader=Connection: close",
                "protocol.file.allow=never",
            ] {
                assert!(
                    args[..at].iter().any(|a| a == setting),
                    "missing {setting}: {args:?}"
                );
            }
            assert_eq!(args.last().unwrap(), "ENV:1:0");
        }
    }
    for op in ["clone", "fetch", "ls-remote", "push"] {
        assert!(
            operations.iter().any(|a| a == op),
            "missing {op}: {operations:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn active_clone_outlives_inactivity_limit_and_ignores_parent_repository_config() {
    use std::os::unix::fs::PermissionsExt;
    let s = sandbox("engine::git_transport::active_clone_outlives_inactivity_limit_and_ignores_parent_repository_config");
    let seed = RepoBuilder::new(&s, "seed").file("a", "a").init();
    seed.bare_remote("origin");
    let http = FakeGitHttp::start(&s, &s.root().join("remotes"));
    let parent = RepoBuilder::new(&s, "parent").file("a", "a").init();
    parent.git(&["config", "credential.helper", "!false"]);
    let bin = s.root().join("slow-git");
    std::fs::create_dir(&bin).unwrap();
    let real = s
        .command("sh")
        .args(["-c", "command -v git"])
        .run()
        .stdout_text();
    let script = bin.join("git");
    // An owned slow Git fixture emits progress throughout the transfer.
    std::fs::write(&script,format!("#!/bin/sh\ncase \" $* \" in *\" clone \"*) for i in 1 2 3 4 5 6 7 8 9 10; do echo progress >&2; sleep 0.1; done;; esac\nexec '{}' \"$@\"\n",real.trim())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut env: std::collections::BTreeMap<String, String> = s
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    env.insert(
        "PATH".into(),
        format!("{}:{}", bin.display(), env.get("PATH").unwrap()),
    );
    let g = GitEngine::with_env(EnvSnapshot::from_full_env(env));
    let mut bound = guard();
    bound.timeout = std::time::Duration::from_millis(400);
    let start = std::time::Instant::now();
    g.clone_with_transport(
        &http.url("origin.git"),
        None,
        &parent.path().join("clone"),
        &HostGitTransport::user(CredentialSource::Ambient),
        &mut RecordingMessageSink::new(),
        &bound,
    )
    .unwrap();
    assert!(start.elapsed() > bound.timeout);
    assert!(parent.path().join("clone/.git/config").exists());
}
