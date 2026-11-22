//! The worker feature's internals (queue worker, admission, session setup,
//! cancellation) and the workdir allowlist helpers the daemon validates at
//! startup. The process lifecycle is `awman daemon` (`commands::daemon`).

use std::net::IpAddr;
use std::path::PathBuf;

use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};

pub mod admission;
pub mod cancel;
pub mod command_log_stream;
pub mod event_bus;
pub mod inline_work_item;
pub mod owned_runtime;
pub mod push_back;
pub mod push_job;
pub mod queue_worker;
pub mod runtime;
pub mod session_setup;

/// The one bearer-auth decision, owned by Layer 1. Re-exported here because
/// `WorkerServices` hands it to the router; Layer 2 holds no copy of it
/// (WI 0114 F-14).
pub use cancel::{CancelOutcome, CommandCanceller, TokenRegistry};
pub use runtime::{ApiSessionLifecycle, CloseOutcome, SetupReadiness, WorkerServices};

/// The worker feature's construction inputs (`WorkerServices::from_core`).
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub port: u16,
    pub bind_ip: IpAddr,
    pub workdirs: Vec<PathBuf>,
    pub workdir_roots: Vec<PathBuf>,
    /// The environment of the invocation that started the server. Every API
    /// session inherits it, so a relocated `AWMAN_HOME` (and the global
    /// builtin network policy in it) applies to API sessions too.
    pub env: crate::data::config::env::EnvSnapshot,
}

/// Resolve the merged-and-validated workdir allowlist (per spec §6.4a).
/// Concatenate CLI-supplied workdirs and config workdirs, canonicalize,
/// deduplicate, and reject missing paths.
pub fn resolve_workdirs(
    cli: &[String],
    config: &[String],
) -> Result<Vec<std::path::PathBuf>, CommandError> {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<std::path::PathBuf> = BTreeSet::new();
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for raw in cli.iter().chain(config.iter()) {
        let path = std::path::PathBuf::from(raw);
        if !path.exists() {
            return Err(CommandError::ApiWorkdirNotFound { path });
        }
        let canon = path.canonicalize().unwrap_or(path);
        if seen.insert(canon.clone()) {
            out.push(canon);
        }
    }
    Ok(out)
}

/// Roots must remain host-owned: an ancestor mount could replace another
/// session's child directory with a symlink after validation.
pub fn validate_root_mount_overlap(
    workdirs: &[PathBuf],
    roots: &[PathBuf],
) -> Result<(), CommandError> {
    for workdir in workdirs {
        if roots.iter().any(|root| root.starts_with(workdir)) {
            return Err(CommandError::Other(format!("API exact workdir '{}' contains a workdir root; roots and their ancestors must not be mounted by another session", workdir.display())));
        }
    }
    Ok(())
}

/// Merge CLI and global-config roots, rejecting roots at or inside a git
/// repository. Missing roots are kept for request-time canonicalization.
pub fn resolve_workdir_roots(
    cli: &[String],
    config: &[String],
    git: &crate::engine::git::GitEngine,
    sink: &mut dyn UserMessageSink,
) -> Result<Vec<PathBuf>, CommandError> {
    use std::collections::BTreeSet;

    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for raw in cli.iter().chain(config.iter()) {
        let path = PathBuf::from(raw);
        let root = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !seen.insert(root.clone()) {
            continue;
        }
        if !root.exists() {
            // Anchor relative entries now so they still refer to the same
            // place when a later request runs, even if the CWD changes.
            let root = if root.is_absolute() {
                root
            } else {
                std::env::current_dir()
                    .map_err(|e| {
                        CommandError::Other(format!("cannot determine working directory: {e}"))
                    })?
                    .join(root)
            };
            sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "API workdir root does not exist yet: {}; it is allowed once it is created",
                    root.display(),
                ),
            });
            out.push(root);
        } else {
            match git.resolve_root(&root) {
                Ok(git_root) => return Err(CommandError::ApiWorkdirRootInvalid { root, git_root }),
                Err(crate::engine::error::EngineError::Data(
                    crate::data::error::DataError::GitRootNotFound { .. },
                )) => {}
                Err(error) => return Err(error.into()),
            }
            out.push(root);
        }
    }
    Ok(out)
}
