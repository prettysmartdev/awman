//! Owner-authenticated resource listener. TCP bearer authentication is separate.
use crate::command::error::CommandError;
use axum::Router;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

#[derive(Clone, Copy)]
pub(super) struct OwnerAuthenticated;

/// Aborts the accept loop and removes its socket when the TCP server exits.
pub struct LocalListener {
    task: tokio::task::JoinHandle<()>,
    #[cfg(unix)]
    endpoint: PathBuf,
}
impl Drop for LocalListener {
    fn drop(&mut self) {
        self.task.abort();
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

pub fn start(endpoint: &Path, router: Router) -> Result<LocalListener, CommandError> {
    let router = router.layer(axum::Extension(OwnerAuthenticated));
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        let directory = endpoint.parent().expect("local endpoint parent");
        crate::data::fs::private_files::create_private_dir(directory)?;
        if let Ok(meta) = std::fs::symlink_metadata(endpoint) {
            if !meta.file_type().is_socket() || meta.uid() != nix::unistd::geteuid().as_raw() {
                return Err(CommandError::Other(
                    "refusing a local endpoint not owned by this user".into(),
                ));
            }
            std::fs::remove_file(endpoint).map_err(|e| CommandError::Other(e.to_string()))?;
        }
        let listener = tokio::net::UnixListener::bind(endpoint)
            .map_err(|e| CommandError::Other(e.to_string()))?;
        std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| CommandError::Other(e.to_string()))?;
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                if !stream
                    .peer_cred()
                    .is_ok_and(|peer| peer.uid() == nix::unistd::geteuid().as_raw())
                {
                    continue;
                }
                serve_connection(stream, router.clone());
            }
        });
        Ok(LocalListener {
            task,
            #[cfg(unix)]
            endpoint: endpoint.into(),
        })
    }
    #[cfg(windows)]
    {
        let mut listener = crate::engine::remote::local::owner_pipe(endpoint, true)?;
        let name = endpoint.to_path_buf();
        let task = tokio::spawn(async move {
            loop {
                if listener.connect().await.is_err() {
                    break;
                }
                let next = match crate::engine::remote::local::owner_pipe(&name, false) {
                    Ok(next) => next,
                    Err(_) => break,
                };
                serve_connection(listener, router.clone());
                listener = next;
            }
        });
        Ok(LocalListener {
            task,
            #[cfg(unix)]
            endpoint: endpoint.into(),
        })
    }
}
fn serve_connection<S>(stream: S, router: Router)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let service = hyper_util::service::TowerToHyperService::new(router);
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
            .await;
    });
}
