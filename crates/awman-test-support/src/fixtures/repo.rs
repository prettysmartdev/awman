//! Git repositories with a fixed author and fixed dates, so commit IDs are
//! identical on every run. Requires a component-or-higher sandbox (it runs
//! the real `git` CLI through the supervisor, on temporary repositories only).

use crate::TestSandbox;
use std::cell::Cell;
use std::path::{Path, PathBuf};

/// Builds a repository: `RepoBuilder::new(&sandbox, "app").file("README.md", "hi").init()`.
pub struct RepoBuilder<'a> {
    sandbox: &'a TestSandbox,
    path: PathBuf,
    files: Vec<(String, Vec<u8>)>,
    branch: String,
}

impl<'a> RepoBuilder<'a> {
    /// A repository at `<sandbox>/work/<name>`.
    pub fn new(sandbox: &'a TestSandbox, name: &str) -> RepoBuilder<'a> {
        RepoBuilder::at(sandbox, sandbox.work().join(name))
    }

    /// A repository at an explicit path inside the sandbox.
    pub fn at(sandbox: &'a TestSandbox, path: PathBuf) -> RepoBuilder<'a> {
        assert!(
            path.starts_with(sandbox.root()),
            "repositories are created inside the sandbox"
        );
        RepoBuilder {
            sandbox,
            path,
            files: Vec::new(),
            branch: "main".to_string(),
        }
    }

    /// A file of the initial commit.
    pub fn file(mut self, relative: &str, contents: impl AsRef<[u8]>) -> Self {
        self.files
            .push((relative.to_string(), contents.as_ref().to_vec()));
        self
    }

    /// The initial branch (default `main`).
    pub fn branch(mut self, name: &str) -> Self {
        self.branch = name.to_string();
        self
    }

    /// Create the repository; commits the files (if any) as "initial commit".
    pub fn init(self) -> Repo<'a> {
        std::fs::create_dir_all(&self.path).expect("create repository directory");
        let repo = Repo {
            sandbox: self.sandbox,
            path: self.path,
            commits: Cell::new(0),
        };
        repo.git(&["init", "-q", "-b", &self.branch]);
        if !self.files.is_empty() {
            for (relative, contents) in &self.files {
                repo.write(relative, contents);
            }
            repo.commit("initial commit");
        }
        repo
    }
}

/// A repository created by [`RepoBuilder`].
pub struct Repo<'a> {
    sandbox: &'a TestSandbox,
    path: PathBuf,
    commits: Cell<u32>,
}

impl<'a> Repo<'a> {
    /// The work tree.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run git in the work tree; panics with output unless it succeeds.
    /// Returns stdout, trimmed.
    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.path, args)
    }

    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        // Fixed dates: one minute per commit from 2026-01-01T00:00:00Z.
        let n = self.commits.get();
        let date = format!("2026-01-01T00:{:02}:00Z", n.min(59));
        let output = self
            .sandbox
            .command("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "awman test")
            .env("GIT_AUTHOR_EMAIL", "test@awman.invalid")
            .env("GIT_COMMITTER_NAME", "awman test")
            .env("GIT_COMMITTER_EMAIL", "test@awman.invalid")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .label(&format!("git {}", args.first().copied().unwrap_or("")))
            .run();
        output.assert_success();
        output.stdout_text().trim().to_string()
    }

    /// Write a file (creating parent directories).
    pub fn write(&self, relative: &str, contents: impl AsRef<[u8]>) -> &Self {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent directory");
        }
        std::fs::write(&path, contents).expect("write file");
        self
    }

    /// `git add -A` and commit; returns the commit ID.
    pub fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.commits.set(self.commits.get() + 1);
        self.head()
    }

    /// The commit ID of HEAD.
    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// Create a branch at HEAD (without switching).
    pub fn create_branch(&self, name: &str) -> &Self {
        self.git(&["branch", name]);
        self
    }

    /// Switch to a branch.
    pub fn checkout(&self, name: &str) -> &Self {
        self.git(&["checkout", "-q", name]);
        self
    }

    /// Add a worktree for a new branch at `<sandbox>/work/<name>`.
    pub fn worktree(&self, name: &str, branch: &str) -> PathBuf {
        let dir = self.sandbox.work().join(name);
        let dir_text = dir.to_string_lossy().into_owned();
        self.git(&["worktree", "add", "-q", "-b", branch, &dir_text]);
        dir
    }

    /// A bare clone at `<sandbox>/remotes/<name>.git`, added as remote `name`
    /// with every branch pushed.
    pub fn bare_remote(&self, name: &str) -> PathBuf {
        let remotes = self.sandbox.root().join("remotes");
        std::fs::create_dir_all(&remotes).expect("create remotes directory");
        let bare = remotes.join(format!("{name}.git"));
        let bare_text = bare.to_string_lossy().into_owned();
        self.git_in(
            &remotes,
            &["init", "-q", "--bare", "-b", "main", &bare_text],
        );
        self.git(&["remote", "add", name, &bare_text]);
        self.git(&["push", "-q", name, "--all"]);
        bare
    }

    /// Push `branch` to `remote`.
    pub fn push(&self, remote: &str, branch: &str) -> &Self {
        self.git(&["push", "-q", remote, branch]);
        self
    }
}
