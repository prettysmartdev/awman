use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::data::config::env::EnvSnapshot;
use crate::data::fs::data_paths::AWMAN_HOME;
use crate::data::session::SessionOpenOptions;

/// A frontend that closes the show dialog immediately.
struct Quiet;

impl UserMessageSink for Quiet {
    fn write_message(&mut self, _msg: UserMessage) {}
    fn replay_queued(&mut self) {}
}

impl ConfigCommandFrontend for Quiet {
    fn present_config_table(
        &mut self,
        _rows: &[ConfigFieldRow],
        _rejected: Option<&ConfigEditRejection>,
    ) -> Result<Option<ConfigEditRequest>, CommandError> {
        Ok(None)
    }
}

/// No daemon running.
struct NoDaemons;

impl DaemonReloader for NoDaemons {
    fn running(&self, _: &Session) -> Vec<RunningDaemon> {
        Vec::new()
    }
    fn reload(&self, _: &RunningDaemon) -> Result<ReloadReply, String> {
        unreachable!("nothing is running")
    }
}

/// One reachable daemon that applies everything.
struct OneDaemon;

impl DaemonReloader for OneDaemon {
    fn running(&self, _: &Session) -> Vec<RunningDaemon> {
        vec![RunningDaemon {
            name: "api",
            local_endpoint: None,
            base_url: "https://127.0.0.1:9876".into(),
            bearer: Some("k".into()),
            pinned_cert_pem: None,
            reachable: true,
        }]
    }
    fn reload(&self, _: &RunningDaemon) -> Result<ReloadReply, String> {
        Ok(ReloadReply::default())
    }
}

struct Fixture {
    repo: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            repo: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn env(&self) -> EnvSnapshot {
        EnvSnapshot::with_overrides([(AWMAN_HOME, self.home.path().to_str().unwrap())])
    }

    fn session(&self) -> Session {
        Session::for_tests_with_env(self.repo.path(), self.env())
    }

    fn session_outside_a_repo(&self) -> Session {
        Session::open_at_directory_root(
            self.repo.path().to_path_buf(),
            SessionOpenOptions {
                env: Some(self.env()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn global_file(&self) -> std::path::PathBuf {
        self.home.path().join("config.json")
    }

    fn repo_file(&self) -> std::path::PathBuf {
        self.repo.path().join(".awman").join("config.json")
    }
}

fn read(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

async fn run(session: Session, sub: ConfigSubcommand) -> Result<ConfigOutcome, CommandError> {
    run_with(session, sub, Arc::new(NoDaemons)).await
}

async fn run_with(
    session: Session,
    sub: ConfigSubcommand,
    reloader: Arc<dyn DaemonReloader>,
) -> Result<ConfigOutcome, CommandError> {
    ConfigCommand::new(sub, session)
        .with_reloader(reloader)
        .run_with_frontend(Box::new(Quiet))
        .await
}

fn set(field: &str, value: &str, target: ConfigTarget) -> ConfigSubcommand {
    ConfigSubcommand::Set(ConfigSetFlags {
        field: field.into(),
        value: value.into(),
        target,
    })
}

fn unset(field: &str, target: ConfigTarget) -> ConfigSubcommand {
    ConfigSubcommand::Unset(ConfigUnsetFlags {
        field: field.into(),
        target,
    })
}

fn set_outcome(outcome: ConfigOutcome) -> ConfigSetOutcome {
    match outcome {
        ConfigOutcome::Set(o) | ConfigOutcome::Unset(o) => o,
        other => panic!("expected a set/unset outcome, got {other:?}"),
    }
}

// ── automatic scope ─────────────────────────────────────────────────────────

#[tokio::test]
async fn set_picks_scope_automatically() {
    let fx = Fixture::new();

    // A global-scope key goes to the global file, even inside a repository.
    let o = set_outcome(
        run(
            fx.session(),
            set("runtime.kind", "docker", ConfigTarget::Auto),
        )
        .await
        .unwrap(),
    );
    assert_eq!(o.scope, "global");
    assert_eq!(o.path, fx.global_file().display().to_string());
    assert_eq!(read(&fx.global_file())["runtime"]["kind"], "docker");

    // A repo- or task-scope key goes to the repo file inside a repository.
    let o = set_outcome(
        run(
            fx.session(),
            set("agents.stuckTimeout", "45s", ConfigTarget::Auto),
        )
        .await
        .unwrap(),
    );
    assert_eq!(o.scope, "repo");
    assert_eq!(o.path, fx.repo_file().display().to_string());
    assert_eq!(read(&fx.repo_file())["agents"]["stuckTimeout"], "45s");

    // ... and to the global file outside one.
    let o = set_outcome(
        run(
            fx.session_outside_a_repo(),
            set("tui.scrollbackLines", "7", ConfigTarget::Auto),
        )
        .await
        .unwrap(),
    );
    assert_eq!(o.scope, "global");
    assert_eq!(read(&fx.global_file())["tui"]["scrollbackLines"], 7);
}

#[tokio::test]
async fn explicit_scope_flags_force_the_file_and_are_checked_against_the_key() {
    let fx = Fixture::new();
    let o = set_outcome(
        run(
            fx.session(),
            set("agents.stuckTimeout", "45s", ConfigTarget::Global),
        )
        .await
        .unwrap(),
    );
    assert_eq!(o.path, fx.global_file().display().to_string());

    let err = run(
        fx.session(),
        set("runtime.kind", "docker", ConfigTarget::Repo),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("may only be set in"), "{err}");

    let err = run(
        fx.session_outside_a_repo(),
        set("agents.stuckTimeout", "1s", ConfigTarget::Repo),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("--repo needs a Git repository"), "{err}");

    // Task scope admits task keys only, and the task must exist.
    let err = run(
        fx.session(),
        set("runtime.kind", "docker", ConfigTarget::Task("t".into())),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("may only be set in"), "{err}");
    let err = run(
        fx.session(),
        set(
            "agents.stuckTimeout",
            "1s",
            ConfigTarget::Task("nope".into()),
        ),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("no squad task named `nope`"), "{err}");
}

#[tokio::test]
async fn set_parses_by_type_and_rejects_unknown_keys_with_the_hint() {
    let fx = Fixture::new();
    run(
        fx.session(),
        set("overlays", "ssh(), env(T)", ConfigTarget::Auto),
    )
    .await
    .unwrap();
    assert_eq!(
        read(&fx.repo_file())["overlays"],
        serde_json::json!(["ssh()", "env(T)"])
    );

    let err = run(
        fx.session(),
        set("agents.launchMode", "banana", ConfigTarget::Auto),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("invalid value for `agents.launchMode`"),
        "{err}"
    );

    let err = run(
        fx.session(),
        set("agentStuckTimeout", "30s", ConfigTarget::Auto),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("unknown config key `agentStuckTimeout`"),
        "{err}"
    );
    assert!(err.contains("agents.stuckTimeout"), "{err}");

    let err = run(
        fx.session(),
        set("runtime.builtin.imageSource", "x", ConfigTarget::Auto),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("is an object"), "{err}");
}

#[tokio::test]
async fn unset_removes_a_leaf_or_a_whole_map_entry() {
    let fx = Fixture::new();
    run(
        fx.session(),
        set("agents.stuckTimeout", "45s", ConfigTarget::Auto),
    )
    .await
    .unwrap();
    run(
        fx.session(),
        set("tui.scrollbackLines", "9", ConfigTarget::Auto),
    )
    .await
    .unwrap();
    let o = set_outcome(
        run(
            fx.session(),
            unset("agents.stuckTimeout", ConfigTarget::Auto),
        )
        .await
        .unwrap(),
    );
    assert_eq!((o.scope.as_str(), o.value), ("repo", None));
    let doc = read(&fx.repo_file());
    assert!(doc.pointer("/agents/stuckTimeout").is_none(), "{doc}");
    assert_eq!(doc["tui"]["scrollbackLines"], 9);

    run(
        fx.session(),
        set("workServers.in-fact.url", "https://h:1", ConfigTarget::Auto),
    )
    .await
    .unwrap();
    run(
        fx.session(),
        set("workServers.in-fact.claim", "manual", ConfigTarget::Auto),
    )
    .await
    .unwrap();
    assert_eq!(
        read(&fx.global_file())["workServers"]["in-fact"]["claim"],
        "manual"
    );
    run(
        fx.session(),
        unset("workServers.in-fact", ConfigTarget::Auto),
    )
    .await
    .unwrap();
    let doc = read(&fx.global_file());
    assert!(doc.pointer("/workServers/in-fact").is_none(), "{doc}");
}

// ── live apply ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn set_reports_live_apply() {
    let fx = Fixture::new();
    let o = set_outcome(
        run(
            fx.session(),
            set("daemon.maxConcurrentJobs", "3", ConfigTarget::Auto),
        )
        .await
        .unwrap(),
    );
    assert_eq!(o.live, LiveApply::NextStart);

    let o = set_outcome(
        run_with(
            fx.session(),
            set("daemon.maxConcurrentJobs", "4", ConfigTarget::Auto),
            Arc::new(OneDaemon),
        )
        .await
        .unwrap(),
    );
    assert_eq!(o.live, LiveApply::Applied);

    let o = set_outcome(
        run_with(
            fx.session(),
            set(
                "daemon.network.listen",
                "127.0.0.1:9999",
                ConfigTarget::Auto,
            ),
            Arc::new(OneDaemon),
        )
        .await
        .unwrap(),
    );
    assert_eq!(o.live, LiveApply::RestartRequired);
}

// ── show ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn show_masks_sensitive_values_and_names_the_state_file() {
    let fx = Fixture::new();
    std::fs::write(
        fx.global_file(),
        r#"{"remote":{"targets":{"prod":{"url":"https://h:1","apiKey":"secret-value-1234"}}}}"#,
    )
    .unwrap();
    let outcome = run(
        fx.session(),
        ConfigSubcommand::Show(ConfigShowFlags { json: false }),
    )
    .await
    .unwrap();
    let ConfigOutcome::Show(show) = outcome else {
        panic!("expected show")
    };
    let key = show
        .rows
        .iter()
        .find(|r| r.field == "remote.targets.prod.apiKey")
        .expect("map entry row");
    assert!(key.sensitive);
    assert_eq!(key.global_value.as_deref(), Some("••••1234"));
    assert_eq!(key.effective_value.as_deref(), Some("••••1234"));
    let json = serde_json::to_string(&show).unwrap();
    assert!(!json.contains("secret-value"), "{json}");

    let url = show
        .rows
        .iter()
        .find(|r| r.field == "remote.targets.prod.url")
        .unwrap();
    assert_eq!(url.source, Layer::Global);
    let stuck = show
        .rows
        .iter()
        .find(|r| r.field == "agents.stuckTimeout")
        .unwrap();
    assert_eq!(
        (stuck.source, stuck.type_name.as_str()),
        (Layer::Default, "duration")
    );

    let state = show.state.expect("a repository has a state section");
    assert!(state.path.ends_with("state.json"), "{}", state.path);
    assert!(
        state.path.starts_with(fx.home.path().to_str().unwrap()),
        "{}",
        state.path
    );
    assert_eq!(state.auto_agent_auth_accepted, None);
    assert!(show.notes.iter().any(|n| n.contains("experimental")));
}

#[tokio::test]
async fn show_outside_a_repo_has_no_state_section() {
    let fx = Fixture::new();
    let ConfigOutcome::Show(show) = run(
        fx.session_outside_a_repo(),
        ConfigSubcommand::Show(ConfigShowFlags { json: false }),
    )
    .await
    .unwrap() else {
        panic!("expected show")
    };
    assert!(show.state.is_none());
}

#[tokio::test]
async fn get_reports_the_layer_of_the_effective_value() {
    let fx = Fixture::new();
    let mut session = fx.session();
    session
        .add_overrides(
            crate::data::config::overrides::ConfigOverrides::parse_set_args(&[
                "agents.stuckTimeout=5s".to_string(),
            ])
            .unwrap(),
        )
        .unwrap();
    let ConfigOutcome::Get(get) = run(
        session,
        ConfigSubcommand::Get(ConfigGetFlags {
            field: "agents.stuckTimeout".into(),
            json: false,
        }),
    )
    .await
    .unwrap() else {
        panic!("expected get")
    };
    assert_eq!(get.effective_value.as_deref(), Some("5s"));
    assert_eq!(get.source, Layer::Set);
}

#[test]
fn rows_follow_the_key_table_and_list_map_entries() {
    let global = serde_json::json!({
        "agents": {"flags": {"codex": ["--x"]}},
        "remote": {"targets": {"prod": {"url": "https://h:1", "apiKey": "secret-value-1234"}}}
    });
    let repo = serde_json::json!({});
    let source = |_: &str| Layer::Default;
    let rows = collect_config_rows(&ConfigDocs {
        global: &global,
        repo: &repo,
        effective: &global,
        source: &source,
    });
    assert_eq!(rows[0].field, "agents.default");
    assert!(rows.iter().any(|r| r.field == "agents.flags"
        && r.shape
            == ConfigRowKind::MapHeader {
                child_prefix: "agents.flags."
            }));
    assert!(rows.iter().any(|r| r.field == "agents.flags.codex"));
    let runtime = rows.iter().find(|r| r.field == "runtime.kind").unwrap();
    assert!(runtime.global_writable && !runtime.repo_writable);
}
