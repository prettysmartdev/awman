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
use std::sync::Arc;

use awman::command::dispatch::Engines;
use awman::data::config::env::{EnvSnapshot, AWMAN_API_ROOT, AWMAN_CONFIG_HOME};
use awman::data::config::flags::FlagConfig;
use awman::data::fs::{ApiPaths, AuthPathResolver};
use awman::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;

// ─── Runtime skip helpers ────────────────────────────────────────────────────

/// Returns true when a Docker daemon is reachable.
pub fn docker_available() -> bool {
    std::process::Command::new(awman::engine::host_cli::program("docker"))
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
        let api_root = self.home_dir.path().join("api");
        EnvSnapshot::with_overrides([
            (
                AWMAN_CONFIG_HOME.to_string(),
                self.home_dir.path().to_str().unwrap().to_string(),
            ),
            (
                AWMAN_API_ROOT.to_string(),
                api_root.to_str().unwrap().to_string(),
            ),
        ])
    }

    pub fn api_root(&self) -> PathBuf {
        self.home_dir.path().join("api")
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

    /// A hermetic engine bundle rooted at this fixture's fake HOME (WI 0114
    /// F-52).
    ///
    /// `Engines::for_tests` is `#[cfg(test)]`, so it is invisible from an
    /// integration-test binary; every test binary that needed one used to
    /// hand-assemble the eight engines, and the copies had already drifted in
    /// whether `container_runtime` was populated. That choice is the one thing
    /// a caller actually varies, so it is the argument: `Some(runtime)` means
    /// "this test may reach a container", `None` means "refuse before Docker
    /// is touched".
    pub fn engines(&self, with_container_runtime: bool) -> Engines {
        engines_at(
            self.home_dir.path(),
            self.git_root.path(),
            with_container_runtime,
        )
    }
}

/// [`IsolatedEnv::engines`] for a test that owns its own directories.
///
/// `home` roots the auth and API paths; `git_root` roots the workflow-state
/// store. They are the same directory in most fixtures.
pub fn engines_at(
    home: &std::path::Path,
    git_root: &std::path::Path,
    with_container_runtime: bool,
) -> Engines {
    let runtime = Arc::new(ContainerRuntime::docker());
    let auth_paths = AuthPathResolver::at_home(home);
    // `home/api`, the same place `IsolatedEnv::api_root` names.
    let api_paths = ApiPaths::from_root(home.join("api"));
    api_paths.ensure_root().expect("create API paths");
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    Engines {
        runtime: runtime.clone(),
        container_runtime: with_container_runtime.then(|| runtime.clone() as Arc<ContainerRuntime>),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths)),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, runtime)),
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(git_root)),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
    }
}

/// A session whose working directory and Git root are both `root`, with
/// `AWMAN_CONFIG_HOME` pinned there too so it cannot read the developer's real
/// global config. The integration-test counterpart of `Session::for_tests`.
pub fn session_at(root: &std::path::Path) -> Session {
    let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, root.to_str().unwrap())]);
    let opts = SessionOpenOptions {
        env: Some(env),
        ..Default::default()
    };
    Session::open_at_git_root(root.to_path_buf(), root.to_path_buf(), opts).expect("Session::open")
}

// ─── Minimal workflow definition builders ───────────────────────────────────

pub use awman::data::workflow_definition::WorkflowStep;

pub fn wf_step(name: &str, deps: &[&str], prompt: &str) -> WorkflowStep {
    WorkflowStep {
        name: name.to_string(),
        depends_on: deps.iter().map(|s| s.to_string()).collect(),
        prompt_template: prompt.to_string(),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
    }
}
