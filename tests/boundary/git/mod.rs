//! Boundary tests: git over real transports to servers on loopback.
//!
//! The shared git transport conformance suite
//! (`awman::engine::conformance::git`) runs through awman's `GitEngine`
//! against a bare remote served by `git http-backend` (smart HTTP) and by
//! an unprivileged `sshd`. Local-file clones need no server and are covered
//! in the `component` tier.

mod http;
mod ssh;
mod world;

use crate::support;
use awman::engine::conformance::git::git_transport;
use awman_test_support::capability::{GitTransports, LocalGit};

/// `GitEngine` over smart HTTP conforms: clone of the default and of a named
/// branch, pull to a newer head, push of a new branch, and an unreachable
/// server fails without leaving a checkout. Needs only git itself (its
/// `http-backend` CGI), which every boundary runner has.
#[test]
fn smart_http_conforms_to_the_transport_contract() {
    let mut sandbox = support::sandbox("git::smart_http_conforms_to_the_transport_contract");
    let _ = sandbox.require::<LocalGit>();
    sandbox.env_mut().set("GIT_TERMINAL_PROMPT", "0");
    let server = http::SmartHttp::start(&sandbox);
    let world = world::RemoteWorld::new(&sandbox, server);
    let report = git_transport(&world);
    report.assert_conforms();
    assert_eq!(
        report.not_applicable.len(),
        1,
        "only the access-control check is not applicable to the open HTTP server: {:?}",
        report.not_applicable
    );
    drop(world);
    sandbox.finish();
}

/// `GitEngine` over SSH conforms, including a server that refuses the
/// client, which must fail fast instead of prompting.
#[test]
fn ssh_conforms_to_the_transport_contract() {
    let mut sandbox = support::sandbox("git::ssh_conforms_to_the_transport_contract");
    let transports = sandbox.require::<GitTransports>();
    sandbox.env_mut().set("GIT_TERMINAL_PROMPT", "0");
    let server = ssh::SshServer::start(&mut sandbox, &transports);
    let world = world::RemoteWorld::new(&sandbox, server);
    let report = git_transport(&world);
    report.assert_conforms();
    assert!(
        report.not_applicable.is_empty(),
        "an SSH server has access control: {:?}",
        report.not_applicable
    );
    drop(world);
    sandbox.finish();
}
