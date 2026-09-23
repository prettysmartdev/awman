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
        let home = match env.config_home() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_is_panic_log_under_dot_awman() {
        let log = PanicLog::at_home("/home/someone");
        assert_eq!(
            log.path(),
            std::path::Path::new("/home/someone/.awman/panic.log")
        );
    }

    #[test]
    fn append_creates_the_file_and_its_parent_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let log = PanicLog::at_home(tmp.path());
        assert!(!log.path().exists());

        assert!(log.append("first report\n"));

        assert_eq!(
            std::fs::read_to_string(log.path()).unwrap(),
            "first report\n"
        );
    }

    #[test]
    fn append_appends_rather_than_truncating() {
        let tmp = tempfile::tempdir().unwrap();
        let log = PanicLog::at_home(tmp.path());

        assert!(log.append("first\n"));
        assert!(log.append("second\n"));

        assert_eq!(
            std::fs::read_to_string(log.path()).unwrap(),
            "first\nsecond\n"
        );
    }

    #[test]
    fn append_reports_failure_instead_of_panicking_on_an_unwritable_path() {
        let tmp = tempfile::tempdir().unwrap();
        // A regular file where the `.awman` directory would go: `create_dir_all`
        // cannot succeed, so the append has nowhere to write.
        std::fs::write(tmp.path().join(".awman"), "not a directory").unwrap();
        let log = PanicLog::at_home(tmp.path());

        assert!(
            !log.append("report\n"),
            "an unwritable path must report false, never panic"
        );
    }

    #[test]
    fn from_env_prefers_the_configured_home() {
        let env = EnvSnapshot::with_overrides([(
            crate::data::config::env::AWMAN_CONFIG_HOME,
            "/tmp/awman-home",
        )]);
        let log = PanicLog::from_env(&env).expect("a configured home always resolves");
        assert_eq!(
            log.path(),
            std::path::Path::new("/tmp/awman-home/.awman/panic.log")
        );
    }
}
