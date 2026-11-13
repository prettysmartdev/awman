//! WI 0145: push-session credentials through the one-shot socket helper and
//! the per-command `credential-cache` listener for container Git.
#![cfg(unix)]
use crate::support;
use awman::data::git_transport::{GitAuthKind, GitAuthSpec};
use awman::data::message::RecordingMessageSink;
use awman::data::secret::SecretString;
use awman::engine::error::EngineError;
use awman::engine::git::credential_socket::{
    CacheSocketListener, CredentialSocketServer, GIT_CREDENTIAL_SOCKET_ENV,
};
use awman::engine::git::lease_transport::{CredentialScope, HelperProgram};
use awman::engine::git::transport::{CredentialSource, HostGitTransport, TransportGuard};
use awman::engine::git::GitEngine;
use awman_test_support::capability::{LocalGit, Loopback};
use awman_test_support::fakes::{FakeGitHttp, GitHttpFault};
use awman_test_support::fixtures::RepoBuilder;
use awman_test_support::{wait_for, Stdin, TestSandbox};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "test-secret-session-token";
const STALE: &str = "test-secret-stale-user-helper";

fn sandbox(id: &'static str) -> TestSandbox {
    let s = support::sandbox(id);
    s.require::<LocalGit>();
    s.require::<Loopback>();
    s.redact(TOKEN);
    s.redact(STALE);
    s
}

fn spec(password: &str) -> Arc<GitAuthSpec> {
    Arc::new(
        GitAuthSpec::new(
            GitAuthKind::Basic,
            Some("alice".into()),
            SecretString::new(password),
        )
        .unwrap(),
    )
}

fn server(s: &TestSandbox) -> Arc<CredentialSocketServer> {
    // Keep the socket path short (sun_path).
    CredentialSocketServer::start(
        &s.root().join("r"),
        HelperProgram::awman(support::awman_exe().to_path_buf()),
    )
    .unwrap()
}

/// A global helper that would answer with a stale password: the session
/// transport must reset it.
fn engine_with_stale_global_helper(s: &TestSandbox) -> GitEngine {
    let credential = s.root().join("stale.credential");
    std::fs::write(&credential, format!("username=alice\npassword={STALE}\n")).unwrap();
    let global = s.root().join("global.gitconfig");
    s.command("git")
        .args(["config", "--file"])
        .arg(&global)
        .args([
            "credential.helper",
            &format!("!cat '{}'", credential.display()),
        ])
        .run()
        .assert_success();
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
    values.insert("GIT_CONFIG_GLOBAL".into(), global.display().to_string());
    GitEngine::with_env(awman::data::config::env::EnvSnapshot::from_full_env(values))
}

fn guard(cancel: &CancellationToken) -> TransportGuard {
    TransportGuard::new(cancel.clone())
}

fn assert_clean(text: &str, what: &str) {
    for secret in [TOKEN, STALE] {
        assert!(!text.contains(secret), "{what} carries a secret");
    }
}

#[test]
fn session_credential_clone_fetch_ls_remote_and_push_leave_no_secret_in_argv_env_or_git_config() {
    let s = sandbox("engine::git_credentials::session_credential_clone_fetch_ls_remote_and_push_leave_no_secret_in_argv_env_or_git_config");
    let seed = RepoBuilder::new(&s, "seed").file("a", "a").init();
    seed.bare_remote("origin");
    let http = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("alice", TOKEN)
        .start();
    let url = http.url("origin.git");
    let g = engine_with_stale_global_helper(&s);
    let sockets = server(&s);
    let ticket = sockets.ticket(spec(TOKEN), CredentialScope::for_http_url(&url).unwrap());
    let transport = HostGitTransport::user(CredentialSource::Helper(Arc::new(ticket)));

    // The built command: nonce in argv, socket path in env, never the token.
    let built = transport.command(g.env(), &s.work(), "origin").unwrap();
    let argv = format!("{:?}", built.get_args().collect::<Vec<_>>());
    let env = format!("{:?}", built.get_envs().collect::<Vec<_>>());
    assert_clean(&argv, "argv");
    assert_clean(&env, "environment");
    assert!(argv.contains("credential.helper=") && argv.contains("__git-credential-helper"));
    assert!(env.contains(GIT_CREDENTIAL_SOCKET_ENV));

    let cancel = CancellationToken::new();
    let clone = s.work().join("clone");
    let mut sink = RecordingMessageSink::new();
    g.clone_with_transport(&url, None, &clone, &transport, &mut sink, &guard(&cancel))
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
        &guard(&cancel),
    )
    .unwrap();
    assert!(g
        .tag_only_merge_ref(&url, &["late".into()], &transport, &guard(&cancel))
        .unwrap()
        .is_none());
    std::fs::write(clone.join("result"), "result").unwrap();
    let push = g
        .isolate_remote_push_with_transport(&clone, &url, transport.clone())
        .unwrap();
    push.commit_leftovers("result", &mut sink).unwrap();
    let head = push.push("main", &mut sink).unwrap();
    assert_eq!(push.origin_sha("main", &mut sink).unwrap(), Some(head));

    let users: Vec<_> = http.requests().into_iter().flatten().collect();
    assert!(users.len() >= 5, "{users:?}");
    assert!(users.iter().all(|u| u == "alice"));
    let config = std::fs::read_to_string(clone.join(".git/config")).unwrap();
    assert_clean(&config, ".git/config");
    assert!(
        config.contains(&format!("url = {url}")),
        "origin keeps the clean URL"
    );
    assert!(!config.contains("credential"), "{config}");
    assert_clean(&format!("{:?}", sink.all()), "messages");
    let listing = s
        .command("git")
        .args(["config", "--list", "--show-origin"])
        .current_dir(&clone)
        .run();
    assert_clean(&listing.stdout_text(), "git config --list");
    drop(transport);
    drop(push);
    assert_eq!(
        sockets.live_tickets(),
        0,
        "tickets end with their transport"
    );
}

/// While a session clone is in flight (the server holds the request that
/// follows authentication), no process of this user carries the token in its
/// command line or environment. Linux reads `/proc`, the same data `ps`
/// shows.
#[cfg(target_os = "linux")]
#[test]
fn running_git_and_helper_processes_carry_no_secret_in_argv_or_environment() {
    let s = sandbox("engine::git_credentials::running_git_and_helper_processes_carry_no_secret_in_argv_or_environment");
    let seed = RepoBuilder::new(&s, "seed").file("a", "a").init();
    seed.bare_remote("origin");
    let held = Arc::new(AtomicBool::new(false));
    let http = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("alice", TOKEN)
        .fault({
            let held = Arc::clone(&held);
            move |request| {
                (request.method == "POST").then(|| {
                    held.store(true, Ordering::SeqCst);
                    GitHttpFault::StallBeforeResponse
                })
            }
        })
        .start();
    let url = http.url("origin.git");
    let g = engine_with_stale_global_helper(&s);
    let sockets = server(&s);
    let transport = HostGitTransport::user(CredentialSource::Helper(Arc::new(
        sockets.ticket(spec(TOKEN), CredentialScope::for_http_url(&url).unwrap()),
    )));
    let cancel = CancellationToken::new();
    let clone = s.work().join("clone");
    let worker = {
        let (url, clone, transport, cancel, g) = (
            url.clone(),
            clone.clone(),
            transport.clone(),
            cancel.clone(),
            g,
        );
        std::thread::spawn(move || {
            g.clone_with_transport(
                &url,
                None,
                &clone,
                &transport,
                &mut RecordingMessageSink::new(),
                &guard(&cancel),
            )
        })
    };
    wait_for("the authenticated request", Duration::from_secs(30), || {
        held.load(Ordering::SeqCst).then_some(())
    });
    let mut saw_session_git = false;
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let pid = entry.path();
        if !pid
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let read = |name: &str| std::fs::read(pid.join(name)).unwrap_or_default();
        let cmdline = String::from_utf8_lossy(&read("cmdline")).replace('\0', " ");
        let environ = String::from_utf8_lossy(&read("environ")).replace('\0', " ");
        assert_clean(&cmdline, "a process command line");
        assert_clean(&environ, "a process environment");
        if cmdline.contains("__git-credential-helper") {
            saw_session_git = true;
            assert!(environ.contains(GIT_CREDENTIAL_SOCKET_ENV), "{cmdline}");
        }
    }
    assert!(saw_session_git, "the session's Git process was running");
    cancel.cancel();
    let result = worker.join().unwrap();
    assert!(result.is_err());
}

fn ask_helper(s: &TestSandbox, socket: &Path, nonce: &str, request: &str) -> String {
    let out = s
        .command(support::awman_exe())
        .args(["__git-credential-helper", nonce, "get"])
        .env(GIT_CREDENTIAL_SOCKET_ENV, socket)
        .stdin(Stdin::Bytes(request.as_bytes().to_vec()))
        .run();
    out.assert_success();
    out.stdout_text()
}

fn nonce_of(transport_args: &[String]) -> String {
    transport_args
        .iter()
        .find_map(|arg| arg.strip_prefix("credential.helper=!"))
        .and_then(|helper| helper.rsplit(' ').next())
        .unwrap()
        .to_string()
}

#[test]
fn socket_helper_answers_only_a_live_nonce_for_the_session_host_and_path() {
    use awman::engine::git::transport::CredentialHelper;
    let s = sandbox(
        "engine::git_credentials::socket_helper_answers_only_a_live_nonce_for_the_session_host_and_path",
    );
    let sockets = server(&s);
    let ticket = sockets.ticket(
        spec(TOKEN),
        CredentialScope::for_http_url("https://git.example/team/repo.git").unwrap(),
    );
    let nonce = nonce_of(&ticket.git_config_args());
    let socket = sockets.path().to_path_buf();
    let good = "protocol=https\nhost=git.example\npath=team/repo.git\n\n";
    assert_eq!(
        ask_helper(&s, &socket, &nonce, good),
        format!("username=alice\npassword={TOKEN}\n")
    );
    for refused in [
        "protocol=https\nhost=other.example\npath=team/repo.git\n\n",
        "protocol=http\nhost=git.example\npath=team/repo.git\n\n",
        "protocol=https\nhost=git.example\n\n",
    ] {
        assert_eq!(ask_helper(&s, &socket, &nonce, refused), "", "{refused}");
    }
    let wrong = format!("{:0>32}", "1");
    assert_eq!(ask_helper(&s, &socket, &wrong, good), "");
}

#[test]
fn wrong_repository_on_the_same_host_gets_nothing() {
    use awman::engine::git::transport::CredentialHelper;
    let s = sandbox("engine::git_credentials::wrong_repository_on_the_same_host_gets_nothing");
    let sockets = server(&s);
    let ticket = sockets.ticket(
        spec(TOKEN),
        CredentialScope::for_http_url("https://git.example/team/repo.git").unwrap(),
    );
    let nonce = nonce_of(&ticket.git_config_args());
    assert_eq!(
        ask_helper(
            &s,
            sockets.path(),
            &nonce,
            "protocol=https\nhost=git.example\npath=team/other.git\n\n"
        ),
        ""
    );
}

#[test]
fn a_dropped_ticket_no_longer_answers() {
    use awman::engine::git::transport::CredentialHelper;
    let s = sandbox("engine::git_credentials::a_dropped_ticket_no_longer_answers");
    let sockets = server(&s);
    let ticket = sockets.ticket(
        spec(TOKEN),
        CredentialScope::for_http_url("https://git.example/team/repo.git").unwrap(),
    );
    let nonce = nonce_of(&ticket.git_config_args());
    let good = "protocol=https\nhost=git.example\npath=team/repo.git\n\n";
    assert!(!ask_helper(&s, sockets.path(), &nonce, good).is_empty());
    drop(ticket);
    assert_eq!(ask_helper(&s, sockets.path(), &nonce, good), "");
}

#[test]
fn a_refused_session_credential_is_git_auth_failed_without_falling_back() {
    let s = sandbox(
        "engine::git_credentials::a_refused_session_credential_is_git_auth_failed_without_falling_back",
    );
    let seed = RepoBuilder::new(&s, "seed").file("a", "a").init();
    seed.bare_remote("origin");
    // The stale global helper would be refused too; the expired session
    // token is what Git is offered.
    let http = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("alice", "test-secret-current-token")
        .start();
    let url = http.url("origin.git");
    let g = engine_with_stale_global_helper(&s);
    let sockets = server(&s);
    let transport = HostGitTransport::user(CredentialSource::Helper(Arc::new(
        sockets.ticket(spec(TOKEN), CredentialScope::for_http_url(&url).unwrap()),
    )));
    let mut sink = RecordingMessageSink::new();
    let error = g
        .clone_with_transport(
            &url,
            None,
            &s.work().join("clone"),
            &transport,
            &mut sink,
            &guard(&CancellationToken::new()),
        )
        .unwrap_err();
    assert!(
        matches!(&error, EngineError::GitAuth { operation, .. } if operation == "clone"),
        "{error}"
    );
    assert_clean(&error.to_string(), "the error");
    assert_clean(&format!("{:?}", sink.all()), "messages");
    let users: Vec<_> = http.requests().into_iter().flatten().collect();
    assert!(users.iter().all(|u| u == "alice"), "{users:?}");
}

fn cache_get(s: &TestSandbox, socket: &Path, request: &str) -> String {
    let out = s
        .command("git")
        .args(["credential-cache", "--socket"])
        .arg(socket)
        .arg("get")
        .stdin(Stdin::Bytes(request.as_bytes().to_vec()))
        .run();
    out.stdout_text()
}

#[test]
fn cache_protocol_listener_answers_git_credential_cache_client() {
    let s = sandbox(
        "engine::git_credentials::cache_protocol_listener_answers_git_credential_cache_client",
    );
    let listener = CacheSocketListener::start(
        &s.root().join("r"),
        spec(TOKEN),
        CredentialScope::for_http_url("http://127.0.0.1:9/team/repo.git").unwrap(),
    )
    .unwrap();
    let socket: PathBuf = listener.socket().to_path_buf();
    assert_eq!(
        cache_get(&s, &socket, "protocol=http\nhost=127.0.0.1:9\n\n"),
        ""
    );
    let answer = cache_get(
        &s,
        &socket,
        "protocol=http\nhost=127.0.0.1:9\npath=team/repo.git\n\n",
    );
    assert!(
        answer.contains(&format!("password={TOKEN}")),
        "real credential-cache client got: {answer:?}"
    );
    assert!(cache_get(
        &s,
        &socket,
        "protocol=http\nhost=127.0.0.1:9\npath=team/repo.git\n\n"
    )
    .contains("username=alice"));
    assert_eq!(
        cache_get(&s, &socket, "protocol=https\nhost=evil.example\n\n"),
        ""
    );
    assert_eq!(
        cache_get(
            &s,
            &socket,
            "protocol=http\nhost=127.0.0.1:9\npath=team/other.git\n\n"
        ),
        ""
    );
    // The directory is the container mount source and ends with the command.
    let dir = listener.dir().to_path_buf();
    drop(listener);
    assert!(!dir.exists());
}

/// A token that expires (the remote starts refusing it) after the clone:
/// the next operation of the same session is `git_auth_failed`, never a
/// fallback to the user's own helper.
#[test]
fn an_expired_token_mid_session_reports_git_auth_failed() {
    let s =
        sandbox("engine::git_credentials::an_expired_token_mid_session_reports_git_auth_failed");
    let seed = RepoBuilder::new(&s, "seed").file("a", "a").init();
    seed.bare_remote("origin");
    let expired = Arc::new(AtomicBool::new(false));
    let http = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("alice", TOKEN)
        .fault({
            let expired = Arc::clone(&expired);
            move |_| {
                expired
                    .load(Ordering::SeqCst)
                    .then_some(GitHttpFault::RefuseCredentials)
            }
        })
        .start();
    let url = http.url("origin.git");
    let g = engine_with_stale_global_helper(&s);
    let sockets = server(&s);
    let transport = HostGitTransport::user(CredentialSource::Helper(Arc::new(
        sockets.ticket(spec(TOKEN), CredentialScope::for_http_url(&url).unwrap()),
    )));
    let cancel = CancellationToken::new();
    let clone = s.work().join("clone");
    let mut sink = RecordingMessageSink::new();
    g.clone_with_transport(&url, None, &clone, &transport, &mut sink, &guard(&cancel))
        .unwrap();
    expired.store(true, Ordering::SeqCst);
    std::fs::write(clone.join("result"), "result").unwrap();
    let push = g
        .isolate_remote_push_with_transport(&clone, &url, transport)
        .unwrap();
    push.commit_leftovers("result", &mut sink).unwrap();
    let error = push.push("main", &mut sink).unwrap_err();
    assert!(
        matches!(&error, EngineError::GitAuth { operation, .. } if operation == "push" || operation == "ls-remote"),
        "{error}"
    );
    assert_eq!(
        awman::command::session_git_transport::git_failure_code(&error),
        Some(("git_auth", "git_auth_failed"))
    );
    assert_clean(&error.to_string(), "the error");
    assert_eq!(http.receive_packs(), 0);
}

#[test]
fn explicit_credentials_never_fall_back_to_netrc_or_http_headers() {
    use base64::Engine as _;
    let s = sandbox(
        "engine::git_credentials::explicit_credentials_never_fall_back_to_netrc_or_http_headers",
    );
    let seed = RepoBuilder::new(&s, "seed").file("a", "a").init();
    seed.bare_remote("origin");
    let http = FakeGitHttp::builder(&s, &s.root().join("remotes"))
        .basic_auth("alice", TOKEN)
        .start();
    let url = http.url("origin.git");
    std::fs::write(
        s.home().join(".netrc"),
        format!("machine 127.0.0.1 login alice password {TOKEN}\n"),
    )
    .unwrap();
    let g = engine_with_stale_global_helper(&s);
    let global = g.env().get("GIT_CONFIG_GLOBAL").unwrap();
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("alice:{TOKEN}"));
    s.command("git")
        .args([
            "config",
            "--file",
            global,
            &format!("http.{url}.extraHeader"),
            &format!("Authorization: Basic {encoded}"),
        ])
        .run()
        .assert_success();
    let sockets = server(&s);
    for isolated in [false, true] {
        let source = CredentialSource::Helper(Arc::new(
            sockets.ticket(spec(STALE), CredentialScope::for_http_url(&url).unwrap()),
        ));
        let transport = if isolated {
            HostGitTransport::isolated(source, false)
        } else {
            HostGitTransport::user(source)
        };
        let mut sink = RecordingMessageSink::new();
        let error = g
            .ls_remote_with_transport(
                &url,
                &transport,
                &mut sink,
                &guard(&CancellationToken::new()),
            )
            .unwrap_err();
        assert!(matches!(error, EngineError::GitAuth { .. }), "{error}");
        assert_clean(&error.to_string(), "auth failure");
        assert_clean(&format!("{:?}", sink.all()), "events");
    }
}

#[test]
fn credential_scope_matches_real_git_wire_paths_and_ports_exactly_once() {
    use awman::engine::git::transport::CredentialHelper;
    let s = sandbox("engine::git_credentials::credential_scope_matches_real_git_wire_paths_and_ports_exactly_once");
    let sockets = server(&s);
    for url in [
        "https://example.invalid:443/repo.git",
        "http://example.invalid:80/repo.git",
        "https://[::1]:443/repo.git",
        "https://example.invalid:8443/a%252Fb.git",
        "https://example.invalid/a/../b.git",
        "https://example.invalid/a%2Fb.git",
        "https://example.invalid/a%25b.git",
        "https://éxample.invalid/repo.git",
    ] {
        let scope = CredentialScope::for_http_url(url).unwrap();
        let ticket = sockets.ticket(spec(TOKEN), scope);
        let out = s
            .command("git")
            .args(["-c", "credential.helper="])
            .args(ticket.git_config_args())
            .env(&ticket.env()[0].0, &ticket.env()[0].1)
            .args(["credential", "fill"])
            .stdin(Stdin::Bytes(format!("url={url}\n\n").into_bytes()))
            .run();
        out.assert_success();
        assert!(out.stdout_text().contains(TOKEN), "same URL refused: {url}");
    }
    let ticket = sockets.ticket(
        spec(TOKEN),
        CredentialScope::for_http_url("https://example.invalid/a/b.git").unwrap(),
    );
    let out = s
        .command("git")
        .args(["-c", "credential.helper="])
        .args(ticket.git_config_args())
        .env(&ticket.env()[0].0, &ticket.env()[0].1)
        .args(["credential", "fill"])
        .stdin(Stdin::Bytes(
            b"url=https://example.invalid/a%252Fb.git\n\n".to_vec(),
        ))
        .run();
    assert!(!out.stdout_text().contains(TOKEN));
    assert!(!out.exit.success());
    assert!(CredentialScope::for_http_url("https://example.invalid/%FF.git").is_none());
}
