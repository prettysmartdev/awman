//! The seams the production driver runs through, besides `Forge` and
//! `NativeHost`: the local source clone, the gates-owned JSON tools, time and
//! the operator prompt. Production implementations live here; the
//! fault-injection world implements the same traits in `forge_dir.rs`.

use super::error::{Step, Stop};
use super::forge::Forge;
use super::native::NativeHost;
use super::plan::NativePlan;
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct Head {
    pub commit: String,
    pub branch: String,
    pub clean: bool,
}

pub trait Source {
    /// `owner/name` of `origin`.
    fn origin_repo(&self) -> Step<String>;
    fn head(&self) -> Step<Head>;
    fn tree(&self, commit: &str) -> Step<String>;
    fn file_at(&self, commit: &str, path: &str) -> Step<Option<Vec<u8>>>;
    /// Fetch `origin/<branch>` and return its commit.
    fn fetch(&self, branch: &str) -> Step<String>;
    fn is_ancestor(&self, ancestor: &str, commit: &str) -> Step<bool>;
    /// A detached, read-only checkout of `commit` at `dir` (reused if valid).
    fn checkout(&self, commit: &str, dir: &Path) -> Step<PathBuf>;
    fn remove_checkout(&self, dir: &Path);
    fn config_get(&self, key: &str) -> Option<String>;
    fn config_set(&self, key: &str, value: &str);
}

/// Verified CI evidence, `gates verify-release-evidence` schema 1 (design 7.2).
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct VerifiedCi {
    pub schema: u32,
    pub outcome: String,
    #[serde(default)]
    pub reason_code: Option<String>,
    pub repository_id: u64,
    pub run_id: u64,
    pub run_attempt: u32,
    pub commit: String,
    pub tree: String,
    pub manifest_sha256: String,
    pub plan_sha256: String,
    pub policy_sha256: String,
    pub workflow: serde_json::Value,
    pub cells: Vec<serde_json::Value>,
    pub artifacts: Vec<serde_json::Value>,
}

pub trait Tools {
    /// `gates release-plan --root <root>` stdout.
    fn release_plan(&self, root: &Path) -> Step<Vec<u8>>;
    /// `gates verify-release-evidence …`; returns the exit class and output.
    fn verify_ci(
        &self,
        root: &Path,
        plan: &Path,
        repo: &str,
        run_id: u64,
        out: &Path,
    ) -> Step<(i32, Vec<u8>)>;
}

pub trait Clock {
    fn now(&self) -> DateTime<Utc>;
    fn sleep(&self, seconds: u64);
}

pub trait Prompt {
    /// Show the summary and ask for publication confirmation.
    fn confirm(&self, summary: &str) -> bool;
}

/// Creates the host for one native check. Production chooses `LocalHost`
/// for `host = "local"` and `SshHost` for `host = "ssh"`.
pub trait Hosts {
    fn host(&self, plan: &NativePlan, linux_host: Option<&str>) -> Step<Box<dyn NativeHost>>;
}

pub struct World {
    pub forge: Box<dyn Forge>,
    pub source: Box<dyn Source>,
    pub tools: Box<dyn Tools>,
    pub clock: Box<dyn Clock>,
    pub prompt: Box<dyn Prompt>,
    pub hosts: Box<dyn Hosts>,
    pub interrupted: Arc<AtomicBool>,
    /// The real GitHub/hosts (tool checks apply); false in the fault world.
    pub production: bool,
    /// Poll interval for hosted runs.
    pub poll_seconds: u64,
}

impl World {
    pub fn interrupted(&self) -> Step {
        if self.interrupted.load(Ordering::SeqCst) {
            return Err(Stop::new(
                "interrupted",
                "interrupted by the operator; completed work is kept",
            ));
        }
        Ok(())
    }
}

// ── production implementations ───────────────────────────────────────────────

pub struct GitSource {
    pub root: PathBuf,
}

impl GitSource {
    fn git(&self, args: &[&str]) -> Step<String> {
        let out = Command::new("git")
            .current_dir(&self.root)
            .args(args)
            .output()
            .map_err(|e| Stop::new("tool-missing", format!("git: {e}")))?;
        if !out.status.success() {
            return Err(Stop::new(
                "state-corrupt",
                format!(
                    "git {}: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// `https://github.com/o/n(.git)`, `git@github.com:o/n(.git)`, `ssh://…/o/n`.
pub fn repo_from_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.split('/');
    let (o, n) = (parts.next()?, parts.next()?);
    (parts.next().is_none() && !o.is_empty() && !n.is_empty()).then(|| format!("{o}/{n}"))
}

impl Source for GitSource {
    fn origin_repo(&self) -> Step<String> {
        let url = self.git(&["remote", "get-url", "origin"])?;
        repo_from_url(&url).ok_or_else(|| {
            Stop::new(
                "wrong-repo",
                format!("origin {url} is not a GitHub repository"),
            )
        })
    }

    fn head(&self) -> Step<Head> {
        Ok(Head {
            commit: self.git(&["rev-parse", "HEAD"])?,
            branch: self.git(&["branch", "--show-current"])?,
            clean: self
                .git(&["status", "--porcelain", "--untracked-files=no"])?
                .is_empty(),
        })
    }

    fn tree(&self, commit: &str) -> Step<String> {
        self.git(&["rev-parse", &format!("{commit}^{{tree}}")])
    }

    fn file_at(&self, commit: &str, path: &str) -> Step<Option<Vec<u8>>> {
        let out = Command::new("git")
            .current_dir(&self.root)
            .args(["show", &format!("{commit}:{path}")])
            .output()
            .map_err(|e| Stop::new("tool-missing", format!("git: {e}")))?;
        Ok(out.status.success().then_some(out.stdout))
    }

    fn fetch(&self, branch: &str) -> Step<String> {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.root)
            .args(["fetch", "--quiet", "origin", branch])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // A network call: bounded like every other external operation.
        let out = super::util::output_bounded(&mut cmd, None, std::time::Duration::from_secs(300))
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::TimedOut {
                    Stop::new("network", format!("git fetch origin {branch}: {e}"))
                } else {
                    Stop::new("tool-missing", format!("git: {e}"))
                }
            })?;
        if !out.status.success() {
            return Err(Stop::new(
                "network",
                format!(
                    "git fetch origin {branch}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        self.git(&["rev-parse", "FETCH_HEAD"])
    }

    fn is_ancestor(&self, ancestor: &str, commit: &str) -> Step<bool> {
        Ok(Command::new("git")
            .current_dir(&self.root)
            .args(["merge-base", "--is-ancestor", ancestor, commit])
            .status()
            .map_err(|e| Stop::new("tool-missing", format!("git: {e}")))?
            .success())
    }

    fn checkout(&self, commit: &str, dir: &Path) -> Step<PathBuf> {
        if dir.join(".git").exists() {
            let head = Command::new("git")
                .current_dir(dir)
                .args(["rev-parse", "HEAD"])
                .output();
            let clean = Command::new("git")
                .current_dir(dir)
                .args(["status", "--porcelain"])
                .output();
            if let (Ok(h), Ok(c)) = (head, clean) {
                if String::from_utf8_lossy(&h.stdout).trim() == commit && c.stdout.is_empty() {
                    return Ok(dir.to_path_buf());
                }
            }
            self.remove_checkout(dir);
        }
        self.git(&[
            "worktree",
            "add",
            "--force",
            "--detach",
            &dir.to_string_lossy(),
            commit,
        ])?;
        Ok(dir.to_path_buf())
    }

    fn remove_checkout(&self, dir: &Path) {
        let _ = Command::new("git")
            .current_dir(&self.root)
            .args(["worktree", "remove", "--force"])
            .arg(dir)
            .output();
    }

    fn config_get(&self, key: &str) -> Option<String> {
        self.git(&["config", "--local", "--get", key])
            .ok()
            .filter(|v| !v.is_empty())
    }

    fn config_set(&self, key: &str, value: &str) {
        let _ = self.git(&["config", "--local", key, value]);
    }
}

/// Runs this executable's own `gates` commands (design 5.4: JSON, no Rust
/// dependency on the manifest modules).
pub struct ExeTools;

impl Tools for ExeTools {
    fn release_plan(&self, root: &Path) -> Step<Vec<u8>> {
        let exe = std::env::current_exe().map_err(|e| Stop::new("tool-missing", e.to_string()))?;
        let out = Command::new(exe)
            .args(["gates", "release-plan", "--root"])
            .arg(root)
            .output()
            .map_err(|e| Stop::new("tool-missing", e.to_string()))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let line = err
                .lines()
                .last()
                .unwrap_or("release-plan failed")
                .trim()
                .to_string();
            let code = if line.contains("pending") || line.contains("pin") {
                "pin-pending"
            } else {
                "manifest-stale"
            };
            return Err(Stop::new(code, line));
        }
        Ok(out.stdout)
    }

    fn verify_ci(
        &self,
        root: &Path,
        plan: &Path,
        repo: &str,
        run_id: u64,
        out: &Path,
    ) -> Step<(i32, Vec<u8>)> {
        let exe = std::env::current_exe().map_err(|e| Stop::new("tool-missing", e.to_string()))?;
        let status = Command::new(exe)
            .args(["gates", "verify-release-evidence", "--root"])
            .arg(root)
            .arg("--plan")
            .arg(plan)
            .args(["--repo", repo, "--run-id", &run_id.to_string(), "--out"])
            .arg(out)
            .status()
            .map_err(|e| Stop::new("tool-missing", e.to_string()))?;
        let code = status.code().unwrap_or(2);
        let bytes = std::fs::read(out).unwrap_or_default();
        Ok((code, bytes))
    }
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
    fn sleep(&self, seconds: u64) {
        std::thread::sleep(std::time::Duration::from_secs(seconds));
    }
}

pub struct TtyPrompt {
    pub yes: bool,
}

impl Prompt for TtyPrompt {
    fn confirm(&self, summary: &str) -> bool {
        println!("{summary}");
        if self.yes {
            println!("Publication confirmed by YES=1.");
            return true;
        }
        print!("Publish this release? [y/N] ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_urls_resolve_to_owner_and_name_only() {
        for url in [
            "https://github.com/prettysmartdev/awman.git",
            "git@github.com:prettysmartdev/awman.git",
            "ssh://git@github.com/prettysmartdev/awman",
        ] {
            assert_eq!(repo_from_url(url).as_deref(), Some("prettysmartdev/awman"));
        }
        assert_eq!(repo_from_url("https://example.com/a/b"), None);
        assert_eq!(repo_from_url("https://github.com/a/b/c"), None);
    }
}
