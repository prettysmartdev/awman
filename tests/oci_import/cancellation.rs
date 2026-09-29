//! Real socket cancellation regressions for the public acquisition boundary.
//! These are controlled loopback services, not evidence of real Docker/store parity.
use crate::support::*;
use awman::data::config::image_source::{ImageSourceSpec, RegistryHostConfig};
use awman::engine::oci::{AcquirePolicy, AcquireRequest, CancelToken, ImageAcquirer, RetryPolicy};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
enum Phase {
    DockerHeaders,
    DockerBody,
    RegistryHeaders,
    TokenHeaders,
    TokenBody,
}
fn request_path(socket: &mut TcpStream) -> String {
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).unwrap(), 1);
        bytes.push(byte[0]);
        assert!(bytes.len() < 64 * 1024);
    }
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .into()
}
fn reply(socket: &mut TcpStream, body: &str) {
    write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}
fn exercise(phase: Phase, cancelled: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        loop {
            let (mut socket, _) = listener.accept().unwrap();
            let path = request_path(&mut socket);
            match phase {
                Phase::DockerBody if path == "/_ping" => {
                    reply(&mut socket, "OK");
                    continue;
                }
                Phase::DockerBody if path == "/version" => {
                    reply(&mut socket, r#"{"ApiVersion":"1.48"}"#);
                    continue;
                }
                Phase::DockerBody if path.ends_with("/json") => {
                    reply(
                        &mut socket,
                        &format!(
                            r#"{{"Os":"linux","Architecture":"{}","Size":128}}"#,
                            awman::data::oci_identity::OciPlatform::host_linux().architecture
                        ),
                    );
                    continue;
                }
                Phase::TokenHeaders | Phase::TokenBody if !path.starts_with("/token") => {
                    write!(socket, "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"http://{address}/token\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                }
                _ => {}
            }
            if matches!(phase, Phase::DockerBody | Phase::TokenBody) {
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 99999\r\n\r\n")
                    .unwrap();
            }
            arrived_tx.send(()).unwrap();
            // Cancellation must close the actual socket, not just abandon a
            // blocked transfer thread until its one-hour request deadline.
            // The forced runtime shutdown that drives this either finishes
            // its graceful FIN in time (`Ok(0)`) or, under scheduling
            // pressure, aborts the connection first (`ConnectionReset`); both
            // mean the peer will never block on this socket again, which is
            // the actual guarantee under test. Anything else — actual bytes,
            // or a read that times out instead of erroring — means the
            // socket was never touched at all.
            let mut byte = [0];
            let outcome = socket.read(&mut byte);
            let closed = matches!(&outcome, Ok(0))
                || matches!(
                    &outcome,
                    Err(e) if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                    )
                );
            assert!(
                closed,
                "{phase:?}: cancelled transport did not close the socket: {outcome:?}"
            );
            return;
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let token = CancelToken::new();
    let registry = !matches!(phase, Phase::DockerHeaders | Phase::DockerBody);
    let request = AcquireRequest {
        tag: "team/agent:latest".into(),
        source: if registry {
            ImageSourceSpec::Registry {
                registry: None,
                reference: Some(format!("{address}/team/agent:latest")),
            }
        } else {
            ImageSourceSpec::DockerStore {
                host: Some(format!("tcp://{address}")),
                tls: None,
                reference: None,
            }
        },
        platform: awman::data::oci_identity::OciPlatform::host_linux(),
        policy: AcquirePolicy::Refresh,
        registries: [(
            address.to_string(),
            RegistryHostConfig {
                insecure: true,
                ca_cert: None,
                auth: None,
            },
        )]
        .into(),
    };
    let acquirer = acquirer(&state)
        .with_cancel(token.clone())
        .with_retry(RetryPolicy {
            deadline: if cancelled {
                Duration::from_secs(3)
            } else {
                Duration::from_millis(300)
            },
            max_attempts: 1,
            ..RetryPolicy::NONE
        });
    let worker = std::thread::spawn(move || acquirer.acquire(&request, &mut |_| {}));
    arrived_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let started = Instant::now();
    if cancelled {
        token.cancel();
    }
    let error = worker.join().unwrap().unwrap_err();
    if cancelled {
        assert!(awman::engine::oci::retry::is_cancelled(&error), "{error}");
    } else {
        assert!(error.to_string().contains("deadline"), "{error}");
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{phase:?}: {error}"
    );
    server.join().unwrap();
    for path in ["images", "refs", "tmp"] {
        assert_eq!(
            std::fs::read_dir(state.join("oci-cache").join(path))
                .unwrap()
                .count(),
            0,
            "partial state in {path}"
        );
    }
}

#[test]
fn cancel_closes_docker_headers_and_body_without_a_transfer_worker() {
    exercise(Phase::DockerHeaders, true);
    exercise(Phase::DockerBody, true);
}
#[test]
fn cancel_closes_registry_and_auth_header_and_body_waits() {
    exercise(Phase::RegistryHeaders, true);
    exercise(Phase::TokenHeaders, true);
    exercise(Phase::TokenBody, true);
}
#[test]
fn deadline_closes_withheld_headers_and_leaves_no_cache_reference() {
    exercise(Phase::DockerHeaders, false);
    exercise(Phase::RegistryHeaders, false);
}
