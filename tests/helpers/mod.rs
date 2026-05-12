//! Shared helpers for all integration test binaries (WI 0073).
//!
//! Each test binary includes this file via:
//!   `#[path = "../helpers/mod.rs"] mod helpers;`
//!
//! Tests that require Docker must include "docker" in their function name
//! so `make test-fast` skips them via `--skip docker`.
//! Tests that require real git must include "real_git".
//! Tests that require network access must include "real_network".

#![allow(dead_code)]

use std::path::PathBuf;

use amux::data::config::env::{EnvSnapshot, AMUX_CONFIG_HOME, AMUX_HEADLESS_ROOT};
use amux::data::config::flags::FlagConfig;
use amux::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};

// ─── Runtime skip helpers ────────────────────────────────────────────────────

/// Returns true when a Docker daemon is reachable.
pub fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Returns true when the `git` binary is available.
pub fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Skip the calling test at runtime if Docker is unavailable, printing
/// a clear message so CI logs explain why the test did not run.
/// The macro already requires "docker" in the function name to work with
/// `make test-fast`'s `--skip docker` filter.
#[macro_export]
macro_rules! docker_skip {
    () => {
        if !$crate::helpers::docker_available() {
            eprintln!(
                "SKIP: Docker daemon not available — \
                 run `make test-full` on a host with Docker to include this test"
            );
            return;
        }
    };
}

/// Skip the calling test at runtime if git is unavailable.
#[macro_export]
macro_rules! real_git_skip {
    () => {
        if !$crate::helpers::git_available() {
            eprintln!("SKIP: git not available");
            return;
        }
    };
}

// ─── Isolated repo / home helpers ───────────────────────────────────────────

/// Provides an isolated temp directory pair: a fake git root and a fake
/// HOME (config home). Suitable for hermetic data-layer tests.
pub struct IsolatedEnv {
    pub git_root: tempfile::TempDir,
    pub home_dir: tempfile::TempDir,
}

impl IsolatedEnv {
    pub fn new() -> Self {
        Self {
            git_root: tempfile::tempdir().expect("tempdir"),
            home_dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    pub fn env(&self) -> EnvSnapshot {
        let headless_root = self.home_dir.path().join("headless");
        EnvSnapshot::with_overrides([
            (
                AMUX_CONFIG_HOME.to_string(),
                self.home_dir.path().to_str().unwrap().to_string(),
            ),
            (
                AMUX_HEADLESS_ROOT.to_string(),
                headless_root.to_str().unwrap().to_string(),
            ),
        ])
    }

    pub fn headless_root(&self) -> PathBuf {
        self.home_dir.path().join("headless")
    }

    pub fn open_session(&self) -> Session {
        self.open_session_with_flags(FlagConfig::default())
    }

    pub fn open_session_with_flags(&self, flags: FlagConfig) -> Session {
        let resolver = StaticGitRootResolver::new(self.git_root.path());
        let opts = SessionOpenOptions {
            flags,
            env: Some(self.env()),
            available_agents: None,
        };
        Session::open(self.git_root.path().to_path_buf(), &resolver, opts).expect("Session::open")
    }
}

// ─── Minimal workflow definition builders ───────────────────────────────────

pub use amux::data::workflow_definition::WorkflowStep;

pub fn wf_step(name: &str, deps: &[&str], prompt: &str) -> WorkflowStep {
    WorkflowStep {
        name: name.to_string(),
        depends_on: deps.iter().map(|s| s.to_string()).collect(),
        prompt_template: prompt.to_string(),
        agent: None,
        model: None,
    }
}
