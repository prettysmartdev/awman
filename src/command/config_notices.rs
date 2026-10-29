//! Config deprecation notices shown once per process per repository.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;

/// The `dynamicWorkflows` deprecation notice (WI 0150 §3).
pub const LEGACY_DYNAMIC_WORKFLOWS_NOTICE: &str = "`.awman/config.json` uses `dynamicWorkflows`, \
     which has moved to `workflows.dynamic`. It still works. Run `awman config migrate` to move it.";

/// Notices that must be shown once per process per repository.
///
/// Lives on `Engines`, which is built once per CLI/TUI process and once per
/// API/squad daemon, so a daemon warns at the first command for a repo and
/// not on every scheduler tick. Never a process-global (F-38). Every frontend
/// receives the notice through its sink: CLI stderr, TUI toast, API
/// `StatusMessage`.
#[derive(Debug, Default)]
pub struct ConfigNotices {
    warned_legacy_dynamic: Mutex<HashSet<PathBuf>>,
}

impl ConfigNotices {
    /// Emit the deprecation warning when the session's repo config still uses
    /// `dynamicWorkflows` and this git root has not been warned about yet.
    /// Returns whether it emitted.
    pub fn warn_legacy_dynamic_workflows(
        &self,
        session: &Session,
        sink: &mut dyn UserMessageSink,
    ) -> bool {
        if !session.repo_config().uses_legacy_dynamic_workflows() {
            return false;
        }
        let first = self
            .warned_legacy_dynamic
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session.git_root().to_path_buf());
        if first {
            sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: LEGACY_DYNAMIC_WORKFLOWS_NOTICE.into(),
            });
        }
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
    use crate::data::message::{MessageLevel, RecordingMessageSink};

    #[test]
    fn legacy_notice_emits_once_per_repo_for_process() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".awman")).unwrap();
        std::fs::write(
            root.path().join(".awman/config.json"),
            r#"{"dynamicWorkflows":{}}"#,
        )
        .unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, home.path().to_str().unwrap())]);
        let session = Session::for_tests_with_env(root.path(), env);
        let notices = ConfigNotices::default();
        let mut sink = RecordingMessageSink::new();
        assert!(notices.warn_legacy_dynamic_workflows(&session, &mut sink));
        assert!(!notices.warn_legacy_dynamic_workflows(&session, &mut sink));
        let warnings = sink
            .queued()
            .iter()
            .filter(|m| m.level == MessageLevel::Warning)
            .collect::<Vec<_>>();
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].text.contains("workflows.dynamic")
                && warnings[0].text.contains("config migrate")
        );
        let second_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(second_root.path().join(".awman")).unwrap();
        std::fs::write(
            second_root.path().join(".awman/config.json"),
            r#"{"dynamicWorkflows":{}}"#,
        )
        .unwrap();
        let second = Session::for_tests_with_env(
            second_root.path(),
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, home.path().to_str().unwrap())]),
        );
        assert!(notices.warn_legacy_dynamic_workflows(&second, &mut sink));
        assert_eq!(
            sink.queued()
                .iter()
                .filter(|m| m.level == MessageLevel::Warning)
                .count(),
            2
        );
    }

    /// A squad evaluation and the workflow it generates hold clones of one
    /// `Engines`, so the evaluation that warns first (triggered or not) is
    /// the only one that warns.
    #[test]
    fn engines_clones_share_one_gate() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".awman")).unwrap();
        std::fs::write(
            root.path().join(".awman/config.json"),
            r#"{"dynamicWorkflows":{}}"#,
        )
        .unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, home.path().to_str().unwrap())]);
        let session = Session::for_tests_with_env(root.path(), env);
        let evaluation = crate::command::dispatch::Engines::for_tests(root.path());
        let workflow = evaluation.clone();
        let mut sink = RecordingMessageSink::new();
        // Two evaluations that do not trigger, then one that runs a workflow.
        evaluation
            .config_notices
            .warn_legacy_dynamic_workflows(&session, &mut sink);
        evaluation
            .config_notices
            .warn_legacy_dynamic_workflows(&session, &mut sink);
        workflow
            .config_notices
            .warn_legacy_dynamic_workflows(&session, &mut sink);
        assert_eq!(sink.queued().len(), 1);
    }
}
