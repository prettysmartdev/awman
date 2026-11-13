//! Exercise the native pipe with the shipped helper process.
#![cfg(windows)]
use awman::data::{
    git_transport::{GitAuthKind, GitAuthSpec},
    secret::SecretString,
};
use awman::engine::git::{
    credential_socket::{CredentialSocketServer, GIT_CREDENTIAL_SOCKET_ENV},
    lease_transport::{CredentialScope, HelperProgram},
    transport::CredentialHelper,
};
use awman_test_support::Stdin;
use std::sync::Arc;

#[test]
fn native_pipe_scopes_and_revokes_the_session_credential() {
    let s = crate::support::sandbox(
        "engine::git_credentials_windows::native_pipe_scopes_and_revokes_the_session_credential",
    );
    let secret = "test-secret-native-pipe";
    s.require::<awman_test_support::capability::LocalGit>();
    s.redact(secret);
    let server = CredentialSocketServer::start(
        &s.root().join("run"),
        HelperProgram::awman(crate::support::awman_exe().into()),
    )
    .unwrap();
    let ticket = server.ticket(
        Arc::new(GitAuthSpec::new(GitAuthKind::Token, None, SecretString::new(secret)).unwrap()),
        CredentialScope::for_http_url("https://git.example/team/repo.git").unwrap(),
    );
    let args = ticket.git_config_args();
    let helper = args
        .iter()
        .find(|a| a.starts_with("credential.helper=!"))
        .unwrap();
    let nonce = helper.rsplit(' ').next().unwrap();
    assert!(!format!("{args:?} {:?}", ticket.env()).contains(secret));
    let ask = |nonce: &str, path: &str| {
        s.command(crate::support::awman_exe())
            .args(["__git-credential-helper", nonce, "get"])
            .env(GIT_CREDENTIAL_SOCKET_ENV, server.path())
            .stdin(Stdin::Bytes(
                format!("protocol=https\nhost=git.example\npath={path}\n\n").into_bytes(),
            ))
            .run()
            .stdout_text()
    };
    assert!(ask(nonce, "team/repo.git").contains(secret));
    assert!(ask(nonce, "team/repo.git").contains(secret));
    assert!(!ask(nonce, "team/other.git").contains(secret));
    assert!(ask(nonce, "team/repo.git").contains(secret));
    assert!(!ask("invalid", "team/repo.git").contains(secret));
    let filled = s
        .command("git")
        .args(["-c", "credential.helper="])
        .args(ticket.git_config_args())
        .env(GIT_CREDENTIAL_SOCKET_ENV, server.path())
        .args(["credential", "fill"])
        .stdin(Stdin::Bytes(
            b"url=https://git.example/team/repo.git\n\n".to_vec(),
        ))
        .run();
    filled.assert_success();
    assert!(filled.stdout_text().contains(secret));
    drop(ticket);
    assert!(!ask(nonce, "team/repo.git").contains(secret));
}
