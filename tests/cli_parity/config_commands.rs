//! WI 0139: `awman config [show] | get | set | unset`, driven by the key table.

use std::sync::Arc;

use awman::command::commands::config::{
    ConfigCommand, ConfigCommandFrontend, ConfigEditRejection, ConfigEditRequest, ConfigFieldRow,
    ConfigOutcome, ConfigSetFlags, ConfigShowFlags, ConfigSubcommand, ConfigTarget, DaemonReloader,
    LiveApply, ReloadReply, RunningDaemon,
};
use awman::command::commands::Command;
use awman::command::error::CommandError;
use awman::data::config::env::{EnvSnapshot, AWMAN_HOME};
use awman::data::message::{UserMessage, UserMessageSink};
use awman::data::session::{Session, SessionOpenOptions};

struct Quiet;
impl UserMessageSink for Quiet {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}
impl ConfigCommandFrontend for Quiet {
    fn present_config_table(
        &mut self,
        _: &[ConfigFieldRow],
        _: Option<&ConfigEditRejection>,
    ) -> Result<Option<ConfigEditRequest>, CommandError> {
        Ok(None)
    }
}

struct NoDaemons;
impl DaemonReloader for NoDaemons {
    fn running(&self, _: &Session) -> Option<RunningDaemon> {
        None
    }
    fn reload(&self, _: &RunningDaemon) -> Result<ReloadReply, String> {
        unreachable!()
    }
}

fn session(repo: &std::path::Path, home: &std::path::Path) -> Session {
    Session::open_at_git_root(
        repo.to_path_buf(),
        repo.to_path_buf(),
        SessionOpenOptions {
            env: Some(EnvSnapshot::with_overrides([(
                AWMAN_HOME,
                home.to_str().unwrap(),
            )])),
            ..Default::default()
        },
    )
    .unwrap()
}

async fn run(session: Session, sub: ConfigSubcommand) -> ConfigOutcome {
    ConfigCommand::new(sub, session)
        .with_reloader(Arc::new(NoDaemons))
        .run_with_frontend(Box::new(Quiet))
        .await
        .unwrap()
}

#[tokio::test]
async fn set_picks_scope_automatically() {
    let (repo, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for (key, value, scope, file) in [
        (
            "daemon.maxConcurrentJobs",
            "3",
            "global",
            home.path().join("config.json"),
        ),
        (
            "agents.launchMode",
            "acp",
            "repo",
            repo.path().join(".awman").join("config.json"),
        ),
        (
            "tui.backend",
            "daemon",
            "repo",
            repo.path().join(".awman").join("config.json"),
        ),
        (
            "daemon.features.engine.enabled",
            "true",
            "global",
            home.path().join("config.json"),
        ),
    ] {
        let outcome = run(
            session(repo.path(), home.path()),
            ConfigSubcommand::Set(ConfigSetFlags {
                field: key.into(),
                value: value.into(),
                target: ConfigTarget::Auto,
            }),
        )
        .await;
        let ConfigOutcome::Set(set) = outcome else {
            panic!("expected set")
        };
        assert_eq!(set.scope, scope, "{key}");
        assert_eq!(set.path, file.display().to_string(), "{key}");
        assert_eq!(set.live, LiveApply::NextStart);
        assert!(file.exists(), "{key} must be written to {}", file.display());
    }
}

#[tokio::test]
async fn show_masks_secrets_and_has_a_state_heading() {
    let (repo, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(
        home.path().join("config.json"),
        r#"{"remote":{"default":"p","targets":{"p":{"url":"https://h:1","apiKey":"sk-abcdefgh-9876"}}}}"#,
    )
    .unwrap();
    let ConfigOutcome::Show(show) = run(
        session(repo.path(), home.path()),
        ConfigSubcommand::Show(ConfigShowFlags { json: true }),
    )
    .await
    else {
        panic!("expected show")
    };
    let text = serde_json::to_string(&show).unwrap();
    assert!(!text.contains("sk-abcdefgh"), "{text}");
    assert!(
        text.contains("9876"),
        "the last four characters stay visible: {text}"
    );
    let state = show.state.expect("state heading inside a repository");
    assert!(state.path.ends_with("state.json"));
}
