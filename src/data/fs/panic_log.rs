//! `PanicLog` — the file TUI panics are appended to.
//!
//! Layer 0 owns the path and the write; the panic hook in Layer 3 owns the
//! formatting (F-44). Splitting it that way is what makes the append
//! testable: a hook cannot be invoked from a test without panicking, but
//! `PanicLog::append` can.

use std::path::{Path, PathBuf};

use crate::data::config::env::EnvSnapshot;

/// `$HOME/.awman/panic.log`.
///
/// Every method is best-effort and silent: this type only ever runs inside a
/// panic hook, where an error path that itself panics, blocks, or prints
/// would replace the report the user needs with noise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanicLog {
    path: PathBuf,
}

impl PanicLog {
    /// Resolve the log under the home directory `env` describes.
    ///
    /// `None` when there is no home directory to resolve — a panic still
    /// aborts the TUI cleanly, it just leaves no file behind.
    pub fn from_env(env: &EnvSnapshot) -> Option<Self> {
        let home = match crate::data::fs::data_paths::awman_home(env) {
            Some(dir) => dir,
            None => dirs::home_dir()?,
        };
        Some(Self::at_home(home))
    }

    /// The log under an explicit home directory.
    pub fn at_home(home: impl AsRef<Path>) -> Self {
        Self {
            path: home.as_ref().join(".awman").join("panic.log"),
        }
    }

    /// Where reports are written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one already-formatted report, creating the file and its parent
    /// directory if needed.
    ///
    /// Returns `false` when nothing could be written (unwritable path, no
    /// permission, read-only filesystem). Never panics and never blocks on an
    /// error — the caller is a panic hook.
    pub fn append(&self, report: &str) -> bool {
        use std::io::Write as _;

        if let Some(parent) = self.path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return false;
            }
        }
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return false;
        };
        file.write_all(report.as_bytes()).is_ok()
    }
}
