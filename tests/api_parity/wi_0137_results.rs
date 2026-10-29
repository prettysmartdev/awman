//! WI 0137: structured command results and the one final `command_status`.
//!
//! * `CommandError::code()` / `stage()` — pure table tests over every variant.
//! * `GitEngine::command_result` — real `git` in a temp repo, no server.
//! * Everything else drives the shipped `awman api start` binary with the fake
//!   `docker` of `helpers/api_process.rs` (no container runtime, no network
//!   beyond loopback). Tests that need `git` carry `real_git`; the ones that
//!   need only loopback HTTP carry `real_network`, so `make test-fast` skips
//!   both.
//!
//! Outputs, translators, attribution, the workflow view and the `?step=`
//! filter are covered by `wi_0137_surface.rs`.

use awman::command::error::{failure_stages, CommandError};
use awman::engine::error::EngineError;

// ─── CommandError::code() / stage() ──────────────────────────────────────────

/// One value of every `CommandError` variant, so a new variant cannot ship
/// without a code and a stage: the witness below fails to compile until the
/// variant is added to both it and `every_variant()`.
fn every_variant() -> Vec<CommandError> {
    use awman::command::session_create::WorkdirRefusal;
    use awman::data::workflow_prerequisites::{
        PrerequisiteOrigin, PrerequisitePhase, WorkflowPrerequisiteReport,
    };
    let command = || vec!["exec".to_string(), "prompt".to_string()];
    vec![
        CommandError::Engine(EngineError::Other("boom".into())),
        CommandError::Data(awman::data::error::DataError::Other("boom".into())),
        CommandError::UnknownCommand {
            path: command(),
            suggestions: vec![],
        },
        CommandError::NotAvailableForFrontend {
            command: "x".into(),
            frontend: "api".into(),
        },
        CommandError::UnknownFlag {
            command: command(),
            flag: "--x".into(),
        },
        CommandError::MissingRequiredFlag {
            command: command(),
            flag: "--x".into(),
        },
        CommandError::MissingRequiredArgument {
            command: command(),
            argument: "prompt".into(),
        },
        CommandError::UnexpectedArgument {
            command: command(),
            argument: "x".into(),
        },
        CommandError::MutuallyExclusive {
            command: command(),
            a: "a".into(),
            b: "b".into(),
        },
        CommandError::InvalidFlagValue {
            command: command(),
            flag: "--x".into(),
            reason: "r".into(),
        },
        CommandError::InvalidArgumentValue {
            command: command(),
            argument: "a".into(),
            reason: "r".into(),
        },
        CommandError::CommandBoxParse("x".into()),
        CommandError::Aborted,
        CommandError::MergeConflict {
            branch: "b".into(),
            worktree_path: "/w".into(),
            git_root: "/r".into(),
        },
        CommandError::MergeTargetDirty {
            target_branch: "main".into(),
            branch: "b".into(),
            worktree_path: "/w".into(),
        },
        CommandError::WorktreeExists {
            path: "/w".into(),
            branch: "b".into(),
        },
        CommandError::MissingRemoteAddress,
        CommandError::MissingApiKey,
        CommandError::RemoteTimeout,
        CommandError::RemoteConnectionRefused("refused".into()),
        CommandError::RemoteHttpStatus {
            status: 500,
            body: "b".into(),
        },
        CommandError::MalformedSseEvent("x".into()),
        CommandError::RemoteTransport("x".into()),
        CommandError::ApiWorkdirNotFound { path: "/w".into() },
        CommandError::ApiWorkdirRootInvalid {
            root: "/r".into(),
            git_root: "/g".into(),
        },
        CommandError::ApiServerAlreadyRunning { pid: 1 },
        CommandError::ApiServerNotRunning,
        CommandError::ApiServerAuthMissing,
        CommandError::SquadKeyMissing,
        CommandError::SessionInvalidType { got: "x".into() },
        CommandError::SessionWorkdirRequired,
        CommandError::SessionWorkdirUnresolvable { path: "/p".into() },
        CommandError::SessionWorkdirNotAllowed {
            requested: "/p".into(),
            rule: WorkdirRefusal::NotListed,
            allowed: vec![],
            roots: vec![],
        },
        CommandError::SessionRepoUrlRequired,
        CommandError::SessionRepoUrlEmpty,
        CommandError::SessionRepoUrlInvalidScheme { url: "u".into() },
        CommandError::SessionFieldRemoteOnly { field: "push" },
        CommandError::SessionFieldRequiresBranch { field: "push" },
        CommandError::SessionRefInvalid {
            field: "branch",
            value: "v".into(),
            reason: "r".into(),
        },
        CommandError::RemoteSessionMissing,
        CommandError::RemoteSessionKillFailed {
            session_id: "s".into(),
            reason: "r".into(),
        },
        CommandError::WorkItemNotFound { number: 1 },
        CommandError::SpecTemplateMissing { path: "/p".into() },
        CommandError::InvalidOverlaySpec {
            spec: "s".into(),
            reason: "r".into(),
        },
        CommandError::UnknownConfigField {
            name: "n".into(),
            suggestions: "s".into(),
        },
        CommandError::InteractiveInputUnavailable {
            prompt: "prompt".into(),
        },
        CommandError::WorkflowFileNotFound { path: "/w".into() },
        CommandError::WorkflowPrerequisitesUnresolved {
            report: Box::new(WorkflowPrerequisiteReport {
                phase: PrerequisitePhase::BeforeTeardown,
                origin: PrerequisiteOrigin::AssumedAll,
                checks: vec![],
                advisories: vec![],
                notes: vec![],
            }),
        },
        CommandError::PrerequisiteWideningRefused { flag: "f".into() },
        CommandError::PrerequisiteFlagUnsupported { flag: "f".into() },
        CommandError::AgentNotFound {
            reference: "r".into(),
        },
        CommandError::AgentNoLongerRunning {
            reference: "r".into(),
        },
        CommandError::AgentNotRunning {
            reference: "r".into(),
        },
        CommandError::AgentReplaced {
            reference: "r".into(),
        },
        CommandError::AgentAmbiguous {
            reference: "r".into(),
            candidates: vec!["a".into(), "b".into()],
        },
        CommandError::AgentReferenceEmpty,
        CommandError::AgentOwnerGone {
            name: "n".into(),
            runtime: "r".into(),
        },
        CommandError::NotImplemented("x"),
        CommandError::Other("boom".into()),
        CommandError::Engine(EngineError::UnknownAgent {
            name: "pi".into(),
            first_party: vec![],
            custom: vec![],
            notes: vec![],
        }),
        CommandError::Engine(EngineError::AgentDefinitionUntrusted {
            name: "pi".into(),
            path: "/x".into(),
            sha256: "ab".into(),
        }),
    ]
}

/// Compile-time witness: lists every variant by name. Adding a variant to
/// `CommandError` breaks this match until `every_variant()` is extended too.
#[allow(dead_code)]
fn variant_witness(error: &CommandError) -> &'static str {
    match error {
        CommandError::Engine(_) => "Engine",
        CommandError::Data(_) => "Data",
        CommandError::UnknownCommand { .. } => "UnknownCommand",
        CommandError::NotAvailableForFrontend { .. } => "NotAvailableForFrontend",
        CommandError::UnknownFlag { .. } => "UnknownFlag",
        CommandError::MissingRequiredFlag { .. } => "MissingRequiredFlag",
        CommandError::MissingRequiredArgument { .. } => "MissingRequiredArgument",
        CommandError::UnexpectedArgument { .. } => "UnexpectedArgument",
        CommandError::MutuallyExclusive { .. } => "MutuallyExclusive",
        CommandError::InvalidFlagValue { .. } => "InvalidFlagValue",
        CommandError::InvalidArgumentValue { .. } => "InvalidArgumentValue",
        CommandError::CommandBoxParse(_) => "CommandBoxParse",
        CommandError::Aborted => "Aborted",
        CommandError::MergeConflict { .. } => "MergeConflict",
        CommandError::MergeTargetDirty { .. } => "MergeTargetDirty",
        CommandError::WorktreeExists { .. } => "WorktreeExists",
        CommandError::MissingRemoteAddress => "MissingRemoteAddress",
        CommandError::MissingApiKey => "MissingApiKey",
        CommandError::RemoteTimeout => "RemoteTimeout",
        CommandError::RemoteConnectionRefused(_) => "RemoteConnectionRefused",
        CommandError::RemoteHttpStatus { .. } => "RemoteHttpStatus",
        CommandError::MalformedSseEvent(_) => "MalformedSseEvent",
        CommandError::RemoteTransport(_) => "RemoteTransport",
        CommandError::ApiWorkdirNotFound { .. } => "ApiWorkdirNotFound",
        CommandError::ApiWorkdirRootInvalid { .. } => "ApiWorkdirRootInvalid",
        CommandError::ApiServerAlreadyRunning { .. } => "ApiServerAlreadyRunning",
        CommandError::ApiServerNotRunning => "ApiServerNotRunning",
        CommandError::ApiServerAuthMissing => "ApiServerAuthMissing",
        CommandError::SquadKeyMissing => "SquadKeyMissing",
        CommandError::SessionInvalidType { .. } => "SessionInvalidType",
        CommandError::SessionWorkdirRequired => "SessionWorkdirRequired",
        CommandError::SessionWorkdirUnresolvable { .. } => "SessionWorkdirUnresolvable",
        CommandError::SessionWorkdirNotAllowed { .. } => "SessionWorkdirNotAllowed",
        CommandError::SessionRepoUrlRequired => "SessionRepoUrlRequired",
        CommandError::SessionRepoUrlEmpty => "SessionRepoUrlEmpty",
        CommandError::SessionRepoUrlInvalidScheme { .. } => "SessionRepoUrlInvalidScheme",
        CommandError::SessionFieldRemoteOnly { .. } => "SessionFieldRemoteOnly",
        CommandError::SessionFieldRequiresBranch { .. } => "SessionFieldRequiresBranch",
        CommandError::SessionRefInvalid { .. } => "SessionRefInvalid",
        CommandError::RemoteSessionMissing => "RemoteSessionMissing",
        CommandError::RemoteSessionKillFailed { .. } => "RemoteSessionKillFailed",
        CommandError::WorkItemNotFound { .. } => "WorkItemNotFound",
        CommandError::SpecTemplateMissing { .. } => "SpecTemplateMissing",
        CommandError::InvalidOverlaySpec { .. } => "InvalidOverlaySpec",
        CommandError::UnknownConfigField { .. } => "UnknownConfigField",
        CommandError::InteractiveInputUnavailable { .. } => "InteractiveInputUnavailable",
        CommandError::WorkflowFileNotFound { .. } => "WorkflowFileNotFound",
        CommandError::WorkflowPrerequisitesUnresolved { .. } => "WorkflowPrerequisitesUnresolved",
        CommandError::PrerequisiteWideningRefused { .. } => "PrerequisiteWideningRefused",
        CommandError::PrerequisiteFlagUnsupported { .. } => "PrerequisiteFlagUnsupported",
        CommandError::AgentNotFound { .. } => "AgentNotFound",
        CommandError::AgentNoLongerRunning { .. } => "AgentNoLongerRunning",
        CommandError::AgentNotRunning { .. } => "AgentNotRunning",
        CommandError::AgentReplaced { .. } => "AgentReplaced",
        CommandError::AgentAmbiguous { .. } => "AgentAmbiguous",
        CommandError::AgentReferenceEmpty => "AgentReferenceEmpty",
        CommandError::AgentOwnerGone { .. } => "AgentOwnerGone",
        CommandError::NotImplemented(_) => "NotImplemented",
        CommandError::Other(_) => "Other",
    }
}

const ALLOWED_STAGES: [&str; 7] = [
    failure_stages::SETUP,
    failure_stages::PREFLIGHT,
    failure_stages::WORKFLOW_PREREQUISITES,
    failure_stages::STEP,
    failure_stages::TEARDOWN,
    failure_stages::MERGE,
    failure_stages::PUSH,
];

#[test]
fn every_command_error_variant_has_a_snake_case_code_and_a_known_stage() {
    let variants = every_variant();
    // Every variant is listed exactly once (the specialised engine errors at the
    // end are extra values of `Engine`).
    let mut names: Vec<&str> = variants.iter().map(variant_witness).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 59, "variant list drifted: {names:?}");
    for error in &variants {
        let code = error.code();
        assert!(!code.is_empty(), "{error}");
        assert!(
            code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{error}: {code} is not snake_case"
        );
        let stage = error.stage();
        assert!(
            ALLOWED_STAGES.contains(&stage),
            "{error}: unknown stage {stage:?}"
        );
    }
}

#[test]
fn command_error_codes_distinguish_the_variants_clients_act_on() {
    let code = |e: CommandError| e.code();
    assert_eq!(
        code(CommandError::WorktreeExists {
            path: "/w".into(),
            branch: "b".into()
        }),
        "worktree_exists"
    );
    assert_eq!(
        code(CommandError::MergeConflict {
            branch: "b".into(),
            worktree_path: "/w".into(),
            git_root: "/r".into()
        }),
        "merge_conflict"
    );
    assert_eq!(
        code(CommandError::MergeTargetDirty {
            target_branch: "m".into(),
            branch: "b".into(),
            worktree_path: "/w".into()
        }),
        "merge_target_dirty"
    );
    assert_eq!(code(CommandError::Aborted), "aborted");
    assert_eq!(
        code(CommandError::Engine(EngineError::UnknownAgent {
            name: "pi".into(),
            first_party: vec![],
            custom: vec![],
            notes: vec![]
        })),
        "unknown_agent"
    );
    assert_eq!(
        code(CommandError::Engine(
            EngineError::AgentDefinitionUntrusted {
                name: "pi".into(),
                path: "/x".into(),
                sha256: "ab".into()
            }
        )),
        "agent_untrusted"
    );
    // Class codes for everything else.
    assert_eq!(code(CommandError::SessionRepoUrlEmpty), "invalid_usage");
    assert_eq!(
        code(CommandError::WorkItemNotFound { number: 1 }),
        "not_found"
    );
    assert_eq!(code(CommandError::Other("x".into())), "internal");
}

#[test]
fn command_error_stages_follow_the_failure_path() {
    assert_eq!(
        CommandError::WorktreeExists {
            path: "/w".into(),
            branch: "b".into()
        }
        .stage(),
        "preflight"
    );
    assert_eq!(
        CommandError::MergeConflict {
            branch: "b".into(),
            worktree_path: "/w".into(),
            git_root: "/r".into()
        }
        .stage(),
        "merge"
    );
    assert_eq!(CommandError::Aborted.stage(), "step");
    assert_eq!(CommandError::Other("x".into()).stage(), "step");
    assert_eq!(
        CommandError::UnknownFlag {
            command: vec![],
            flag: "--x".into()
        }
        .stage(),
        "preflight"
    );
}

#[test]
fn failure_codes_for_non_command_errors_are_snake_case_and_distinct() {
    use awman::data::fs::api_db::failure_codes::*;
    let codes = [
        STEP_FAILED,
        TEARDOWN_FAILED,
        PUSH_FAILED,
        SETUP_FAILED,
        CANCELLED,
        ABORTED,
        PAUSED,
        INTERNAL,
    ];
    let mut sorted = codes.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), codes.len(), "duplicate failure code");
    for code in codes {
        assert!(
            code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{code}"
        );
    }
    assert_eq!(STEP_FAILED, "step_failed");
    assert_eq!(TEARDOWN_FAILED, "teardown_failed");
    assert_eq!(PUSH_FAILED, "push_failed");
    assert_eq!(SETUP_FAILED, "setup_failed");
    assert_eq!(CANCELLED, "cancelled");
}

// ─── Everything below needs a unix host (fake docker, git) ───────────────────

#[cfg(unix)]
mod checkout_facts {
    //! `GitEngine::command_result` against real repositories: the producer of
    //! `result.git.*`. No server involved.

    use std::path::Path;

    use awman::data::fs::api_db::{ChangedFileStatus, CHANGED_FILES_CAP};
    use awman::engine::git::GitEngine;
    use serde_json::json;

    use crate::git_daemon::git;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.name", "T"]);
        git(dir.path(), &["config", "user.email", "t@awman.test"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);
        dir
    }

    fn head(dir: &Path) -> String {
        git(dir, &["rev-parse", "HEAD"])
    }

    fn write_untracked(dir: &Path, count: usize) {
        std::fs::create_dir_all(dir.join("many")).unwrap();
        for i in 0..count {
            std::fs::write(dir.join("many").join(format!("f{i:04}.txt")), "x\n").unwrap();
        }
    }

    #[test]
    fn real_git_clean_checkout_reports_branch_head_and_no_changes() {
        crate::real_git_skip!();
        let dir = repo();
        let base = head(dir.path());
        let result = GitEngine::new()
            .command_result(dir.path(), Some(&base))
            .unwrap()
            .expect("a checkout");
        assert_eq!(result.branch.as_deref(), Some("main"));
        assert_eq!(result.base_sha.as_deref(), Some(base.as_str()));
        assert_eq!(result.head_sha.as_deref(), Some(base.as_str()));
        assert!(!result.dirty);
        assert!(result.changed_files.is_empty());
        assert!(!result.changed_files_truncated);
    }

    #[test]
    fn real_git_uncommitted_edits_are_compared_with_the_base_commit() {
        crate::real_git_skip!();
        let dir = repo();
        let base = head(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "n\n").unwrap();
        let result = GitEngine::new()
            .command_result(dir.path(), Some(&base))
            .unwrap()
            .unwrap();
        assert!(result.dirty);
        // HEAD did not move: the edits are only in the working tree.
        assert_eq!(result.head_sha.as_deref(), Some(base.as_str()));
        assert_eq!(
            serde_json::to_value(&result.changed_files).unwrap(),
            json!([
                {"path": "a.txt", "status": "modified", "added": 2, "deleted": 1},
                {"path": "new.txt", "status": "added", "added": null, "deleted": null},
            ])
        );
    }

    #[test]
    fn real_git_committed_changes_after_the_base_are_listed_and_the_checkout_is_clean() {
        crate::real_git_skip!();
        let dir = repo();
        let base = head(dir.path());
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "more"]);
        let result = GitEngine::new()
            .command_result(dir.path(), Some(&base))
            .unwrap()
            .unwrap();
        assert!(!result.dirty);
        assert_ne!(result.head_sha.as_deref(), Some(base.as_str()));
        assert_eq!(result.changed_files.len(), 1);
        assert_eq!(result.changed_files[0].path, "b.txt");
        assert_eq!(result.changed_files[0].status, ChangedFileStatus::Added);
        assert_eq!(result.changed_files[0].added, Some(1));
    }

    #[test]
    fn real_git_binary_files_report_null_counts() {
        crate::real_git_skip!();
        let dir = repo();
        std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2, 0, 255]).unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "blob"]);
        let base = head(dir.path());
        std::fs::write(dir.path().join("blob.bin"), [0u8, 9, 9, 0, 0, 7]).unwrap();
        std::fs::write(dir.path().join("fresh.bin"), [0u8, 0, 1]).unwrap();
        let result = GitEngine::new()
            .command_result(dir.path(), Some(&base))
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(&result.changed_files).unwrap(),
            json!([
                {"path": "blob.bin", "status": "modified", "added": null, "deleted": null},
                {"path": "fresh.bin", "status": "added", "added": null, "deleted": null},
            ])
        );
    }

    #[test]
    fn real_git_renames_deletions_and_odd_paths_keep_their_status() {
        crate::real_git_skip!();
        let dir = repo();
        std::fs::write(dir.path().join("gone.txt"), "bye\n").unwrap();
        std::fs::write(
            dir.path().join("old name.txt"),
            "a long enough body\nline2\nline3\n",
        )
        .unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "more"]);
        let base = head(dir.path());
        git(dir.path(), &["mv", "old name.txt", "new name.txt"]);
        std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
        std::fs::write(dir.path().join("tab\there.txt"), "t\n").unwrap();
        let result = GitEngine::new()
            .command_result(dir.path(), Some(&base))
            .unwrap()
            .unwrap();
        let by_path = |p: &str| {
            result
                .changed_files
                .iter()
                .find(|f| f.path == p)
                .unwrap_or_else(|| panic!("{p} missing from {:?}", result.changed_files))
        };
        let renamed = by_path("new name.txt");
        assert_eq!(renamed.status, ChangedFileStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("old name.txt"));
        assert_eq!(by_path("gone.txt").status, ChangedFileStatus::Deleted);
        assert_eq!(by_path("tab\there.txt").status, ChangedFileStatus::Added);
    }

    #[test]
    fn real_git_changed_files_are_capped_at_500_and_flag_truncation() {
        crate::real_git_skip!();
        assert_eq!(CHANGED_FILES_CAP, 500);
        let dir = repo();
        let base = head(dir.path());
        write_untracked(dir.path(), 650);
        let result = GitEngine::new()
            .command_result(dir.path(), Some(&base))
            .unwrap()
            .unwrap();
        assert_eq!(result.changed_files.len(), 500);
        assert!(result.changed_files_truncated);
        assert!(result.dirty);
    }

    #[test]
    fn real_git_exactly_500_changed_files_are_not_truncated() {
        crate::real_git_skip!();
        let dir = repo();
        let base = head(dir.path());
        write_untracked(dir.path(), 500);
        let result = GitEngine::new()
            .command_result(dir.path(), Some(&base))
            .unwrap()
            .unwrap();
        assert_eq!(result.changed_files.len(), 500);
        assert!(
            !result.changed_files_truncated,
            "nothing was left out, so the list is complete"
        );
        // One more tips it over.
        std::fs::write(dir.path().join("many/zzz.txt"), "x\n").unwrap();
        let result = GitEngine::new()
            .command_result(dir.path(), Some(&base))
            .unwrap()
            .unwrap();
        assert_eq!(result.changed_files.len(), 500);
        assert!(result.changed_files_truncated);
    }

    #[test]
    fn real_git_tracked_changes_count_towards_the_cap_before_untracked_files() {
        crate::real_git_skip!();
        let dir = repo();
        for i in 0..520 {
            std::fs::write(dir.path().join(format!("t{i:04}.txt")), "x\n").unwrap();
        }
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "many"]);
        let base = head(dir.path());
        for i in 0..520 {
            std::fs::write(dir.path().join(format!("t{i:04}.txt")), "y\n").unwrap();
        }
        std::fs::write(dir.path().join("untracked.txt"), "u\n").unwrap();
        let result = GitEngine::new()
            .command_result(dir.path(), Some(&base))
            .unwrap()
            .unwrap();
        assert_eq!(result.changed_files.len(), 500);
        assert!(result.changed_files_truncated);
        assert!(result
            .changed_files
            .iter()
            .all(|f| f.status == ChangedFileStatus::Modified));
    }

    #[test]
    fn real_git_collecting_facts_never_touches_the_index() {
        crate::real_git_skip!();
        let dir = repo();
        let base = head(dir.path());
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        std::fs::write(dir.path().join("untracked.txt"), "u\n").unwrap();
        let index = dir.path().join(".git/index");
        let before = std::fs::read(&index).unwrap();
        let _ = GitEngine::new()
            .command_result(dir.path(), Some(&base))
            .unwrap();
        assert_eq!(std::fs::read(&index).unwrap(), before, "index rewritten");
        assert_eq!(git(dir.path(), &["diff", "--cached", "--name-only"]), "");
        assert_eq!(
            git(dir.path(), &["status", "--porcelain"]),
            // `git()` trims the leading space of the first status line.
            "M a.txt\n?? untracked.txt"
        );
    }

    #[test]
    fn real_git_a_directory_outside_any_checkout_has_no_git_result() {
        crate::real_git_skip!();
        let dir = tempfile::tempdir().unwrap();
        let engine = GitEngine::new();
        assert_eq!(engine.command_base_sha(dir.path()), None);
        assert!(engine.command_result(dir.path(), None).unwrap().is_none());
    }
}

#[cfg(unix)]
mod server {
    //! The shipped `awman api start` with a fake `docker`: one test per way a
    //! command can end, each checking the row, the event stream and the result.

    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::{json, Value};

    use crate::api_process::{clone_path, AgentMode, ApiHome, ApiServer};
    use crate::git_daemon::{git, Origin};

    // ── helpers ──────────────────────────────────────────────────────────────

    /// A committed repo the server may open as a local session: the agent
    /// Dockerfiles it needs, a work item and any `workflows` (relative path →
    /// TOML). Allow-listed through the server's global config.
    struct LocalRepo {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl LocalRepo {
        fn new(home: &ApiHome, workflows: &[(&str, &str)]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().canonicalize().unwrap().join("repo");
            std::fs::create_dir_all(path.join(".awman")).unwrap();
            std::fs::create_dir_all(path.join("aspec/work-items")).unwrap();
            std::fs::write(path.join("Dockerfile.dev"), "FROM scratch\n").unwrap();
            std::fs::write(path.join(".awman/Dockerfile.claude"), "FROM scratch\n").unwrap();
            std::fs::write(
                path.join("aspec/work-items/0042-test.md"),
                "# Work item 42\n",
            )
            .unwrap();
            std::fs::write(path.join("shared.txt"), "line one\n").unwrap();
            for (name, body) in workflows {
                std::fs::write(path.join(name), body).unwrap();
            }
            git(&path, &["init", "-q", "-b", "main"]);
            git(&path, &["config", "user.name", "Fixture"]);
            git(&path, &["config", "user.email", "fixture@awman.test"]);
            git(&path, &["add", "-A"]);
            git(&path, &["commit", "-q", "-m", "base"]);
            let config = home.home().join(".config/awman");
            std::fs::create_dir_all(&config).unwrap();
            std::fs::write(
                config.join("config.json"),
                json!({"api": {"workDirs": [path.display().to_string()]}}).to_string(),
            )
            .unwrap();
            Self { _dir: dir, path }
        }

        fn head(&self) -> String {
            git(&self.path, &["rev-parse", "HEAD"])
        }

        async fn session(&self, server: &ApiServer) -> String {
            server
                .ready_session(json!({"session_type": "local", "workdir": self.path}))
                .await
        }
    }

    fn remote(url: &str) -> Value {
        json!({"session_type": "remote", "repo_url": url})
    }

    /// A repo URL whose clone fails fast.
    const BAD_REPO: &str = "git://127.0.0.1:1/missing.git";

    async fn http(
        server: &ApiServer,
        method: reqwest::Method,
        path: &str,
        session: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = reqwest::Client::new().request(method, format!("{}{path}", server.url));
        if let Some(session) = session {
            request = request.header("x-awman-session", session);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("request");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn submit(server: &ApiServer, session: &str, subcommand: &str, args: &[&str]) -> String {
        let (status, body) = http(
            server,
            reqwest::Method::POST,
            "/v1/commands",
            Some(session),
            Some(json!({"subcommand": subcommand, "args": args})),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        body["command_id"].as_str().unwrap().to_string()
    }

    async fn wait_status(server: &ApiServer, id: &str, wanted: &[&str]) -> Value {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let record = server.get(&format!("/v1/commands/{id}/status")).await;
            if wanted.contains(&record["status"].as_str().unwrap_or("")) {
                return record;
            }
            assert!(
                Instant::now() < deadline,
                "command {id} never reached {wanted:?}: {record}\n{}",
                server.log()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait_slow_agent(home: &ApiHome) {
        let pid = home.control().join("slow.pid");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !pid.exists() {
            assert!(Instant::now() < deadline, "slow agent never started");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The command's persisted events, once its `Done` is on disk. The row turns
    /// terminal just before the final pair is published, so poll for it.
    async fn events_of(record: &Value) -> Vec<Value> {
        let log =
            Path::new(record["log_path"].as_str().expect("log_path")).with_file_name("events.log");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains(r#""type":"Done""#) {
                return text
                    .lines()
                    .map(|line| {
                        serde_json::from_str(line)
                            .expect("every persisted event must be intact JSON")
                    })
                    .collect();
            }
            assert!(
                Instant::now() < deadline,
                "no Done event in {}: {text}",
                log.display()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn payload_type(event: &Value) -> &str {
        event["payload"]["type"].as_str().unwrap_or("")
    }

    /// Asserts the stream contract and returns the final `command_status`
    /// payload: exactly one, immediately before the single `Done`, carrying
    /// the status route's `result`, `exit_code` and (for done/error rows) status.
    fn assert_one_final_status(events: &[Value], record: &Value) -> Value {
        let statuses: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| payload_type(e) == "CommandStatus")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(statuses.len(), 1, "want one command_status: {events:#?}");
        let dones = events.iter().filter(|e| payload_type(e) == "Done").count();
        assert_eq!(dones, 1, "want one Done");
        assert_eq!(
            statuses[0],
            events.len() - 2,
            "command_status not just before Done"
        );
        assert_eq!(payload_type(events.last().unwrap()), "Done");
        let data = events[statuses[0]]["payload"]["data"].clone();
        assert_eq!(
            data["result"], record["result"],
            "event result != status route"
        );
        assert_eq!(data["exit_code"], record["exit_code"], "{data} vs {record}");
        assert_eq!(
            data["status"], record["result"]["outcome"],
            "event status != result.outcome: {data}"
        );
        data
    }

    // ── exec prompt: git facts for uncommitted edits ─────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_exec_prompt_leaves_uncommitted_edits_that_git_reports() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        let repo = LocalRepo::new(&home, &[]);
        let base = repo.head();
        let server = home.start().await;
        let id = repo.session(&server).await;

        let record = server
            .run_command(&id, "exec prompt", &["edit a file"])
            .await;

        assert_eq!(record["status"], "done", "{record}\n{}", server.log());
        let result = &record["result"];
        assert_eq!(result["outcome"], "done");
        assert!(result.get("failure").is_none(), "{result}");
        assert!(result.get("failed_steps").is_none(), "{result}");
        let git = &result["git"];
        assert_eq!(git["branch"], "main", "{result}");
        assert_eq!(git["base_sha"], base.as_str());
        assert_eq!(git["head_sha"], base.as_str(), "nothing was committed");
        assert_eq!(git["dirty"], true);
        assert_eq!(git["changed_files_truncated"], false);
        assert_eq!(
            git["changed_files"],
            json!([{"path": "agent-edit.txt", "status": "added", "added": null, "deleted": null}])
        );
        assert_eq!(result["outputs"], json!({"files": 0, "bytes": 0}));
        // Legacy fields are untouched by the additive ones.
        assert_eq!(result["exit_code"], 0);
        assert!(result.get("worktree_path").is_none(), "{result}");
        assert!(result.get("pushed").is_none(), "{result}");
        assert!(result.get("error").is_none(), "{result}");
        // The edit really is loose in the checkout.
        assert_eq!(git_status(&repo.path), "?? agent-edit.txt");

        let events = events_of(&record).await;
        let status = assert_one_final_status(&events, &record);
        assert_eq!(status["status"], "done");
    }

    fn git_status(path: &Path) -> String {
        git(path, &["status", "--porcelain"])
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_exec_prompt_without_edits_has_a_clean_git_result() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        home.set_agent(AgentMode::NoOp);
        let repo = LocalRepo::new(&home, &[]);
        let server = home.start().await;
        let id = repo.session(&server).await;

        let record = server.run_command(&id, "exec prompt", &["look only"]).await;

        assert_eq!(record["status"], "done", "{record}");
        let git = &record["result"]["git"];
        assert_eq!(git["dirty"], false, "{record}");
        assert_eq!(git["changed_files"], json!([]));
        assert_eq!(git["base_sha"], git["head_sha"]);
        assert_one_final_status(&events_of(&record).await, &record);
    }

    const ONE_STEP: &str = r#"
[[steps]]
name = "implement"
agent = "claude"
prompt = "implement the work item"
"#;

    /// The same step, but a failure aborts at once: no yolo countdown, no retry.
    const ONE_STEP_ABORT: &str = r#"
[[steps]]
name = "implement"
agent = "claude"
prompt = "implement the work item"
abort_on_failure = true
"#;

    /// A workflow with a teardown shell step and no `on_failure` block.
    fn teardown_workflow(command: &str) -> String {
        format!("{ONE_STEP}\n[[teardown]]\ntype = \"run_shell\"\ncommand = \"{command}\"\n")
    }

    /// The ordered `StdoutLine` payloads of a stream.
    fn stdout_lines(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .filter(|e| payload_type(e) == "StdoutLine")
            .filter_map(|e| e["payload"]["data"].as_str().map(str::to_string))
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_prompt_translates_agent_frames_into_activity_and_the_final_result() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        std::fs::write(home.control().join("mode"), "structured").unwrap();
        std::fs::write(
            home.control().join("agent.ndjson"),
            include_str!("../../src/engine/agent_output/testdata/claude_stream_json.ndjson"),
        )
        .unwrap();
        let repo = LocalRepo::new(&home, &[]);
        let server = home.start().await;
        let id = repo.session(&server).await;
        let record = server
            .run_command(&id, "exec prompt", &["do the work"])
            .await;
        assert_eq!(
            record["result"]["final_message"],
            "Implemented structured output.\nValidation: cargo check passed."
        );
        let events = events_of(&record).await;
        assert_one_final_status(&events, &record);
        assert_eq!(
            events
                .iter()
                .filter(|event| payload_type(event) == "StepActivity")
                .count(),
            7
        );
        let lines = stdout_lines(&events);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(!serde_json::to_string(&events).unwrap().contains("fn main"));
    }

    // ── non-exec commands and pre-engine failures ────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_agents_status_puts_its_json_in_result_data_and_keeps_streaming_it() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        let repo = LocalRepo::new(&home, &[]);
        let server = home.start().await;
        let id = repo.session(&server).await;

        let record = server.run_command(&id, "agents status", &[]).await;
        assert!(record["result"].get("git").is_none());

        assert_eq!(record["status"], "done", "{record}");
        let data = &record["result"]["data"];
        assert_eq!(data["runtime"], "docker", "{record}");
        assert_eq!(data["available"], true);
        assert_eq!(data["agents"], json!([]));
        assert_eq!(
            data["totals"],
            json!({"running": 0, "stuck": 0, "stopped": 0, "cpu_percent": null, "memory_mb": null})
        );
        let events = events_of(&record).await;
        // Kept for one release: the same JSON as a stdout line.
        let lines = stdout_lines(&events);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(serde_json::from_str::<Value>(&lines[0]).unwrap(), *data);
        let status = assert_one_final_status(&events, &record);
        assert_eq!(status["status"], "done");
        // A command that ran no agent leaves no worktree and no output.
        assert_eq!(record["result"]["outputs"], json!({"files": 0, "bytes": 0}));
        assert!(record["result"].get("worktree_path").is_none());
        assert!(record["result"].get("final_message").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_agents_list_puts_its_json_array_in_result_data() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        let repo = LocalRepo::new(&home, &[]);
        let server = home.start().await;
        let id = repo.session(&server).await;

        let record = server.run_command(&id, "agents list", &[]).await;
        assert!(record["result"].get("git").is_none());

        assert_eq!(record["status"], "done", "{record}");
        assert_eq!(record["result"]["data"], json!([]), "{record}");
        assert_one_final_status(&events_of(&record).await, &record);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_failure_before_the_engine_starts_still_gets_one_final_status() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        let repo = LocalRepo::new(&home, &[]);
        let base = repo.head();
        let server = home.start().await;
        let id = repo.session(&server).await;

        let record = server
            .run_command(&id, "exec prompt", &["--agent", "nosuch", "hi"])
            .await;

        assert_eq!(record["status"], "error", "{record}");
        let result = &record["result"];
        assert_eq!(result["outcome"], "error");
        assert_eq!(result["failure"]["stage"], "preflight", "{result}");
        assert_eq!(result["failure"]["code"], "unknown_agent");
        assert!(result["failure"].get("step").is_none(), "{result}");
        let message = result["failure"]["message"].as_str().unwrap();
        assert!(message.contains("nosuch"), "{message}");
        // Legacy fields are filled exactly as before, and agree with `failure`.
        assert_eq!(result["error"], result["failure"]["message"]);
        assert_eq!(result["error_status"], 404);
        assert_eq!(result["stage"], "preflight");
        assert_eq!(result["exit_code"], 4);
        assert_eq!(record["exit_code"], 4);
        // The checkout is still reported; nothing ran, so nothing changed.
        assert_eq!(result["git"]["base_sha"], base.as_str());
        assert_eq!(result["git"]["changed_files"], json!([]));
        assert!(result.get("failed_steps").is_none(), "{result}");

        let events = events_of(&record).await;
        let status = assert_one_final_status(&events, &record);
        assert_eq!(status["status"], "error");
        assert_eq!(status["exit_code"], 4);
        assert_eq!(status["error"], result["error"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_existing_worktree_fails_in_preflight_with_its_own_code() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        // The first run keeps its worktree (failed main step); the second finds it.
        home.set_agent(AgentMode::Fail);
        let repo = LocalRepo::new(&home, &[("wf.toml", ONE_STEP_ABORT)]);
        let server = home.start().await;
        let id = repo.session(&server).await;
        let first = server.run_command(&id, "exec workflow", &["wf.toml"]).await;
        assert!(first["result"]["worktree_path"].is_string(), "{first}");

        let record = server.run_command(&id, "exec workflow", &["wf.toml"]).await;

        assert_eq!(record["status"], "error", "{record}");
        let failure = &record["result"]["failure"];
        assert_eq!(failure["stage"], "preflight", "{record}");
        assert_eq!(failure["code"], "worktree_exists");
        assert!(record["result"].get("git").is_none());
        assert_eq!(record["result"]["stage"], "preflight");
        let status = assert_one_final_status(&events_of(&record).await, &record);
        assert_eq!(status["result"]["failure"]["code"], "worktree_exists");
    }

    // ── workflows in a worktree ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_worktree_merge_back_reports_the_merge_target_and_its_new_head() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        let repo = LocalRepo::new(&home, &[("wf.toml", ONE_STEP)]);
        let base = repo.head();
        let server = home.start().await;
        let id = repo.session(&server).await;

        let record = server.run_command(&id, "exec workflow", &["wf.toml"]).await;

        assert_eq!(record["status"], "done", "{record}\n{}", server.log());
        let result = &record["result"];
        assert_eq!(result["outcome"], "done");
        let git = &result["git"];
        // After merge-back the result lives on the branch the user started on.
        assert_eq!(git["branch"], "main", "{result}");
        assert_eq!(git["base_sha"], base.as_str());
        assert_eq!(git["head_sha"], repo.head().as_str());
        assert_ne!(git["head_sha"], git["base_sha"], "merge-back moved HEAD");
        assert_eq!(git["dirty"], false);
        let files = git["changed_files"].as_array().unwrap();
        let edit = files
            .iter()
            .find(|f| f["path"] == "agent-edit.txt")
            .unwrap_or_else(|| panic!("agent-edit.txt not in {files:?}"));
        assert_eq!(edit["status"], "added");
        assert_eq!(edit["added"], 1);
        assert_eq!(edit["deleted"], 0);
        // The squash-merged worktree is gone, so there is no path to report.
        assert!(result.get("worktree_path").is_none(), "{result}");
        assert!(result.get("failed_steps").is_none(), "{result}");
        assert!(result.get("data").is_none());
        let status = assert_one_final_status(&events_of(&record).await, &record);
        assert_eq!(status["status"], "done");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_failed_step_keeps_the_worktree_and_git_describes_its_branch() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        home.set_agent(AgentMode::Fail);
        let repo = LocalRepo::new(&home, &[("wf.toml", ONE_STEP_ABORT)]);
        let base = repo.head();
        let server = home.start().await;
        let id = repo.session(&server).await;

        let record = server.run_command(&id, "exec workflow", &["wf.toml"]).await;

        assert_eq!(record["status"], "error", "{record}");
        let result = &record["result"];
        assert_eq!(result["outcome"], "aborted");
        assert_eq!(result["exit_code"], 1);
        let worktree = PathBuf::from(result["worktree_path"].as_str().expect("kept worktree"));
        assert!(worktree.is_dir());
        let git_facts = &result["git"];
        assert_eq!(git_facts["branch"], "awman/workflow-wf", "{result}");
        assert_eq!(
            git_facts["branch"],
            git(&worktree, &["branch", "--show-current"]).as_str()
        );
        assert_eq!(git_facts["base_sha"], base.as_str());
        assert_eq!(
            git_facts["head_sha"],
            git(&worktree, &["rev-parse", "HEAD"]).as_str()
        );
        assert_eq!(
            git_facts["dirty"], true,
            "the failed step's edit is uncommitted"
        );
        let paths: Vec<_> = git_facts["changed_files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.contains(&"agent-edit.txt".to_string()), "{paths:?}");
        // The user's own branch was never touched.
        assert_eq!(repo.head(), base);
        assert_eq!(git_status(&repo.path), "");

        // `abort_on_failure` ends the workflow: the row is still `error`, but the
        // outcome refines it to `aborted`, and the failed step is still listed.
        let failure = &result["failure"];
        assert_eq!(failure["stage"], "step");
        assert_eq!(failure["code"], "aborted");
        assert_eq!(result["error"], failure["message"]);
        let failed = result["failed_steps"].as_array().unwrap();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(
            (&failed[0]["name"], &failed[0]["phase"]),
            (&json!("implement"), &json!("main"))
        );
        assert_eq!(failed[0]["exit_code"], 3);
        assert_eq!(failed[0]["step_attempt"], 1);
        let status = assert_one_final_status(&events_of(&record).await, &record);
        assert_eq!(status["status"], "aborted");
    }

    /// Exercise the real 60 s countdown and automatic retry in the normal suite.
    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_a_retried_main_step_is_listed_once_with_its_last_attempt() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        home.set_agent(AgentMode::Fail);
        let repo = LocalRepo::new(&home, &[("wf.toml", ONE_STEP)]);
        let server = home.start().await;
        let id = repo.session(&server).await;

        let record = server.run_command(&id, "exec workflow", &["wf.toml"]).await;

        let failed = record["result"]["failed_steps"]
            .as_array()
            .expect("failed_steps");
        assert_eq!(
            failed.len(),
            1,
            "one entry per step, not per attempt: {failed:?}"
        );
        let step = &failed[0];
        assert_eq!(step["name"], "implement");
        assert_eq!(step["index"], 0);
        assert_eq!(step["phase"], "main");
        assert_eq!(step["exit_code"], 3);
        assert_eq!(
            step["step_attempt"], 2,
            "the automatic retry was the last attempt"
        );
        let failure = &record["result"]["failure"];
        assert_eq!(failure["code"], "step_failed");
        assert_eq!(failure["stage"], "step");
        assert_eq!(failure["step"], "implement");
        assert_eq!(record["result"]["outcome"], "error");
        assert!(step["error"].as_str().unwrap().contains("exit 3"), "{step}");
        // The docker log shows the agent really ran twice.
        let runs = std::fs::read_to_string(home.control().join("docker.log"))
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("docker run") && !l.contains("-d "))
            .count();
        assert_eq!(runs, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_a_failed_setup_step_that_does_not_abort_is_listed_but_the_command_is_done() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        let workflow = format!("[[setup]]\ntype = \"run_shell\"\ncommand = \"exit 7\"\n{ONE_STEP}");
        let repo = LocalRepo::new(&home, &[("wf.toml", &workflow)]);
        let server = home.start().await;
        let id = repo.session(&server).await;

        let record = server.run_command(&id, "exec workflow", &["wf.toml"]).await;

        // The workflow carried on past the failed setup step, so the command
        // succeeded, yet the step is still reported.
        assert_eq!(record["status"], "done", "{record}");
        let result = &record["result"];
        assert_eq!(result["outcome"], "done");
        assert!(result.get("failure").is_none(), "{result}");
        let failed = result["failed_steps"].as_array().unwrap();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0]["phase"], "setup");
        assert_eq!(failed[0]["index"], 0);
        assert_eq!(failed[0]["exit_code"], 7);
        assert_eq!(failed[0]["name"], "run_shell: exit 7");
        let status = assert_one_final_status(&events_of(&record).await, &record);
        assert_eq!(status["status"], "done");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_an_aborting_setup_failure_is_a_setup_stage_failure() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        let workflow = format!(
            "[[setup]]\ntype = \"run_shell\"\ncommand = \"exit 7\"\nabort_on_failure = true\n{ONE_STEP}"
        );
        let repo = LocalRepo::new(&home, &[("wf.toml", &workflow)]);
        let server = home.start().await;
        let id = repo.session(&server).await;

        let record = server.run_command(&id, "exec workflow", &["wf.toml"]).await;

        assert_eq!(record["status"], "error", "{record}");
        let result = &record["result"];
        assert_eq!(result["outcome"], "error");
        assert_eq!(result["failure"]["stage"], "setup", "{result}");
        assert_eq!(result["failure"]["code"], "setup_failed");
        let failed = result["failed_steps"].as_array().unwrap();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0]["phase"], "setup");
        assert_eq!(failed[0]["exit_code"], 7);
        let status = assert_one_final_status(&events_of(&record).await, &record);
        assert_eq!(status["status"], "error");
    }

    // ── teardown ─────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_teardown_failure_is_error_in_both_the_row_and_the_event() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        let workflow = teardown_workflow("echo boom >&2; exit 3");
        let repo = LocalRepo::new(&home, &[("wf.toml", &workflow)]);
        let server = home.start().await;
        let id = repo.session(&server).await;

        let record = server.run_command(&id, "exec workflow", &["wf.toml"]).await;

        // The row, as before, is an error with the teardown's exit code ...
        assert_eq!(record["status"], "error", "{record}");
        assert_eq!(record["exit_code"], 1);
        let result = &record["result"];
        assert_eq!(result["outcome"], "error");
        assert_eq!(result["failure"]["stage"], "teardown", "{result}");
        assert_eq!(result["failure"]["code"], "teardown_failed");
        assert_eq!(
            result["failure"]["step"],
            "run_shell: echo boom >&2; exit 3"
        );
        assert_eq!(result["error"], result["failure"]["message"]);
        let failed = result["failed_steps"].as_array().unwrap();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0]["phase"], "teardown");
        assert_eq!(failed[0]["index"], 0);
        assert_eq!(failed[0]["exit_code"], 3);
        assert_eq!(failed[0]["step_attempt"], 1);
        assert!(
            failed[0]["error"].as_str().unwrap().contains("boom"),
            "{failed:?}"
        );

        // ... and the event agrees: no more `done` with exit code 1.
        let events = events_of(&record).await;
        let status = assert_one_final_status(&events, &record);
        assert_eq!(status["status"], "error", "{status}");
        assert_eq!(status["exit_code"], 1);
        assert_eq!(status["result"]["outcome"], "error");
        // The workflow's own phase transition still reports the teardown failure,
        // ahead of the final status.
        let phase = events
            .iter()
            .position(|e| {
                payload_type(e) == "WorkflowPhaseTransition"
                    && e["payload"]["data"]["phase"] == "main"
                    && e["payload"]["data"]["status"] == "teardown_failed"
            })
            .expect("main teardown_failed phase transition");
        assert!(phase < events.len() - 2);
        // No `command_status` of any other kind was emitted along the way.
        assert!(events
            .iter()
            .filter(|e| payload_type(e) == "CommandStatus")
            .all(|e| e["payload"]["data"]["status"] == "error"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_a_retried_teardown_step_is_listed_once_with_its_last_attempt() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        let workflow = format!(
            "{ONE_STEP}\n[[teardown]]\ntype = \"run_shell\"\ncommand = \"exit 5\"\n\n\
             [teardown.on_failure]\nagent = \"claude\"\nprompt = \"fix it\"\nmax_attempts = 2\n"
        );
        let repo = LocalRepo::new(&home, &[("wf.toml", &workflow)]);
        let server = home.start().await;
        let id = repo.session(&server).await;

        let record = server.run_command(&id, "exec workflow", &["wf.toml"]).await;

        assert_eq!(record["status"], "error", "{record}");
        let failed = record["result"]["failed_steps"].as_array().unwrap();
        assert_eq!(
            failed.len(),
            1,
            "one entry, not one per attempt: {failed:?}"
        );
        assert_eq!(failed[0]["phase"], "teardown");
        assert_eq!(failed[0]["exit_code"], 5);
        // The original run plus two remediation retries.
        assert_eq!(failed[0]["step_attempt"], 3, "{failed:?}");
        assert_eq!(record["result"]["failure"]["code"], "teardown_failed");
        assert_one_final_status(&events_of(&record).await, &record);
    }

    // ── remote sessions: push-back and the pull request ──────────────────────

    /// Teardown that commits, pushes the branch and opens a pull request.
    const PR_TEARDOWN: &str = r#"
[[teardown]]
type = "run_shell"
command = "git config user.name Test && git config user.email test@localhost"
[[teardown]]
type = "commit_changes"
message = "workflow commit"
add_all = true
[[teardown]]
type = "push_branch"
[[teardown]]
type = "create_pull_request"
"#;

    /// What `gh pr create` prints on success: progress on stderr, and the new
    /// pull request's URL as the last line of stdout.
    const RECORDED_GH_PR_CREATE: &str = r#"#!/bin/sh
echo "Creating pull request for failed-teardown into main in o/r" >&2
echo
echo "https://github.com/o/r/pull/7"
exit 0
"#;

    fn install_gh(home: &ApiHome, script: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let gh = home.control().parent().unwrap().join("bin/gh");
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_remote_session_with_push_reports_the_pushed_head_in_git() {
        crate::real_git_skip!();
        let Some(origin) = Origin::start() else {
            return;
        };
        let home = ApiHome::new();
        let server = home.start().await;
        let id = server
            .ready_session(json!({
                "session_type": "remote",
                "repo_url": origin.url,
                "branch": "prompt-work",
                "push": true,
            }))
            .await;
        let main = origin.branch_sha("main").unwrap();

        let record = server
            .run_command(&id, "exec prompt", &["edit a file"])
            .await;

        assert_eq!(record["status"], "done", "{record}\n{}", server.log());
        let result = &record["result"];
        let pushed_sha = result["pushed"]["sha"].as_str().expect("pushed sha");
        assert_eq!(
            origin.branch_sha("prompt-work").as_deref(),
            Some(pushed_sha)
        );
        let git = &result["git"];
        assert_eq!(git["branch"], "prompt-work", "{result}");
        assert_eq!(
            git["base_sha"],
            main.as_str(),
            "HEAD when the command started"
        );
        assert_eq!(
            git["head_sha"], pushed_sha,
            "HEAD after the push-back commit"
        );
        assert_eq!(git["dirty"], false, "everything was committed and pushed");
        assert_eq!(
            git["changed_files"],
            json!([{"path": "agent-edit.txt", "status": "added", "added": 1, "deleted": 0}])
        );
        assert_eq!(result["pushed"]["branch"], "prompt-work");
        assert_eq!(result["pushed"]["remote"], "origin");
        assert!(result.get("pull_request").is_none(), "{result}");
        let status = assert_one_final_status(&events_of(&record).await, &record);
        // The event carries the push as well as the git facts.
        assert_eq!(status["result"]["pushed"]["sha"], pushed_sha);
        assert_eq!(status["result"]["git"]["head_sha"], pushed_sha);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_pull_request_url_comes_from_the_recorded_gh_output() {
        crate::real_git_skip!();
        let Some(origin) = Origin::start() else {
            return;
        };
        let home = ApiHome::new();
        home.set_teardown(PR_TEARDOWN);
        install_gh(&home, RECORDED_GH_PR_CREATE);
        let server = home.start().await;
        let id = server
            .ready_session(json!({
                "session_type": "remote", "repo_url": origin.url,
                "branch": "with-pr", "push": true,
            }))
            .await;

        let record = server
            .run_command(&id, "exec workflow", &["--dynamic", "--work-item", "0042"])
            .await;

        assert_eq!(record["status"], "done", "{record}\n{}", server.log());
        let result = &record["result"];
        assert_eq!(
            result["pull_request"],
            json!({"url": "https://github.com/o/r/pull/7"})
        );
        let final_sha = origin.branch_sha("with-pr").unwrap();
        assert_eq!(result["pushed"]["sha"], final_sha.as_str());
        assert_eq!(result["git"]["branch"], "with-pr");
        assert_eq!(result["git"]["head_sha"], final_sha.as_str());
        let status = assert_one_final_status(&events_of(&record).await, &record);
        assert_eq!(
            status["result"]["pull_request"]["url"],
            "https://github.com/o/r/pull/7"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_no_pull_request_is_reported_when_gh_prints_no_url() {
        crate::real_git_skip!();
        let Some(origin) = Origin::start() else {
            return;
        };
        let home = ApiHome::new();
        home.set_teardown(PR_TEARDOWN); // its gh stub exits 0 and prints nothing
        let server = home.start().await;
        let id = server
            .ready_session(json!({
                "session_type": "remote", "repo_url": origin.url,
                "branch": "no-url", "push": true,
            }))
            .await;

        let record = server
            .run_command(&id, "exec workflow", &["--dynamic", "--work-item", "0042"])
            .await;

        assert_eq!(record["status"], "done", "{record}\n{}", server.log());
        assert!(record["result"].get("pull_request").is_none(), "{record}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_push_failure_is_the_one_final_status_with_stage_push() {
        crate::real_git_skip!();
        let Some(origin) = Origin::start() else {
            return;
        };
        origin.commit_on("contested", Some("main"), &[("c.txt", "c\n")], "contested");
        let home = ApiHome::new();
        let server = home.start().await;
        let id = server
            .ready_session(json!({
                "session_type": "remote", "repo_url": origin.url,
                "branch": "contested", "push": true,
            }))
            .await;
        let clone = clone_path(&server.session(&id).await);
        // Someone else moves the branch after the session cloned it.
        let theirs = origin.commit_on("contested", None, &[("theirs.txt", "t\n")], "theirs");

        let record = server
            .run_command(&id, "exec prompt", &["edit a file"])
            .await;

        assert_eq!(record["status"], "error", "{record}");
        assert_eq!(record["exit_code"], 1);
        let result = &record["result"];
        assert_eq!(result["outcome"], "error");
        assert_eq!(result["failure"]["stage"], "push", "{result}");
        assert_eq!(result["failure"]["code"], "push_failed");
        assert!(
            result["failure"]["message"]
                .as_str()
                .unwrap()
                .contains("rejected")
                || result["failure"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("fetch first"),
            "{result}"
        );
        // Legacy fields are unchanged.
        assert_eq!(result["stage"], "push");
        assert_eq!(result["error_status"], 500);
        assert_eq!(result["pushed"], Value::Null);
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("git push -u origin HEAD:refs/heads/contested failed"),
            "{result}"
        );
        // The git facts still describe what the agent left in the clone.
        let ours = git(&clone, &["rev-parse", "HEAD"]);
        assert_eq!(result["git"]["head_sha"], ours.as_str());
        assert_eq!(result["git"]["dirty"], false);
        assert_eq!(result["git"]["changed_files"][0]["path"], "agent-edit.txt");
        assert_eq!(origin.branch_sha("contested"), Some(theirs));

        // No separate push-failure event: the one final status carries it.
        let events = events_of(&record).await;
        let status = assert_one_final_status(&events, &record);
        assert_eq!(status["status"], "error");
        assert!(
            status["error"].as_str().unwrap().contains("push:"),
            "{status}"
        ); // the event keeps its legacy prefix
        assert_eq!(status["result"]["failure"]["stage"], "push");
    }

    // ── /v1/runs setup failure ───────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn real_network_runs_setup_failure_gets_one_final_status_with_stage_setup() {
        let home = ApiHome::new();
        let server = home.start().await;
        let (status, created) = http(
            &server,
            reqwest::Method::POST,
            "/v1/runs",
            None,
            Some(json!({
                "session": remote(BAD_REPO),
                "command": {"subcommand": "exec prompt", "args": ["x"]},
            })),
        )
        .await;
        assert_eq!(status, 202, "{created}");

        let record =
            wait_status(&server, created["command_id"].as_str().unwrap(), &["error"]).await;

        let result = &record["result"];
        assert_eq!(record["exit_code"], 1);
        assert_eq!(result["outcome"], "error", "{record}");
        assert_eq!(result["failure"]["stage"], "setup");
        assert_eq!(result["failure"]["code"], "setup_failed");
        // Legacy fields.
        assert_eq!(result["error"], "setup_error");
        assert_eq!(result["stage"], "setup");
        assert_eq!(result["error_status"], 409);
        // There was no checkout and no command, so nothing else is claimed.
        assert!(result.get("git").is_none(), "{result}");
        assert!(result.get("failed_steps").is_none(), "{result}");

        let events = events_of(&record).await;
        let final_status = assert_one_final_status(&events, &record);
        assert_eq!(final_status["status"], "error");
        assert_eq!(final_status["exit_code"], 1);
    }

    // ── cancellation ─────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_cancelling_a_running_command_ends_with_one_cancelled_status() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        home.set_agent(AgentMode::Slow);
        let repo = LocalRepo::new(&home, &[]);
        let server = home.start().await;
        let id = repo.session(&server).await;
        let command = submit(&server, &id, "exec prompt", &["slow"]).await;
        wait_slow_agent(&home).await;

        let (status, body) = http(
            &server,
            reqwest::Method::POST,
            &format!("/v1/commands/{command}/cancel"),
            None,
            Some(Value::Null),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let record = wait_status(&server, &command, &["cancelled", "done", "error"]).await;

        assert_eq!(record["status"], "cancelled", "{record}\n{}", server.log());
        assert_eq!(record["exit_code"], 130);
        let result = &record["result"];
        assert_eq!(result["outcome"], "cancelled");
        assert_eq!(result["failure"]["code"], "cancelled");
        assert_eq!(result["error"], "cancelled");
        assert!(result["cancelled_at"].is_string(), "{result}");
        // The run's leftovers are still reported.
        assert!(result["git"].is_object(), "{result}");
        assert!(result["outputs"].is_object(), "{result}");
        let events = events_of(&record).await;
        let status = assert_one_final_status(&events, &record);
        assert_eq!(status["status"], "cancelled");
        assert_eq!(status["exit_code"], 130);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_git_cancelling_a_queued_command_ends_with_one_cancelled_status() {
        crate::real_git_skip!();
        let home = ApiHome::new();
        home.set_agent(AgentMode::Slow);
        let repo = LocalRepo::new(&home, &[]);
        let server = home.start().await;
        let id = repo.session(&server).await;
        let first = submit(&server, &id, "exec prompt", &["occupy the session"]).await;
        wait_slow_agent(&home).await;
        let queued = submit(&server, &id, "exec prompt", &["never runs"]).await;
        let before = server.get(&format!("/v1/commands/{queued}/status")).await;
        assert_eq!(before["status"], "queued", "{before}");

        let (status, body) = http(
            &server,
            reqwest::Method::POST,
            &format!("/v1/commands/{queued}/cancel"),
            None,
            Some(Value::Null),
        )
        .await;
        assert_eq!(status, 200, "{body}");

        let record = wait_status(&server, &queued, &["cancelled"]).await;
        assert!(record["result"].get("git").is_none());
        assert_eq!(record["exit_code"], 130);
        let result = &record["result"];
        assert_eq!(result["outcome"], "cancelled", "{record}");
        assert_eq!(result["failure"]["code"], "cancelled");
        assert_eq!(result["error"], "cancelled");
        let events = events_of(&record).await;
        let final_status = assert_one_final_status(&events, &record);
        assert_eq!(final_status["status"], "cancelled");
        // Nothing but the final pair: the command never started.
        assert_eq!(events.len(), 2, "{events:#?}");
        let _ = http(
            &server,
            reqwest::Method::POST,
            &format!("/v1/commands/{first}/cancel"),
            None,
            Some(Value::Null),
        )
        .await;
    }
}

mod result_shape {
    //! `CommandResult` as a wire object and the recorders that fill it.

    use awman::data::execution_event::CommandStatusKind;
    use awman::data::fs::api_command_log::CommandStateWriter;
    use awman::data::fs::api_db::{
        CommandFailure, CommandResult, FailedStep, FailedStepPhase, PullRequestRef,
    };
    use awman::data::fs::api_paths::ApiPaths;
    use serde_json::{json, Value};

    #[test]
    fn a_pre_0137_result_round_trips_with_every_original_key_and_no_new_ones() {
        let legacy = json!({
            "exit_code": 1,
            "error": "push: rejected",
            "error_status": 500,
            "stage": "push",
            "pushed": null,
            "worktree_path": "/w",
            "setup_error": {"stage": "clone", "message": "boom"},
            "cancelled_at": "2026-01-01T00:00:00Z",
        });
        let parsed: CommandResult = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(parsed.outcome, None);
        assert!(parsed.failure.is_none() && parsed.git.is_none());
        assert_eq!(serde_json::to_value(&parsed).unwrap(), legacy);
    }

    #[test]
    fn an_unset_pushed_stays_absent_and_an_explicit_null_stays_null() {
        let absent: CommandResult = serde_json::from_value(json!({"exit_code": 0})).unwrap();
        assert!(!serde_json::to_value(&absent)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("pushed"));
        let null: CommandResult =
            serde_json::from_value(json!({"exit_code": 0, "pushed": null})).unwrap();
        assert_eq!(serde_json::to_value(&null).unwrap()["pushed"], Value::Null);
    }

    #[test]
    fn the_new_fields_use_the_documented_wire_names() {
        let result = CommandResult {
            exit_code: Some(1),
            outcome: Some(CommandStatusKind::Error),
            failure: Some(CommandFailure {
                stage: "teardown".into(),
                code: "teardown_failed".into(),
                message: "Teardown failed".into(),
                step: Some("tests".into()),
            }),
            failed_steps: Some(vec![FailedStep {
                name: "tests".into(),
                index: 2,
                phase: FailedStepPhase::Teardown,
                exit_code: Some(1),
                step_attempt: 2,
                error: Some("nope".into()),
            }]),
            pull_request: Some(PullRequestRef {
                url: "https://github.com/o/r/pull/7".into(),
            }),
            ..CommandResult::default()
        };
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            json!({
                "exit_code": 1,
                "outcome": "error",
                "failure": {
                    "stage": "teardown", "code": "teardown_failed",
                    "message": "Teardown failed", "step": "tests"
                },
                "failed_steps": [{
                    "name": "tests", "index": 2, "phase": "teardown",
                    "exit_code": 1, "step_attempt": 2, "error": "nope"
                }],
                "pull_request": {"url": "https://github.com/o/r/pull/7"},
            })
        );
    }

    fn step(name: &str, index: usize, phase: FailedStepPhase, attempt: u32) -> FailedStep {
        FailedStep {
            name: name.into(),
            index,
            phase,
            exit_code: Some(1),
            step_attempt: attempt,
            error: None,
        }
    }

    fn writer() -> (tempfile::TempDir, CommandStateWriter) {
        let dir = tempfile::tempdir().unwrap();
        let writer = CommandStateWriter::new(ApiPaths::from_root(dir.path()), "s", "c");
        (dir, writer)
    }

    #[test]
    fn a_retried_step_is_recorded_once_with_its_last_attempt() {
        let (_dir, writer) = writer();
        writer.record_failed_step(step("tests", 2, FailedStepPhase::Main, 1));
        writer.record_failed_step(step("docs", 3, FailedStepPhase::Main, 1));
        writer.record_failed_step(step("tests", 2, FailedStepPhase::Main, 2));
        let steps = writer.result_contributions().failed_steps;
        assert_eq!(steps.len(), 2, "{steps:?}");
        let tests = steps.iter().find(|s| s.name == "tests").unwrap();
        assert_eq!(tests.step_attempt, 2);
        assert_eq!(
            steps
                .iter()
                .find(|s| s.name == "docs")
                .unwrap()
                .step_attempt,
            1
        );
    }

    #[test]
    fn steps_with_the_same_index_in_different_phases_stay_separate() {
        let (_dir, writer) = writer();
        writer.record_failed_step(step("a", 0, FailedStepPhase::Setup, 1));
        writer.record_failed_step(step("b", 0, FailedStepPhase::Main, 1));
        writer.record_failed_step(step("c", 0, FailedStepPhase::Teardown, 1));
        assert_eq!(writer.result_contributions().failed_steps.len(), 3);
    }

    #[test]
    fn a_later_record_keeps_exit_code_and_error_the_earlier_one_knew() {
        let (_dir, writer) = writer();
        let mut first = step("tests", 0, FailedStepPhase::Teardown, 1);
        first.error = Some("boom".into());
        writer.record_failed_step(first);
        let mut again = step("tests", 0, FailedStepPhase::Teardown, 1);
        again.exit_code = None;
        writer.record_failed_step(again);
        let steps = writer.result_contributions().failed_steps;
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].exit_code, Some(1));
        assert_eq!(steps[0].error.as_deref(), Some("boom"));
    }

    #[test]
    fn contributions_start_empty_and_hold_what_was_recorded() {
        let (_dir, writer) = writer();
        let empty = writer.result_contributions();
        assert!(empty.final_message.is_none() && empty.pull_request.is_none());
        assert!(empty.data.is_none() && empty.failed_steps.is_empty());
        writer.record_pull_request("https://github.com/o/r/pull/9");
        writer.record_data(json!({"agents": []}));
        writer.record_final_message("first");
        writer.record_final_message("second"); // an exec prompt restart replaces it
        let got = writer.result_contributions();
        assert_eq!(
            got.pull_request.unwrap().url,
            "https://github.com/o/r/pull/9"
        );
        assert_eq!(got.data, Some(json!({"agents": []})));
        assert_eq!(got.final_message.as_deref(), Some("second"));
        // A clone shares the same state: the frontend and the worker hold clones.
        let clone = writer.clone();
        clone.record_pull_request("https://github.com/o/r/pull/10");
        assert_eq!(
            writer.result_contributions().pull_request.unwrap().url,
            "https://github.com/o/r/pull/10"
        );
    }
}

#[cfg(unix)]
#[test]
fn real_git_unborn_staged_files_and_unavailable_bases_keep_known_checkout_facts() {
    use crate::git_daemon::git;
    use awman::engine::git::GitEngine;
    crate::real_git_skip!();
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    git(tmp.path(), &["config", "user.name", "Fixture"]);
    git(tmp.path(), &["config", "user.email", "fixture@awman.test"]);
    std::fs::write(tmp.path().join("staged.txt"), "line\n").unwrap();
    git(tmp.path(), &["add", "staged.txt"]);
    let result = GitEngine::new()
        .command_result(tmp.path(), None)
        .unwrap()
        .unwrap();
    assert!(result.dirty);
    assert_eq!(result.changed_files.len(), 1);
    assert_eq!(result.changed_files[0].path, "staged.txt");
    assert_eq!(result.changed_files[0].added, Some(1));
    assert!(result.changed_files_error.is_none());
    git(tmp.path(), &["commit", "-qm", "base"]);
    let result = GitEngine::new()
        .command_result(tmp.path(), Some("invalid-base-sha"))
        .unwrap()
        .unwrap();
    assert_eq!(result.branch.as_deref(), Some("main"));
    assert!(result.head_sha.is_some());
    assert!(!result.dirty);
    assert!(result.changed_files_error.is_some());
}
