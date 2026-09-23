//! squad bearer-key provisioning and disclosure.
//!
//! The daemon has always minted an `squad_key.hash` on its first start, but the
//! plaintext was printed as a bare line with no indication that the CLI and TUI
//! read it back out of `AWMAN_SQUAD_KEY`. These tests pin the two halves that
//! make the key usable: the export snippet `awman squad start` emits, and the
//! `--dangerously-skip-auth` path that opts out of a key entirely.

use std::path::Path;
use std::sync::{Arc, Mutex};

use awman::command::commands::squad::commands::SquadCommandFrontend;
use awman::command::commands::squad::daemon::{
    SquadDaemon, SquadDaemonSubcommand, SquadStartFlags,
};
use awman::command::commands::squad::key_setup::{export_snippet, ShellFlavor};
use awman::command::commands::squad::supervisor::SquadKeySetup;
use awman::command::dispatch::Engines;
use awman::command::error::CommandError;
use awman::data::config::env::{
    Env, EnvSnapshot, AWMAN_API_ROOT, AWMAN_CONFIG_HOME, AWMAN_SQUAD_KEY, AWMAN_SQUAD_ROOT, SHELL,
};
use awman::data::fs::daemon_process::ServerMeta;
use awman::data::fs::{ApiPaths, SquadPaths};
use awman::data::message::{UserMessage, UserMessageSink};
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use tokio::sync::Mutex as AsyncMutex;

/// Serialises the tests that mutate the real process environment.
static PROCESS_ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

/// Captures every user-facing message and returns immediately from the serve
/// call, so `run_start` runs end to end without binding a port.
///
/// The key disclosure is recorded *separately* from the message stream: since
/// WI 0114 F-56 Layer 2 hands a frontend the key, the shell and the export
/// line and each frontend draws its own banner, so a test that looked for the
/// banner's prose in `write_message` output would pass vacuously against the
/// trait's do-nothing default.
#[derive(Clone, Default)]
struct RecordingFrontend {
    messages: Arc<Mutex<Vec<String>>>,
    disclosures: Arc<Mutex<Vec<SquadKeySetup>>>,
}

impl RecordingFrontend {
    fn text(&self) -> String {
        self.messages.lock().unwrap().join("\n")
    }

    /// Every key this run disclosed, in order. More than one would mean the
    /// same secret was shown twice.
    fn disclosures(&self) -> Vec<SquadKeySetup> {
        self.disclosures.lock().unwrap().clone()
    }
}

impl UserMessageSink for RecordingFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.messages.lock().unwrap().push(msg.text);
    }
    fn replay_queued(&mut self) {}
}

#[async_trait::async_trait]
impl SquadCommandFrontend for RecordingFrontend {
    // WI 0113 F-02: the daemon host supplies the unattended frontends its
    // evaluator drives agents with; Layer 2 builds the evaluator from them.
    fn squad_run_frontends(
        &self,
    ) -> Option<Arc<dyn awman::command::commands::squad::evaluation::SquadRunFrontends>> {
        Some(awman::frontend::squad::unattended::UnattendedFrontends::shared())
    }

    fn show_key_setup(&mut self, setup: &SquadKeySetup) {
        self.disclosures.lock().unwrap().push(setup.clone());
    }

    async fn serve_squad_daemon(
        &mut self,
        _handles: awman::command::commands::squad::daemon_runtime::SquadDaemonHandles,
    ) -> Result<(), CommandError> {
        // Stand in for a served-then-shut-down daemon: `run_start` continues
        // through its cleanup path exactly as it would after Ctrl-C.
        Ok(())
    }
}

fn engines_at(root: &Path) -> Engines {
    let api_paths = ApiPaths::from_root(root);
    let auth_paths = awman::data::fs::AuthPathResolver::at_home(root);
    let overlay_engine = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let runtime = Arc::new(ContainerRuntime::docker());
    Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay_engine.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths.clone())),
        agent_engine: Arc::new(AgentEngine::new(overlay_engine, runtime)),
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(api_paths.root())),
        credential_monitor: None,
        global_config: std::sync::Arc::new(Default::default()),
    }
}

/// Scope the process environment to an isolated fixture, then restore it.
/// `SquadDaemon` reads `Env::from_process()`.
struct ScopedEnv(Vec<(&'static str, Option<String>)>);

impl ScopedEnv {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        let saved = vars
            .iter()
            .map(|(key, value)| {
                let previous = std::env::var(key).ok();
                std::env::set_var(key, value);
                (*key, previous)
            })
            .collect();
        Self(saved)
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, previous) in &self.0 {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

async fn run_start(home: &Path, flags: SquadStartFlags) -> (RecordingFrontend, SquadPaths) {
    let squad_root = home.join("squad");
    std::fs::create_dir_all(&squad_root).unwrap();
    let _scoped = ScopedEnv::set(&[
        (AWMAN_API_ROOT, home.join("api").to_str().unwrap()),
        (AWMAN_SQUAD_ROOT, squad_root.to_str().unwrap()),
        (AWMAN_CONFIG_HOME, home.to_str().unwrap()),
        (SHELL, "/bin/zsh"),
    ]);
    let frontend = RecordingFrontend::default();
    SquadDaemon::new(SquadDaemonSubcommand::Start(flags), engines_at(home))
        .run(&mut frontend.clone())
        .await
        .expect("squad start must succeed on a clean fixture");
    (frontend, SquadPaths::from_root(&squad_root))
}

/// The missing-key answer's wording, pinned character for character.
///
/// It moved from a free function in the CLI to `CommandError::SquadKeyMissing`
/// (WI 0113 F-04) so the TUI and any later frontend say the same thing. Every
/// clause is load-bearing: the variable to export, the fact that the key
/// cannot be read back, the command that mints a new one, and the warning that
/// minting invalidates the old one.
#[test]
fn the_missing_key_answer_names_the_variable_the_command_and_the_consequence() {
    let expected = format!(
        "squad requires a bearer key and none is set in this shell.\n\n\
         The key is shown only once, when it is minted, and only its hash is \
         stored — so it cannot be read back. Set {AWMAN_SQUAD_KEY} if you saved it, or mint a \
         new one with:\n    awman squad start --refresh-key\n\
         which invalidates the previous key, so any shell still exporting it \
         must be updated too.",
    );
    assert_eq!(CommandError::SquadKeyMissing.to_string(), expected);
}

/// The first start mints a key AND tells the user how to spend it. Printing the
/// key alone — the previous behaviour — left every later process unable to
/// authenticate, because nothing documented `AWMAN_SQUAD_KEY`.
#[tokio::test]
async fn first_start_emits_a_shell_export_snippet_for_the_minted_key() {
    let _serialised = PROCESS_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let (frontend, paths) = run_start(
        home.path(),
        SquadStartFlags {
            port: 0,
            background: false,
            refresh_key: false,
            dangerously_skip_auth: false,
        },
    )
    .await;

    let disclosures = frontend.disclosures();
    assert_eq!(
        disclosures.len(),
        1,
        "a first start discloses the minted key exactly once"
    );
    let disclosed = &disclosures[0];
    assert_eq!(
        disclosed.export_line,
        format!("export {AWMAN_SQUAD_KEY}={}", disclosed.key),
        "start must hand over a copy-pasteable export line"
    );
    assert_eq!(
        disclosed.rc_file(),
        "~/.zshrc",
        "the fixture's SHELL is /bin/zsh, so the startup file is ~/.zshrc"
    );
    assert!(
        paths.daemon().key_hash_file().exists(),
        "the key hash must be persisted so the daemon can verify it"
    );

    // The disclosure carries the real key, not a placeholder: the hash on disk
    // is never the plaintext, so a wrong key here would be undetectable later.
    assert!(
        disclosed.key.len() >= 32,
        "the disclosed value must be the key itself; got {:?}",
        disclosed.key
    );
    assert!(
        !std::fs::read_to_string(paths.daemon().key_hash_file())
            .unwrap()
            .contains(&disclosed.key),
        "the plaintext key must never be what lands in the hash file"
    );
}

/// `--dangerously-skip-auth` is the documented alternative to holding a key.
/// It must mint nothing — a hash whose plaintext nobody holds would lock the
/// user out of the next auth-enabled start.
#[tokio::test]
async fn skip_auth_mints_no_key_and_warns_that_auth_is_disabled() {
    let _serialised = PROCESS_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let (frontend, paths) = run_start(
        home.path(),
        SquadStartFlags {
            port: 0,
            background: false,
            refresh_key: false,
            dangerously_skip_auth: true,
        },
    )
    .await;

    assert!(
        !paths.daemon().key_hash_file().exists(),
        "--dangerously-skip-auth must write no key hash"
    );
    assert!(
        frontend.disclosures().is_empty(),
        "no key may be disclosed when none was minted"
    );
    let text = frontend.text();
    assert!(
        !text.contains("AWMAN_SQUAD_KEY"),
        "nor may one be named in the message stream; got:\n{text}"
    );
    assert!(
        text.contains("DISABLED") && text.contains("--dangerously-skip-auth"),
        "the user must be told authentication is off; got:\n{text}"
    );
    assert!(
        text.contains("127.0.0.1"),
        "the warning must say why this is tolerable — loopback only; got:\n{text}"
    );
}

/// `--refresh-key` replaces the key and re-emits the same snippet, so a user
/// who lost the original has a documented way back in.
#[tokio::test]
async fn refresh_key_re_emits_the_export_snippet() {
    let _serialised = PROCESS_ENV_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let (frontend, _paths) = run_start(
        home.path(),
        SquadStartFlags {
            port: 0,
            background: false,
            refresh_key: true,
            dangerously_skip_auth: false,
        },
    )
    .await;
    let disclosures = frontend.disclosures();
    assert_eq!(
        disclosures.len(),
        1,
        "--refresh-key must disclose the replacement key too"
    );
    assert!(
        disclosures[0]
            .export_line
            .starts_with(&format!("export {AWMAN_SQUAD_KEY}=")),
        "got: {:?}",
        disclosures[0].export_line
    );
}

/// The env var the snippet exports is the one the client actually reads.
/// These two drifting apart is precisely the gap this work closes.
#[test]
fn the_exported_variable_is_the_one_the_client_reads() {
    let snippet = export_snippet("secret-key", ShellFlavor::Zsh);
    let env = EnvSnapshot::with_overrides([(AWMAN_SQUAD_KEY, "secret-key")]);
    assert_eq!(env.squad_key(), Some("secret-key"));
    assert_eq!(
        snippet,
        format!("export {AWMAN_SQUAD_KEY}=secret-key"),
        "the snippet must export the variable `EnvSnapshot::squad_key` reads"
    );
}

/// An empty `AWMAN_SQUAD_KEY` is not a key. Treating `""` as one would send an
/// empty bearer token and produce a confusing 401 rather than the usual
/// "no key supplied" path.
#[test]
fn an_empty_squad_key_is_treated_as_unset() {
    let env = EnvSnapshot::with_overrides([(AWMAN_SQUAD_KEY, "")]);
    assert_eq!(env.squad_key(), None);
}

/// `Env::from_process` must actually capture the variable — the snapshot only
/// reads a fixed list of names, so an omission here silently disables the key.
#[tokio::test]
async fn squad_key_is_captured_from_the_real_process_environment() {
    let _serialised = PROCESS_ENV_LOCK.lock().await;
    let _scoped = ScopedEnv::set(&[(AWMAN_SQUAD_KEY, "from-the-environment")]);
    assert_eq!(
        Env::from_process().squad_key(),
        Some("from-the-environment")
    );
}

/// Clients read `auth_disabled` off the sidecar to decide whether to provision
/// a key at all. A sidecar written before the field existed must read as
/// "auth required" — the safe direction.
#[test]
fn a_legacy_server_sidecar_reads_as_auth_required() {
    let legacy = r#"{"port":9876,"bind_ip":"127.0.0.1","scheme":"http"}"#;
    let meta: ServerMeta = serde_json::from_str(legacy).expect("legacy sidecar must still parse");
    assert!(
        !meta.auth_disabled,
        "an absent flag must not be read as disabled auth"
    );
}

#[test]
fn a_skip_auth_daemon_publishes_that_fact_in_its_sidecar() {
    let meta = ServerMeta {
        port: 9876,
        bind_ip: "127.0.0.1".into(),
        scheme: "http".into(),
        auth_disabled: true,
    };
    let json = serde_json::to_string(&meta).unwrap();
    assert_eq!(serde_json::from_str::<ServerMeta>(&json).unwrap(), meta);
}
