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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_workdirs_dedupes_overlapping_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let s = tmp.path().to_str().unwrap().to_string();
        let merged = resolve_workdirs(std::slice::from_ref(&s), std::slice::from_ref(&s)).unwrap();
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn resolve_workdirs_errors_on_missing_path() {
        let err = resolve_workdirs(&["/no/such/path".into()], &[]).unwrap_err();
        assert!(matches!(err, CommandError::ApiWorkdirNotFound { .. }));
    }

    #[test]
    fn resolve_workdirs_merges_cli_and_config() {
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        let cli = vec![tmp_a.path().to_str().unwrap().to_string()];
        let cfg = vec![tmp_b.path().to_str().unwrap().to_string()];
        let merged = resolve_workdirs(&cli, &cfg).unwrap();
        assert_eq!(merged.len(), 2, "must contain both cli and config entries");
    }
}

#[cfg(test)]
mod workdir_root_tests {
    //! `resolve_workdir_roots` (WI 0134): merge precedence, startup refusal of
    //! roots that are or sit inside a repository, and missing-root warnings.
    use super::*;

    #[derive(Default)]
    struct WarnSink {
        warnings: Vec<String>,
    }
    impl UserMessageSink for WarnSink {
        fn write_message(&mut self, msg: UserMessage) {
            if matches!(msg.level, MessageLevel::Warning) {
                self.warnings.push(msg.text);
            }
        }
        fn replay_queued(&mut self) {}
    }

    fn git_init(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        let out = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    fn s(p: &std::path::Path) -> String {
        p.to_str().unwrap().to_string()
    }

    #[test]
    fn roots_merge_cli_first_then_config_and_dedupe() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (ca, cb) = (
            a.path().canonicalize().unwrap(),
            b.path().canonicalize().unwrap(),
        );
        let git = crate::engine::git::GitEngine::new();
        let mut sink = WarnSink::default();

        let merged =
            resolve_workdir_roots(&[s(a.path())], &[s(b.path()), s(a.path())], &git, &mut sink)
                .unwrap();
        assert_eq!(merged, vec![ca, cb], "CLI first, config second, deduped");
        assert!(sink.warnings.is_empty());
    }

    #[test]
    fn roots_config_only_and_cli_only_both_work() {
        let a = tempfile::tempdir().unwrap();
        let git = crate::engine::git::GitEngine::new();
        let mut sink = WarnSink::default();
        let ca = a.path().canonicalize().unwrap();
        assert_eq!(
            resolve_workdir_roots(&[s(a.path())], &[], &git, &mut sink).unwrap(),
            vec![ca.clone()]
        );
        assert_eq!(
            resolve_workdir_roots(&[], &[s(a.path())], &git, &mut sink).unwrap(),
            vec![ca]
        );
        assert!(resolve_workdir_roots(&[], &[], &git, &mut sink)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn missing_root_warns_and_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().canonicalize().unwrap().join("not-yet");
        let git = crate::engine::git::GitEngine::new();
        let mut sink = WarnSink::default();

        let roots = resolve_workdir_roots(&[s(&missing)], &[], &git, &mut sink).unwrap();
        assert_eq!(roots, vec![missing.clone()]);
        assert_eq!(sink.warnings.len(), 1);
        assert!(
            sink.warnings[0].contains("does not exist yet"),
            "{:?}",
            sink.warnings
        );
        assert!(sink.warnings[0].contains(&s(&missing)));
    }

    #[test]
    fn root_that_is_a_git_repo_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        git_init(&repo);
        let git = crate::engine::git::GitEngine::new();
        let err =
            resolve_workdir_roots(&[s(&repo)], &[], &git, &mut WarnSink::default()).unwrap_err();
        match err {
            CommandError::ApiWorkdirRootInvalid { root, git_root } => {
                assert_eq!(root, repo);
                assert_eq!(git_root, repo);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn root_inside_a_git_repo_is_refused_from_either_source() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        git_init(&repo);
        let inside = repo.join("sub");
        std::fs::create_dir_all(&inside).unwrap();
        let git = crate::engine::git::GitEngine::new();
        for (cli, cfg) in [(vec![s(&inside)], vec![]), (vec![], vec![s(&inside)])] {
            let err =
                resolve_workdir_roots(&cli, &cfg, &git, &mut WarnSink::default()).unwrap_err();
            assert!(
                matches!(&err, CommandError::ApiWorkdirRootInvalid { git_root, .. } if *git_root == repo),
                "got {err:?}"
            );
            assert_eq!(err.exit_code(), 2);
        }
    }

    #[test]
    fn repo_refused_as_root_is_still_usable_as_exact_workdir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap().join("repo");
        git_init(&repo);
        let git = crate::engine::git::GitEngine::new();
        assert!(resolve_workdir_roots(&[s(&repo)], &[], &git, &mut WarnSink::default()).is_err());
        assert_eq!(resolve_workdirs(&[s(&repo)], &[]).unwrap(), vec![repo]);
    }

    #[test]
    fn exact_workdirs_cli_entries_precede_config_entries() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let merged = resolve_workdirs(&[s(a.path())], &[s(b.path())]).unwrap();
        assert_eq!(
            merged,
            vec![
                a.path().canonicalize().unwrap(),
                b.path().canonicalize().unwrap()
            ]
        );
    }
    #[test]
    fn exact_mount_cannot_contain_a_root_writable_by_another_session() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        assert!(
            validate_root_mount_overlap(&[tmp.path().into()], std::slice::from_ref(&root)).is_err()
        );
        assert!(validate_root_mount_overlap(
            std::slice::from_ref(&root),
            std::slice::from_ref(&root)
        )
        .is_err());
        assert!(validate_root_mount_overlap(&[root.join("child")], &[root]).is_ok());
    }

    #[test]
    fn startup_git_errors_other_than_non_repository_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join(".git/config"), "[invalid config").unwrap();
        let mut sink = crate::data::message::RecordingMessageSink::new();
        let error = resolve_workdir_roots(
            &[s(tmp.path())],
            &[],
            &crate::engine::git::GitEngine::new(),
            &mut sink,
        )
        .unwrap_err();
        assert!(error.to_string().contains("git root discovery failed"));
    }
}
