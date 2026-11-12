//! The daemon's local listener: HTTP over an owner-only Unix socket or named
//! pipe. There is no loopback TCP fallback and no local bearer credential;
//! a connection is the owner's because the transport proves it.
//!
//! Unix: the socket lives in a private (0700, owner) directory, is chmod 0600
//! and every accepted connection's peer UID must equal this process's
//! effective UID. Windows: the pipe has a protected owner-only DACL, rejects
//! remote clients, and every caller's token user must equal this process's.
//! Any failure to create or secure the endpoint is an error (fail closed).
use crate::command::error::CommandError;
use crate::data::fs::daemon_sidecar::SidecarTransport;
use crate::engine::error::EngineError;
use axum::Router;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

/// The transport kind the local listener bound.
pub use crate::data::fs::daemon_sidecar::SidecarTransportKind as LocalTransport;

use crate::engine::daemon::DaemonClaim;

/// Request extension marking a request that arrived over the local listener.
#[derive(Clone, Copy)]
pub struct OwnerAuthenticated;

/// What was bound, for `server.json` and `/v1/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBind {
    pub transport: LocalTransport,
    pub address: String,
}

impl LocalBind {
    pub fn sidecar_transport(&self) -> SidecarTransport {
        SidecarTransport {
            kind: self.transport,
            address: self.address.clone(),
        }
    }
}

/// The running accept loop. Dropping it stops accepting and removes the
/// socket file (only if it is still the one this listener bound).
pub struct LocalListener {
    task: tokio::task::JoinHandle<()>,
    connections: tokio_util::task::TaskTracker,
    stop: tokio_util::sync::CancellationToken,
    #[cfg(unix)]
    endpoint: PathBuf,
    #[cfg(unix)]
    identity: (u64, u64),
}

impl LocalListener {
    /// Stop admission, flush active responses and close keep-alive connections.
    pub async fn shutdown(&self) {
        self.task.abort();
        self.stop.cancel();
        self.connections.close();
        // A peer that stops reading cannot hold daemon shutdown indefinitely.
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(10), self.connections.wait()).await;
    }
}

impl Drop for LocalListener {
    fn drop(&mut self) {
        self.task.abort();
        self.stop.cancel();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if std::fs::symlink_metadata(&self.endpoint)
                .is_ok_and(|meta| (meta.dev(), meta.ino()) == self.identity)
            {
                let _ = std::fs::remove_file(&self.endpoint);
            }
        }
    }
}

/// Bind while the startup lock is held: a stale socket is removed through
/// `claim` only (never a foreign file, never one a live server answers on),
/// then the private directory is created, the endpoint bound and secured, and
/// the accept loop started.
///
pub fn start_claimed(
    claim: &DaemonClaim,
    endpoint: &Path,
    router: Router,
) -> Result<(LocalListener, LocalBind), CommandError> {
    bind(
        endpoint,
        router,
        |path| claim.remove_stale_endpoint(path),
        owner_uid(),
    )
}

#[cfg(unix)]
fn owner_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}
#[cfg(not(unix))]
fn owner_uid() -> u32 {
    0
}

/// The peer rule: a connection is served only when its UID is known and is
/// the daemon owner's.
pub fn peer_is_owner(peer_uid: Option<u32>, owner: u32) -> bool {
    peer_uid == Some(owner)
}

#[cfg(unix)]
fn bind(
    endpoint: &Path,
    router: Router,
    remove_stale: impl FnOnce(&Path) -> Result<(), EngineError>,
    owner: u32,
) -> Result<(LocalListener, LocalBind), CommandError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let router = router.layer(axum::Extension(OwnerAuthenticated));
    let fail = |what: &str, e: &dyn std::fmt::Display| {
        CommandError::Other(format!(
            "could not secure the local listener at {}: {what}: {e}",
            endpoint.display()
        ))
    };
    let directory = endpoint
        .parent()
        .ok_or_else(|| fail("endpoint", &"has no parent directory"))?;
    crate::data::fs::private_files::create_private_dir(directory)?;
    if let Ok(meta) = std::fs::symlink_metadata(endpoint) {
        if !meta.file_type().is_socket() || meta.uid() != nix::unistd::geteuid().as_raw() {
            return Err(CommandError::Other(format!(
                "refusing to replace {}: it is not a socket owned by this user; remove it and retry",
                endpoint.display()
            )));
        }
        if std::os::unix::net::UnixStream::connect(endpoint).is_ok() {
            return Err(CommandError::Other(format!(
                "another process is already serving on {}; stop the running daemon \
                 (`awman daemon stop`) and retry",
                endpoint.display()
            )));
        }
        remove_stale(endpoint)?;
    }
    let listener = tokio::net::UnixListener::bind(endpoint).map_err(|e| fail("bind", &e))?;
    std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| fail("chmod 0600", &e))?;
    let meta = std::fs::symlink_metadata(endpoint).map_err(|e| fail("stat", &e))?;
    let identity = (meta.dev(), meta.ino());
    let connections = tokio_util::task::TaskTracker::new();
    let stop = tokio_util::sync::CancellationToken::new();
    let tasks = connections.clone();
    let connection_stop = stop.clone();
    let task = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    tracing::warn!(error = %e, "local listener accept failed");
                    continue;
                }
            };
            let peer = stream.peer_cred().ok().map(|cred| cred.uid());
            if !peer_is_owner(peer, owner) {
                tracing::warn!(?peer, "refused a local connection from another user");
                continue;
            }
            serve_connection(stream, router.clone(), &tasks, connection_stop.clone());
        }
    });
    let listener = LocalListener {
        task,
        connections,
        stop,
        endpoint: endpoint.to_path_buf(),
        identity,
    };
    let bound = LocalBind {
        transport: LocalTransport::UnixSocket,
        address: endpoint.display().to_string(),
    };
    Ok((listener, bound))
}

#[cfg(windows)]
fn bind(
    endpoint: &Path,
    router: Router,
    _remove_stale: impl FnOnce(&Path) -> Result<(), EngineError>,
    _owner: u32,
) -> Result<(LocalListener, LocalBind), CommandError> {
    use crate::engine::remote::local::{owner_pipe, verify_pipe_client};
    let router = router.layer(axum::Extension(OwnerAuthenticated));
    // Named pipes vanish with their server; there is no stale file to remove.
    // `first = true` fails if any instance exists, so a squatter is refused.
    let mut listener = owner_pipe(endpoint, true).map_err(|e| {
        CommandError::Other(format!(
            "could not create the owner-only named pipe {}: {e}. Another awman daemon may be \
             running (`awman daemon stop`), or this account cannot create secured pipes",
            endpoint.display()
        ))
    })?;
    let name = endpoint.to_path_buf();
    let connections = tokio_util::task::TaskTracker::new();
    let stop = tokio_util::sync::CancellationToken::new();
    let tasks = connections.clone();
    let connection_stop = stop.clone();
    let task = tokio::spawn(async move {
        loop {
            if listener.connect().await.is_err() {
                break;
            }
            let next = match owner_pipe(&name, false) {
                Ok(next) => next,
                Err(e) => {
                    tracing::error!(error = %e, "could not create the next pipe instance");
                    break;
                }
            };
            if verify_pipe_client(&listener) {
                serve_connection(listener, router.clone(), &tasks, connection_stop.clone());
            } else {
                tracing::warn!("refused a named pipe connection from another user");
                let _ = listener.disconnect();
            }
            listener = next;
        }
    });
    let bound = LocalBind {
        transport: LocalTransport::NamedPipe,
        address: endpoint.display().to_string(),
    };
    Ok((
        LocalListener {
            task,
            connections,
            stop,
        },
        bound,
    ))
}

fn serve_connection<S>(
    stream: S,
    router: Router,
    tasks: &tokio_util::task::TaskTracker,
    stop: tokio_util::sync::CancellationToken,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tasks.spawn(async move {
        let service = hyper_util::service::TowerToHyperService::new(router);
        let connection = hyper::server::conn::http1::Builder::new()
            .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
            // `/v1/terminals/{id}` upgrades to a WebSocket (WI 0141).
            .with_upgrades();
        tokio::pin!(connection);
        tokio::select! {
            _ = &mut connection => {}
            _ = stop.cancelled() => {
                connection.as_mut().graceful_shutdown();
                let _ = connection.await;
            }
        }
    });
}
