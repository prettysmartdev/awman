//! A throwaway origin repository served over `git://` by `git daemon` on
//! loopback (WI 0133).
//!
//! A remote API session only accepts the URL schemes in
//! `DEFAULT_REPO_URL_SCHEMES`, which deliberately excludes `file://`, so a test
//! that creates one over HTTP needs a real network URL. `git daemon` bound to
//! 127.0.0.1 is the smallest server that speaks one (`tools/isolated-test.sh`
//! allows loopback), and `--enable=receive-pack` lets awman's push stage
//! publish to it.
//!
//! The seed commit carries what a repository already set up for awman has
//! (`Dockerfile.dev`, `.awman/Dockerfile.claude` and a work item), so session
//! setup checks those files instead of scaffolding new ones into the clone.
//!
//! Included with `#[path = "../helpers/git_daemon.rs"] mod git_daemon;`.
//! Tests using it carry `real_git` in their names so `make test-fast` skips
//! them.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// The identity fixture commits are made with. Passed per command, so neither
/// the fixture nor any clone awman makes ever has `user.*` in its config.
const FIXTURE_IDENTITY: [&str; 4] = [
    "-c",
    "user.name=Fixture",
    "-c",
    "user.email=fixture@awman.test",
];

/// `git` with no global or system config — what `make test` gives every
/// process, pinned here so a plain `cargo test` behaves the same.
pub fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    command
}

/// Run `git args` in `cwd` and return the raw output, success or not.
pub fn git_output(cwd: &Path, args: &[&str]) -> Output {
    git_command(cwd).args(args).output().expect("spawn git")
}

/// Run `git args` in `cwd`, panic on failure, and return trimmed stdout.
pub fn git(cwd: &Path, args: &[&str]) -> String {
    let output = git_output(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Whether this git installation ships `git daemon`.
pub fn daemon_available() -> bool {
    let Ok(output) = Command::new("git").arg("--exec-path").output() else {
        return false;
    };
    let exec_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    output.status.success() && Path::new(&exec_path).join("git-daemon").exists()
}

/// A free loopback port. Another process could take it before the caller
/// binds; callers retry on a failed start.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("bind an ephemeral loopback port")
}

/// `origin.git` (bare) served at [`Origin::url`], plus a working `seed` clone
/// the test uses to shape and later move the origin's branches.
pub struct Origin {
    dir: tempfile::TempDir,
    daemon: Child,
    pub url: String,
}

impl Origin {
    /// Create the origin with one commit on `main` and serve it. `None` (after
    /// printing why) when `git daemon` is unavailable or cannot be started, so
    /// the caller skips rather than fails.
    pub fn start() -> Option<Self> {
        if !daemon_available() {
            eprintln!("SKIP: `git daemon` is not available");
            return None;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let srv = dir.path().join("srv");
        let seed = dir.path().join("seed");
        std::fs::create_dir_all(&srv).unwrap();
        std::fs::create_dir_all(seed.join(".awman")).unwrap();
        std::fs::create_dir_all(seed.join("aspec/work-items")).unwrap();
        git(&srv, &["init", "-q", "--bare", "-b", "main", "origin.git"]);

        std::fs::write(seed.join("Dockerfile.dev"), "FROM scratch\n").unwrap();
        std::fs::write(seed.join(".awman/Dockerfile.claude"), "FROM scratch\n").unwrap();
        std::fs::write(
            seed.join("aspec/work-items/0042-test.md"),
            "# Work item 42\n",
        )
        .unwrap();
        std::fs::write(seed.join("shared.txt"), "line one\n").unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        git(&seed, &["add", "-A"]);
        commit(&seed, "base");
        let bare = srv.join("origin.git");
        git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&seed, &["push", "-q", "origin", "main"]);

        for _ in 0..3 {
            let port = free_port();
            let mut daemon = Command::new("git")
                .args([
                    "daemon",
                    "--listen=127.0.0.1",
                    &format!("--port={port}"),
                    &format!("--base-path={}", srv.display()),
                    "--export-all",
                    "--enable=receive-pack",
                    "--reuseaddr",
                    "--informative-errors",
                ])
                .arg(&srv)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn git daemon");
            let url = format!("git://127.0.0.1:{port}/origin.git");
            if wait_until_served(&mut daemon, dir.path(), &url) {
                return Some(Self { dir, daemon, url });
            }
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        eprintln!("SKIP: `git daemon` did not start on loopback");
        None
    }

    /// The working clone the fixture shapes the origin from.
    pub fn seed(&self) -> PathBuf {
        self.dir.path().join("seed")
    }

    /// Commit `files` on `branch` (created from `from` when given) in the seed
    /// and push it to the origin. Returns the new commit's sha.
    pub fn commit_on(
        &self,
        branch: &str,
        from: Option<&str>,
        files: &[(&str, &str)],
        message: &str,
    ) -> String {
        let seed = self.seed();
        match from {
            Some(start) => git(&seed, &["checkout", "-q", "-B", branch, start]),
            None => git(&seed, &["checkout", "-q", branch]),
        };
        for (path, contents) in files {
            std::fs::write(seed.join(path), contents).unwrap();
        }
        git(&seed, &["add", "-A"]);
        commit(&seed, message);
        git(&seed, &["push", "-q", "origin", branch]);
        git(&seed, &["rev-parse", "HEAD"])
    }

    /// Create and push a lightweight tag at `rev`.
    pub fn tag(&self, name: &str, rev: &str) {
        let seed = self.seed();
        git(&seed, &["tag", name, rev]);
        git(
            &seed,
            &["push", "-q", "origin", &format!("refs/tags/{name}")],
        );
    }

    /// Every ref the daemon advertises, read over `git://` with
    /// `git ls-remote`: refname → sha.
    pub fn ls_remote(&self) -> BTreeMap<String, String> {
        git(self.dir.path(), &["ls-remote", &self.url])
            .lines()
            .filter_map(|line| {
                let (sha, name) = line.split_once('\t')?;
                Some((name.to_string(), sha.to_string()))
            })
            .collect()
    }

    /// The sha of `refs/heads/<branch>` on the origin, per `git ls-remote`.
    pub fn branch_sha(&self, branch: &str) -> Option<String> {
        self.ls_remote().remove(&format!("refs/heads/{branch}"))
    }

    /// Run git against the bare origin repository itself.
    pub fn git_bare(&self, args: &[&str]) -> String {
        let bare = self.dir.path().join("srv/origin.git");
        git(&bare, args)
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn commit(repo: &Path, message: &str) {
    let mut args: Vec<&str> = FIXTURE_IDENTITY.to_vec();
    args.extend(["commit", "-q", "-m", message]);
    git(repo, &args);
}

/// Poll `git ls-remote <url>` until the daemon answers, or it has exited, or
/// ten seconds pass.
fn wait_until_served(daemon: &mut Child, cwd: &Path, url: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if matches!(daemon.try_wait(), Ok(Some(_))) {
            return false;
        }
        if git_output(cwd, &["ls-remote", url]).status.success() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}
