//! Binary-level smoke tests (WI 0073).
//!
//! These tests invoke the real `awman` binary as a subprocess and verify
//! exit codes, stdout shapes, and basic CLI behaviour.
//!
//! All tests here run under `make test-fast` because they don't need Docker.
//! Tests that need a real server or real git include those keywords.

mod awman_binary;
#[path = "../helpers/mod.rs"]
mod helpers;

pub(crate) fn awman_bin() -> std::path::PathBuf {
    awman_binary::awman_bin()
}

/// A `Command` for the shared `awman` binary copy.
pub(crate) fn awman() -> std::process::Command {
    std::process::Command::new(awman_bin())
}

/// Run `git init --quiet` in `path`, failing the test if git fails.
pub(crate) fn git_init(path: &std::path::Path) {
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(path)
        .status()
        .expect("spawn git init");
    assert!(status.success(), "git init failed in {}", path.display());
}

/// A fresh temporary directory containing an empty git repository.
pub(crate) fn make_git_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("create temp repo");
    git_init(repo.path());
    repo
}

mod agents_docker;
#[cfg(unix)]
mod agents_review;
mod antigravity_agent;
mod build_cfg;
mod clean_command;
mod cli_subprocess;
mod custom_agents;
mod dynamic_workflow_config;
mod fatal_cli_errors;
mod headless_no_tty;
mod overlay_cli;
#[cfg(unix)]
mod session_worktree;
mod skill_libraries;
mod worker_link;
