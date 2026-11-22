//! The git transport conformance world: a bare remote served over a real
//! transport to a server on loopback, plus the clones the suite makes.
//!
//! The remote is built with the support crate's repository fixtures (real
//! `git`, fixed identity and dates). What differs per transport is only how
//! the server is reached ([`Transport`]).

use awman::data::config::env::EnvSnapshot;
use awman::engine::conformance::git::{GitRemoteWorld, RemoteFacts, SIDE_BRANCH};
use awman_test_support::fixtures::{Repo, RepoBuilder};
use awman_test_support::TestSandbox;
use std::cell::RefCell;
use std::path::PathBuf;

use crate::support;

/// How a server is reached.
pub trait Transport {
    /// Name for the report, e.g. `smart HTTP (git http-backend)`.
    fn subject(&self) -> String;
    /// The URL of the bare repository `bare` (a path under the served root).
    fn url(&self, bare: &std::path::Path) -> String;
    /// A URL for the same repository the server refuses, if the transport
    /// has access control.
    fn refused_url(&self, bare: &std::path::Path) -> Option<String>;
    /// A URL on this transport where nothing answers.
    fn unreachable_url(&self) -> String;
}

pub struct RemoteWorld<'a, T: Transport> {
    sandbox: &'a TestSandbox,
    transport: T,
    /// The working repository the remote was created from; it keeps pushing
    /// new commits to the remote when the suite asks it to advance.
    origin: RefCell<Option<Repo<'a>>>,
    bare: RefCell<Option<PathBuf>>,
}

impl<'a, T: Transport> RemoteWorld<'a, T> {
    pub fn new(sandbox: &'a TestSandbox, transport: T) -> RemoteWorld<'a, T> {
        RemoteWorld {
            sandbox,
            transport,
            origin: RefCell::new(None),
            bare: RefCell::new(None),
        }
    }

    fn bare(&self) -> PathBuf {
        self.bare.borrow().clone().expect("given_remote ran first")
    }

    fn git_out(&self, args: &[&str]) -> Option<String> {
        let out = self.sandbox.command("git").args(args).label("git").run();
        out.exit
            .success()
            .then(|| out.stdout_text().trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

impl<T: Transport> GitRemoteWorld for RemoteWorld<'_, T> {
    fn subject(&self) -> String {
        self.transport.subject()
    }

    fn env(&self) -> EnvSnapshot {
        support::env_snapshot(self.sandbox)
    }

    fn scratch_dir(&self, name: &str) -> PathBuf {
        self.sandbox.work().join(name)
    }

    fn given_remote(&self) -> RemoteFacts {
        let origin = RepoBuilder::new(self.sandbox, "origin-work")
            .file("README.md", "conformance\n")
            .init();
        origin.create_branch(SIDE_BRANCH).checkout(SIDE_BRANCH);
        origin.write("side.txt", "side\n");
        let side_head = origin.commit("side branch commit");
        origin.checkout("main");
        let default_head = origin.head();
        let bare = origin.bare_remote("origin");
        let url = self.transport.url(&bare);
        *self.bare.borrow_mut() = Some(bare);
        *self.origin.borrow_mut() = Some(origin);
        RemoteFacts {
            url,
            default_branch: "main".to_string(),
            default_head,
            side_head,
        }
    }

    fn advance_remote(&self) -> String {
        let origin = self.origin.borrow();
        let origin = origin.as_ref().expect("given_remote ran first");
        origin.write("advanced.txt", "advanced\n");
        let head = origin.commit("advance the remote");
        origin.push("origin", "main");
        head
    }

    fn remote_head(&self, branch: &str) -> Option<String> {
        let bare = self.bare();
        let bare = bare.to_string_lossy().into_owned();
        self.git_out(&[
            "-C",
            &bare,
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
    }

    fn refused_url(&self) -> Option<String> {
        self.transport.refused_url(&self.bare())
    }

    fn unreachable_url(&self) -> String {
        self.transport.unreachable_url()
    }

    fn cleanup(&self) {
        // Servers and children belong to the transport and the sandbox; the
        // sandbox's finish() kills and reaps them and checks nothing is left.
    }
}
