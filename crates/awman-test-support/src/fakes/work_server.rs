//! The awman-wsp mock work server, served on loopback for a test.

use crate::TestSandbox;
use awman_wsp::mock::{MockWorkServer, ServingMock};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// A running mock work server. Stop it with [`FakeWorkServer::shutdown`]
/// before the sandbox finishes; a server still accepting at the end of the
/// test is a leak.
pub struct FakeWorkServer {
    server: Arc<MockWorkServer>,
    serving: ServingMock,
}

impl FakeWorkServer {
    /// Start a fresh mock server (inside a tokio runtime). Its address is
    /// tracked by the sandbox's leak check.
    ///
    /// The mock binds `127.0.0.1:0` itself and reports the address, which
    /// the sandbox then tracks (the "child reports its address" form of
    /// owned ports).
    pub async fn start(sandbox: &TestSandbox) -> FakeWorkServer {
        let ports = sandbox.ports();
        let server = Arc::new(MockWorkServer::new());
        let serving = server
            .clone()
            .serve()
            .await
            .unwrap_or_else(|e| panic!("BLOCKED[loopback]: mock work server: {e}"));
        ports.track(serving.addr);
        FakeWorkServer { server, serving }
    }

    /// The mock's state and administration API.
    pub fn server(&self) -> &Arc<MockWorkServer> {
        &self.server
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> &str {
        &self.serving.base_url
    }

    /// Its address.
    pub fn addr(&self) -> SocketAddr {
        self.serving.addr
    }

    /// Stop serving and wait until the port no longer accepts connections.
    pub async fn shutdown(self) {
        let addr = self.serving.addr;
        drop(self.serving);
        crate::wait::wait_for_async(
            "the mock work server to stop",
            Duration::from_secs(5),
            || async move {
                tokio::net::TcpStream::connect(addr)
                    .await
                    .is_err()
                    .then_some(())
            },
        )
        .await;
    }
}
