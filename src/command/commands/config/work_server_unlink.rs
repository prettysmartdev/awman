//! `awman config unset workServers.<name>`: what happens to the stored token.
//!
//! The token file is state that goes with the config entry. If the local
//! daemon still has leases running for that server, the token must outlive
//! them (a running lease may need to refresh its access token to report its
//! result), so deletion is *deferred* to the daemon: it stops claiming as
//! soon as it reloads the config, and calls [`finish_deferred_unlink`] when
//! its last lease for that server ends.
//!
//! The daemon side plugs in through [`DaemonReloader::running_leases`].

use super::live::DaemonReloader;
use crate::data::config::file::{global_config_path, ConfigFile, FileKind};
use crate::data::error::DataError;
use crate::data::fs::DataPaths;
use crate::data::session::Session;
use crate::data::worker::link_store::LinkStore;

/// What became of the token file after the config entry was removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenDisposition {
    /// The token file is gone (or never existed).
    Removed,
    /// The running daemon still has `leases` leases for this server; the
    /// daemon deletes the token when the last one ends.
    Deferred { leases: usize },
    /// A daemon is running but could not say whether leases exist; the token
    /// is kept and the daemon's retirement of the server deletes it.
    DeferredUnknown(String),
}

/// Decide whether the token can go now, and delete it if so.
///
/// Call this *after* the config file was written and the running daemon was
/// asked to reload, so the daemon has already stopped claiming for `name`
/// and the lease count can only fall.
pub fn dispose_token(
    reloader: &dyn DaemonReloader,
    session: &Session,
    name: &str,
) -> Result<TokenDisposition, DataError> {
    let paths = DataPaths::from_env(session.env())?;
    // Foreground workers can outlive a disabled or absent daemon worker.
    // Their durable leases use the same database and need the same drain.
    if paths.db_path().exists() {
        let store = std::sync::Arc::new(crate::data::fs::api_db::SqliteSessionStore::open_at(
            &paths.db_path(),
        )?);
        let leases = crate::data::worker::lease_store::LeaseStore::open(store)?
            .active_for_server(name)?
            .len();
        if leases > 0 {
            LinkStore::new(&paths).defer_remove(name)?;
            return Ok(TokenDisposition::Deferred { leases });
        }
    }
    if let Some(daemon) = reloader.running(session) {
        match reloader.running_leases(&daemon, name) {
            Ok(0) => {}
            Ok(leases) => {
                LinkStore::new(&DataPaths::from_env(session.env())?).defer_remove(name)?;
                return Ok(TokenDisposition::Deferred { leases });
            }
            Err(reason) => {
                LinkStore::new(&DataPaths::from_env(session.env())?).defer_remove(name)?;
                return Ok(TokenDisposition::DeferredUnknown(reason));
            }
        }
    }
    LinkStore::new(&paths).remove(name)?;
    Ok(TokenDisposition::Removed)
}

/// Daemon hook: the last lease of a retired server ended; delete its token.
///
/// Refuses (returns `Ok(false)`) when the server was linked again in the
/// meantime, so a re-link during the drain keeps its new token.
pub fn finish_deferred_unlink(session: &Session, name: &str) -> Result<bool, DataError> {
    let config = ConfigFile::load(&global_config_path(session.env())?, FileKind::Global)?;
    let folded = crate::data::config::fields::fold_map_name(name);
    if config
        .work_servers
        .iter()
        .flatten()
        .any(|(existing, _)| crate::data::config::fields::fold_map_name(existing) == folded)
    {
        return Ok(false);
    }
    LinkStore::new(&DataPaths::from_env(session.env())?).remove(name)?;
    Ok(true)
}

/// The message shown for a deferred removal.
pub fn deferred_message(name: &str, disposition: &TokenDisposition) -> Option<String> {
    match disposition {
        TokenDisposition::Removed => None,
        TokenDisposition::Deferred { leases } => Some(format!(
            "work server `{name}` has {leases} running lease(s); its token is deleted when they finish"
        )),
        TokenDisposition::DeferredUnknown(reason) => Some(format!(
            "could not ask the daemon about running leases for `{name}` ({reason}); its token is kept until the daemon retires the server"
        )),
    }
}
