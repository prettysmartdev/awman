//! Owned ports and Unix sockets, and their leak check.

use crate::{expect_panic, fake_cli_exe, leaky_sandbox, sandbox};
use awman_test_support::Ready;
use std::net::TcpStream;

#[test]
fn listeners_are_loopback_port_zero_and_tracked() {
    let sandbox = sandbox("ports::listeners_are_loopback_port_zero_and_tracked");
    let listener = sandbox.ports().tcp_listener();
    let addr = listener.local_addr().unwrap();
    assert!(addr.ip().is_loopback());
    assert_ne!(addr.port(), 0);
    TcpStream::connect(addr).unwrap();
    drop(listener);
    sandbox.finish();
}

#[test]
fn a_listener_still_open_at_the_end_is_a_leak() {
    let (_parent, sandbox) = leaky_sandbox("ports::a_listener_still_open_at_the_end_is_a_leak");
    let listener = sandbox.ports().tcp_listener();
    let text = expect_panic(|| sandbox.finish());
    assert!(text.starts_with("LEAK: "), "{text}");
    assert!(text.contains("still accepts connections"), "{text}");
    drop(listener);
}

#[test]
fn tracked_child_addresses_are_checked_too() {
    let (_parent, sandbox) = leaky_sandbox("ports::tracked_child_addresses_are_checked_too");
    // hygiene-allow(H4): stands in for a port a child bound and reported.
    #[allow(clippy::disallowed_methods)]
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    sandbox.ports().track(other.local_addr().unwrap());
    let text = expect_panic(|| sandbox.finish());
    assert!(text.contains("still accepts connections"), "{text}");
}

#[test]
fn tcp_readiness_waits_for_accepting() {
    let sandbox = sandbox("ports::tcp_readiness_waits_for_accepting");
    let listener = sandbox.ports().tcp_listener();
    let addr = listener.local_addr().unwrap();
    let fake = sandbox.install_fake(fake_cli_exe(), "svc");
    fake.on(&[]).exit(0);
    let mut child = sandbox
        .command("svc")
        .ready(Ready::TcpAccepts(addr))
        .spawn();
    child.wait(std::time::Duration::from_secs(5));
    drop(listener);
}

#[cfg(unix)]
#[test]
fn unix_sockets_live_in_the_short_dir_and_must_be_removed() {
    let (_parent, sandbox) =
        leaky_sandbox("ports::unix_sockets_live_in_the_short_dir_and_must_be_removed");
    let path = sandbox.ports().unix_socket("daemon.sock");
    assert!(path.starts_with(sandbox.sock_dir()));
    assert!(path.as_os_str().len() <= awman_test_support::net::MAX_SOCKET_PATH);
    #[cfg(unix)]
    {
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(listener);
    }
    let text = expect_panic(|| sandbox.finish());
    assert!(text.contains("daemon.sock was not removed"), "{text}");
}

#[test]
fn socket_names_must_be_plain() {
    let sandbox = sandbox("ports::socket_names_must_be_plain");
    let text = expect_panic(|| {
        sandbox.ports().unix_socket("../escape");
    });
    assert!(text.contains("plain file name"), "{text}");
}
