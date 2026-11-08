//! HTTP over the daemon's local owner-only transport. No bearer key, no TCP.
//!
//! Layer 1. Every local caller (`awman daemon api`, `config set` reload, the
//! squad commands) reaches the daemon through [`discover`] and [`LocalClient`].
//!
//! Trust model:
//! - Unix: the client connects only to a socket that is owned by this user,
//!   has no group/other permission bits, and sits in a directory owned by this
//!   user and closed to others (so the path cannot be swapped after the check).
//!   After connecting it also requires the server's peer UID to be this user.
//!   The server independently checks the client's peer UID.
//! - Windows: the pipe carries an owner-only DACL and the server checks the
//!   caller's token user (see `awman_host_shims::verify_client`).
//! - `server.json` is endpoint metadata only; it is read for the address and
//!   protocol identity and is cross-checked against the live pidfile and the
//!   expected endpoint name, so a missing, stale or tampered file grants
//!   nothing.

use super::HttpResponse;
use crate::data::fs::daemon_paths::DaemonPaths;
use crate::data::fs::daemon_sidecar::{ServerSidecar, SidecarTransportKind, PROTOCOL_API_VERSION};
use crate::engine::daemon::DaemonSupervisor;
use crate::engine::error::EngineError;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default whole-request timeout for [`LocalClient::send`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a client says when nothing is listening.
pub const NO_DAEMON: &str = "no awman daemon is running; start it with `awman daemon start`";

/// A running daemon's local endpoint, as discovered from the live pidfile and
/// its `server.json`.
#[derive(Debug, Clone)]
pub struct LocalEndpoint {
    pub address: PathBuf,
    pub pid: u32,
    pub sidecar: ServerSidecar,
}

/// Sidecar + live pidfile. `Ok(None)` when no daemon runs. A sidecar whose pid
/// is dead or differs from the pidfile is stale: ignored (reported as `None`),
/// never followed.
pub fn discover(paths: &DaemonPaths) -> Result<Option<LocalEndpoint>, EngineError> {
    discover_at(
        &paths.pid_file(),
        &paths.server_meta_file(),
        &paths.local_endpoint(),
    )
}

/// [`discover`] over explicit files; `expected` is the endpoint the daemon's
/// home implies (`DaemonPaths::local_endpoint`).
pub fn discover_at(
    pid_file: &Path,
    sidecar_file: &Path,
    expected: &Path,
) -> Result<Option<LocalEndpoint>, EngineError> {
    let Some(pid) = crate::data::fs::daemon_process::read_pid(pid_file)? else {
        return Ok(None);
    };
    if !DaemonSupervisor::is_process_alive(pid) || !DaemonSupervisor::pid_is_awman(pid) {
        return Ok(None);
    }
    let Some(sidecar) = ServerSidecar::read(sidecar_file)? else {
        // Live pid but no metadata yet: the daemon is still starting (or the
        // file was removed). Nothing to follow.
        return Ok(None);
    };
    if sidecar.pid != pid {
        return Ok(None);
    }
    if sidecar.transport.kind != SidecarTransportKind::native() {
        return Err(refuse_sidecar(
            sidecar_file,
            "names a transport this platform cannot use",
        ));
    }
    if sidecar.api_protocol() != Some(PROTOCOL_API_VERSION) {
        return Err(EngineError::Other(format!(
            "the running awman daemon (version {}, PID {pid}) speaks a different API protocol; \
             restart it with `awman daemon stop && awman daemon start`",
            sidecar.version
        )));
    }
    let address = PathBuf::from(&sidecar.transport.address);
    if !endpoint_matches(&address, expected) {
        return Err(refuse_sidecar(sidecar_file, "names an unexpected endpoint"));
    }
    Ok(Some(LocalEndpoint {
        address,
        pid,
        sidecar,
    }))
}

fn refuse_sidecar(path: &Path, why: &str) -> EngineError {
    EngineError::Other(format!(
        "{} {why}; refusing to connect. Remove it and restart the daemon",
        path.display()
    ))
}

/// The recorded address must be the endpoint this home implies. On Unix the
/// directory may legitimately differ (it can depend on `XDG_RUNTIME_DIR` when
/// the home path is too long for a socket), so an absolute path with the same
/// file name is accepted; the socket checks in [`LocalClient`] still apply.
fn endpoint_matches(address: &Path, expected: &Path) -> bool {
    if address == expected {
        return true;
    }
    #[cfg(unix)]
    {
        address.is_absolute()
            && address.file_name().is_some()
            && address.file_name() == expected.file_name()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// HTTP/1.1 client over the local transport.
#[derive(Debug, Clone)]
pub struct LocalClient {
    endpoint: PathBuf,
    timeout: Duration,
}

impl LocalClient {
    pub fn new(endpoint: PathBuf) -> Self {
        Self {
            endpoint,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self { timeout, ..self }
    }

    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }

    /// Send one request and buffer the JSON (or text) response.
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<HttpResponse, EngineError> {
        tokio::time::timeout(self.timeout, async {
            let response = self.send_raw_inner(method, path, body).await?;
            let status = response.status().as_u16();
            use http_body_util::BodyExt;
            let bytes = response
                .into_body()
                .collect()
                .await
                .map_err(|e| failure(e.to_string()))?
                .to_bytes();
            let body = if bytes.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_slice(&bytes)
                    .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).to_string().into())
            };
            Ok(HttpResponse { status, body })
        })
        .await
        .map_err(|_| failure("timed out".into()))?
    }

    /// Same, returning the raw response for streaming bodies (SSE). The
    /// timeout covers connecting and the response head only.
    pub async fn send_raw(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<hyper::Response<hyper::body::Incoming>, EngineError> {
        tokio::time::timeout(self.timeout, self.send_raw_inner(method, path, body))
            .await
            .map_err(|_| failure("timed out".into()))?
    }

    async fn send_raw_inner(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<hyper::Response<hyper::body::Incoming>, EngineError> {
        let stream = connect(&self.endpoint).await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .map_err(|e| failure(e.to_string()))?;
        // The connection task ends when the response body is dropped or read
        // to the end; it must outlive this call for streaming bodies.
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost");
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|e| failure(e.to_string()))?
            .unwrap_or_default();
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let request = request
            .body(http_body_util::Full::new(hyper::body::Bytes::from(bytes)))
            .map_err(|e| failure(e.to_string()))?;
        sender
            .send_request(request)
            .await
            .map_err(connection_failure)
    }
}

fn failure(e: String) -> EngineError {
    EngineError::Other(format!("local daemon request: {e}"))
}

/// A socket can disappear after `connect` succeeds but before HTTP receives a
/// response. Report that race the same way as a refused connection.
fn connection_failure(error: hyper::Error) -> EngineError {
    use std::error::Error as _;
    if error.is_closed() || error.is_incomplete_message() || error.is_canceled() {
        return EngineError::Other(NO_DAEMON.into());
    }
    let mut source = error.source();
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::NotFound
            ) {
                return EngineError::Other(NO_DAEMON.into());
            }
        }
        source = cause.source();
    }
    failure(error.to_string())
}

// ---------------------------------------------------------------- Unix ----

/// Require `endpoint` to be a socket owned by this user with no group/other
/// bits, inside a directory owned by this user and closed to others.
#[cfg(unix)]
pub fn check_owner_socket(endpoint: &Path) -> Result<(), EngineError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let uid = nix::unistd::geteuid().as_raw();
    let refuse = |why: &str| {
        EngineError::Other(format!(
            "refusing local endpoint {}: {why}",
            endpoint.display()
        ))
    };
    let meta = match std::fs::symlink_metadata(endpoint) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(EngineError::Other(NO_DAEMON.into()))
        }
        Err(e) => return Err(refuse(&e.to_string())),
    };
    if !meta.file_type().is_socket() {
        return Err(refuse("not a socket"));
    }
    if meta.uid() != uid {
        return Err(refuse("owned by another user"));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(refuse("accessible to other users"));
    }
    let parent = endpoint
        .parent()
        .ok_or_else(|| refuse("no parent directory"))?;
    let dir = std::fs::symlink_metadata(parent).map_err(|e| refuse(&e.to_string()))?;
    if !dir.file_type().is_dir() || dir.uid() != uid || dir.mode() & 0o077 != 0 {
        return Err(refuse("its directory is not private to this user"));
    }
    Ok(())
}

#[cfg(unix)]
async fn connect(endpoint: &Path) -> Result<tokio::net::UnixStream, EngineError> {
    check_owner_socket(endpoint)?;
    let stream = tokio::net::UnixStream::connect(endpoint)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                EngineError::Other(NO_DAEMON.into())
            }
            _ => failure(e.to_string()),
        })?;
    let uid = nix::unistd::geteuid().as_raw();
    if !stream.peer_cred().is_ok_and(|peer| peer.uid() == uid) {
        return Err(EngineError::Other(format!(
            "refusing local endpoint {}: the server is not running as this user",
            endpoint.display()
        )));
    }
    Ok(stream)
}

// ------------------------------------------------------------- Windows ----

#[cfg(windows)]
async fn connect(
    endpoint: &Path,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient, EngineError> {
    const ERROR_PIPE_BUSY: i32 = 231;
    loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(endpoint) {
            Ok(client) => return Ok(client),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(EngineError::Other(NO_DAEMON.into()))
            }
            Err(e) => return Err(failure(e.to_string())),
        }
    }
}

/// Creates one owner-only pipe instance (`first` = must be the first).
#[cfg(windows)]
pub type OwnerPipeFactory =
    fn(&Path, bool) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer>;
/// After `connect`: whether the caller's token user is this process's user.
#[cfg(windows)]
pub type PipeClientVerifier =
    fn(&tokio::net::windows::named_pipe::NamedPipeServer) -> std::io::Result<bool>;

#[cfg(windows)]
static OWNER_PIPE: std::sync::OnceLock<(OwnerPipeFactory, PipeClientVerifier)> =
    std::sync::OnceLock::new();

/// The binary supplies the native ACL and identity shims; library consumers
/// fail closed without them.
#[cfg(windows)]
pub fn install_owner_pipe(factory: OwnerPipeFactory, verifier: PipeClientVerifier) {
    let _ = OWNER_PIPE.set((factory, verifier));
}

#[cfg(windows)]
fn shims() -> Result<&'static (OwnerPipeFactory, PipeClientVerifier), EngineError> {
    OWNER_PIPE.get().ok_or_else(|| {
        EngineError::Other(
            "owner-only named pipe support is not installed in this build; \
             the local listener cannot be secured"
                .into(),
        )
    })
}

#[cfg(windows)]
pub fn owner_pipe(
    endpoint: &Path,
    first: bool,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, EngineError> {
    (shims()?.0)(endpoint, first).map_err(|e| EngineError::Other(e.to_string()))
}

/// Fail-closed caller identity check: any error is "not the owner".
#[cfg(windows)]
pub fn verify_pipe_client(server: &tokio::net::windows::named_pipe::NamedPipeServer) -> bool {
    shims().is_ok_and(|(_, verify)| verify(server).unwrap_or(false))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn endpoint_name_must_match() {
        let expected = Path::new("/home/u/.awman/data/daemon/api.sock");
        assert!(endpoint_matches(expected, expected));
        assert!(endpoint_matches(
            Path::new("/run/user/1/api.sock"),
            expected
        ));
        assert!(!endpoint_matches(Path::new("/tmp/other.sock"), expected));
        assert!(!endpoint_matches(Path::new("api.sock"), expected));
    }
}
