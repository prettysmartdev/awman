//! Exercises the actual executable used by host Git, including startup bypass.
#[path = "helpers/smart_http.rs"]
mod smart_http;
use awman::engine::git::{
    lease_transport::{
        HelperProgram, LeaseCredentialHelper, LeaseCredentials, LeaseTransport, LeaseUrlPolicy,
    },
    GitEngine,
};
use std::{io::Write, sync::Arc};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_helper_authenticates_clone_fetch_ls_remote_and_push_real_git() {
    assert!(
        smart_http::http_backend_available(),
        "git http-backend required"
    );
    let root = tempfile::tempdir().unwrap();
    let secret = "actual-awman-helper-secret";
    let repo = smart_http::SmartHttpRepo::start(root.path(), Some(secret)).await;
    let credentials = LeaseCredentials::from_wsp(Some(&awman_wsp::Credentials {
        kind: awman_wsp::CredentialKind::Bearer,
        token: Some(secret.into()),
        username: None,
        expires_at: None,
    }))
    .unwrap();
    let helper = Arc::new(
        LeaseCredentialHelper::install(
            &root.path().join("private"),
            &repo.url,
            &credentials,
            HelperProgram::awman(env!("CARGO_BIN_EXE_awman").into()),
            LeaseUrlPolicy::default(),
        )
        .unwrap(),
    );
    let config = helper.git_config_args();
    let nonce = config
        .last()
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .to_string();
    let authority = repo
        .url
        .strip_prefix("http://")
        .and_then(|rest| rest.split_once('/'))
        .map(|(authority, _)| authority.to_string())
        .unwrap();
    let ask = |nonce: &str, request: String| {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_awman"))
            .args(["__git-credential-helper", nonce, "get"])
            .env("AWMAN_LEASE_CREDENTIAL_FILE", helper.file())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    // The approved host and path get the secret …
    let approved = ask(
        &nonce,
        format!("protocol=http\nhost={authority}\npath=repo.git\n\n"),
    );
    assert!(approved.status.success());
    assert!(String::from_utf8_lossy(&approved.stdout).contains(secret));
    // … a wrong nonce, a foreign host, another repository on the same host
    // or a request without a path get nothing.
    for (nonce, request) in [
        (
            "00000000000000000000000000000000".to_string(),
            format!("protocol=http\nhost={authority}\npath=repo.git\n\n"),
        ),
        (
            nonce.clone(),
            "protocol=http\nhost=foreign.example\npath=repo.git\n\n".to_string(),
        ),
        (
            nonce.clone(),
            format!("protocol=http\nhost={authority}\npath=other.git\n\n"),
        ),
        (
            nonce.clone(),
            format!("protocol=http\nhost={authority}\n\n"),
        ),
    ] {
        let output = ask(&nonce, request);
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
    let transport = LeaseTransport::with_helper(helper, LeaseUrlPolicy::default());
    let url = repo.url.clone();
    let dest = root.path().join("clone");
    tokio::task::spawn_blocking(move || {
        let git = GitEngine::new();
        let mut sink = awman::data::message::RecordingMessageSink::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        git.clone_with_lease_transport(&url, Some("main"), &dest, &transport, &mut sink, &cancel)
            .unwrap();
        git.fetch_with_lease_transport(&dest, &[], &transport, &mut sink, &cancel)
            .unwrap();
        assert!(git
            .ls_remote_with_lease_transport(&url, &transport, &mut sink, &cancel)
            .unwrap()
            .contains_key("refs/heads/main"));
        std::fs::write(dest.join("helper-proof"), "actual helper").unwrap();
        let push = git
            .isolate_remote_push(&dest, &url)
            .unwrap()
            .with_lease_transport(transport);
        push.commit_leftovers("helper proof", &mut sink).unwrap();
        push.push("helper-proof", &mut sink).unwrap();
        assert!(!format!("{sink:?}").contains(secret));
        assert!(!std::fs::read_to_string(dest.join(".git/config"))
            .unwrap()
            .contains(secret));
    })
    .await
    .unwrap();
}
