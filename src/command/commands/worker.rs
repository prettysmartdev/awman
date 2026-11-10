//! `awman worker link`: device approval or a server-issued static token.
//!
//! Secret hygiene: the token lives only in [`Secret`] (redacted `Debug`, never
//! formatted), is written solely to the owner-only link store, and is never
//! put in config, messages or errors. `--token -` reads it from stdin so it
//! stays out of the process list.
use async_trait::async_trait;
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::command::commands::Command;
use crate::command::dispatch::{BuildContext, CommandFrontend};
use crate::command::error::CommandError;
use crate::data::config::fields::fold_map_name;
use crate::data::config::work_servers::{normalize_work_server_name, validate_configured_label};
use crate::data::config::{file::global_config_path, ConfigFile, FileKind, WorkServerConfig};
use crate::data::fs::DataPaths;
use crate::data::message::{MessageLevel, UserMessage};
use crate::data::session::Session;
use crate::data::worker::link_store::LinkStore;

/// How long a person has to approve the link.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);
/// Bounds for the server-suggested poll interval.
const MIN_POLL: Duration = Duration::from_millis(100);
const MAX_POLL: Duration = Duration::from_secs(30);
/// Transient transport failures tolerated while polling.
const MAX_POLL_FAILURES: u32 = 5;

/// A token that cannot be printed by accident.
#[derive(Clone)]
struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

pub struct WorkerLinkCommand {
    session: Session,
    url: String,
    name: Option<String>,
    labels: BTreeMap<String, String>,
    token: Option<Secret>,
    cancel: tokio_util::sync::CancellationToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkerLinkOutcome {
    pub name: String,
    pub url: String,
    pub live: String,
}

impl WorkerLinkCommand {
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let labels = parse_labels(ctx.flags.strs("label"))?;
        Ok(Self {
            session: ctx.session.clone(),
            url: ctx.args.require("server-url")?,
            name: ctx.flags.string("name"),
            labels,
            token: ctx.flags.string("token").map(Secret),
            cancel: ctx.cancel.clone(),
        })
    }

    /// The config name: `--name` (validated) or the URL host, normalised by
    /// the shared config helper.
    fn name(&self) -> Result<String, CommandError> {
        let name = match &self.name {
            Some(name) => name.clone(),
            None => normalize_work_server_name(&self.url).map_err(CommandError::Other)?,
        };
        if !crate::data::config::fields::is_valid_map_name(&name) {
            return Err(CommandError::Other(format!(
                "invalid work server name `{name}`; use lowercase letters, digits, `_` and `-` (at most 63, starting with a letter or digit)"
            )));
        }
        Ok(name)
    }

    async fn obtain_token(
        &self,
        client: &awman_wsp::Client,
        frontend: &mut dyn CommandFrontend,
    ) -> Result<awman_wsp::LinkToken, CommandError> {
        if let Some(Secret(token)) = &self.token {
            let token = if token == "-" {
                frontend.read_stdin_secret()?
            } else {
                token.clone()
            };
            let token = token.trim().to_string();
            if token.is_empty() {
                return Err(CommandError::Other("work server token is empty".into()));
            }
            return Ok(awman_wsp::LinkToken {
                refresh_token: token,
            });
        }
        let link = client.start_link().await.map_err(wsp_error)?;
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "Approve work server link at {} with code {}",
                printable(&link.verification_uri),
                printable(&link.user_code)
            ),
        });
        if let Some(url) = browser_target(&link.verification_uri) {
            open_browser(&url);
        }
        let interval = Duration::from_millis(link.interval_ms).clamp(MIN_POLL, MAX_POLL);
        let deadline = tokio::time::Instant::now() + APPROVAL_TIMEOUT;
        let mut failures = 0;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(CommandError::Other(
                    "work server link approval timed out".into(),
                ));
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return Err(CommandError::Other("work server link cancelled".into())),
                _ = tokio::time::sleep(interval) => {}
            }
            match client.poll_link(&link.link_id).await {
                Ok(Some(token)) if token.refresh_token.is_empty() => {
                    return Err(CommandError::Other(
                        "work server returned an empty token".into(),
                    ));
                }
                Ok(Some(token)) => return Ok(token),
                Ok(None) => failures = 0,
                Err(awman_wsp::Error::Transport(_)) if failures < MAX_POLL_FAILURES => {
                    failures += 1;
                }
                Err(error) => return Err(wsp_error(error)),
            }
        }
    }
}

fn parse_labels(pairs: &[String]) -> Result<BTreeMap<String, String>, CommandError> {
    let mut labels = BTreeMap::new();
    for pair in pairs {
        let (key, value) = pair.split_once('=').ok_or_else(|| {
            CommandError::Other(format!("invalid --label `{pair}`; expected k=v"))
        })?;
        if value.is_empty() || validate_configured_label(key).is_err() {
            return Err(CommandError::Other(format!(
                "invalid --label `{pair}`; awman/ is reserved and values must be nonempty"
            )));
        }
        if labels.insert(key.to_string(), value.to_string()).is_some() {
            return Err(CommandError::Other(format!(
                "duplicate --label key `{key}`"
            )));
        }
    }
    Ok(labels)
}

fn wsp_error(error: awman_wsp::Error) -> CommandError {
    CommandError::Other(error.to_string())
}

/// Server-supplied text is shown to a person: drop control characters (no
/// terminal escape sequences) and bound the length.
fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// Only plain web URLs are handed to the system opener: never a `file:` or
/// custom scheme, and never something an opener could read as a flag.
fn browser_target(uri: &str) -> Option<String> {
    let url = reqwest::Url::parse(uri).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

fn open_browser(url: &str) {
    #[cfg(test)]
    {
        let _ = url;
    }
    #[cfg(not(test))]
    {
        let program = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(target_os = "windows") {
            "explorer"
        } else {
            "xdg-open"
        };
        let _ = std::process::Command::new(program)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Refuse a name already linked, including `-`/`_` spellings of it.
fn ensure_unlinked(config: &ConfigFile, name: &str) -> Result<(), CommandError> {
    let folded = fold_map_name(name);
    if config
        .work_servers
        .iter()
        .flatten()
        .any(|(existing, _)| fold_map_name(existing) == folded)
    {
        return Err(CommandError::Other(format!(
            "work server `{name}` is already linked"
        )));
    }
    Ok(())
}

#[async_trait]
impl Command for WorkerLinkCommand {
    type Frontend = Box<dyn CommandFrontend>;
    type Outcome = WorkerLinkOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let name = self.name()?;
        let client = awman_wsp::Client::new(&self.url, String::new()).map_err(wsp_error)?;
        let path = global_config_path(self.session.env())?;
        ensure_unlinked(&ConfigFile::load(&path, FileKind::Global)?, &name)?;
        let paths = DataPaths::from_env(self.session.env())?;
        let store = LinkStore::new(&paths);
        if store.read(&name)?.is_some() {
            return Err(CommandError::Other(format!(
                "work server `{name}` has a stored token but no config entry (an unlink may still be finishing); wait for it, or delete {}/work_servers/{name}.json",
                paths.root().display()
            )));
        }
        let token = self.obtain_token(&client, frontend.as_mut()).await?;
        // Approval can take minutes: re-read the config so a concurrent edit
        // is neither lost nor duplicated.
        let mut config = ConfigFile::load(&path, FileKind::Global)?;
        ensure_unlinked(&config, &name)?;
        config
            .work_servers
            .get_or_insert_with(BTreeMap::new)
            .insert(
                name.clone(),
                WorkServerConfig {
                    url: Some(self.url.clone()),
                    enabled: Some(true),
                    claim: None,
                    max_concurrent_jobs: None,
                    interaction: None,
                    labels: (!self.labels.is_empty()).then_some(self.labels),
                },
            );
        store.write(&name, &token)?;
        if let Err(error) = config.save(&path, FileKind::Global) {
            let _ = store.remove(&name);
            return Err(error.into());
        }
        use crate::command::commands::config::{DaemonReloader, LocalDaemons};
        let reloader = LocalDaemons;
        let live = match reloader.running(&self.session) {
            Some(daemon) => match reloader.reload(&daemon) {
                Ok(_) => "applied to running daemon".to_string(),
                Err(error) => format!("saved; daemon reload failed: {error}"),
            },
            None => "applies on next daemon start".to_string(),
        };
        Ok(WorkerLinkOutcome {
            name,
            url: self.url,
            live,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::EnvSnapshot;
    use std::sync::{Arc, Mutex};

    /// A frontend that records messages and can supply a stdin secret.
    #[derive(Default)]
    struct Recorder {
        stdin: Option<String>,
        messages: Arc<Mutex<Vec<String>>>,
    }
    impl crate::data::message::UserMessageSink for Recorder {
        fn write_message(&mut self, m: UserMessage) {
            self.messages.lock().unwrap().push(m.text);
        }
        fn replay_queued(&mut self) {}
    }
    impl CommandFrontend for Recorder {
        fn read_stdin_secret(&mut self) -> Result<String, CommandError> {
            self.stdin
                .clone()
                .ok_or_else(|| CommandError::Other("no stdin".into()))
        }
        fn kind(&self) -> crate::command::dispatch::catalogue::FrontendKind {
            crate::command::dispatch::catalogue::FrontendKind::Cli
        }
        fn flag_bool(&self, _: &[&str], _: &str) -> Result<Option<bool>, CommandError> {
            Ok(None)
        }
        fn flag_string(&self, _: &[&str], _: &str) -> Result<Option<String>, CommandError> {
            Ok(None)
        }
        fn flag_strings(&self, _: &[&str], _: &str) -> Result<Vec<String>, CommandError> {
            Ok(vec![])
        }
        fn flag_path(
            &self,
            _: &[&str],
            _: &str,
        ) -> Result<Option<std::path::PathBuf>, CommandError> {
            Ok(None)
        }
        fn flag_enum(&self, _: &[&str], _: &str) -> Result<Option<String>, CommandError> {
            Ok(None)
        }
        fn flag_u16(&self, _: &[&str], _: &str) -> Result<Option<u16>, CommandError> {
            Ok(None)
        }
        fn flag_usize(&self, _: &[&str], _: &str) -> Result<Option<usize>, CommandError> {
            Ok(None)
        }
        fn argument(&self, _: &[&str], _: &str) -> Result<Option<String>, CommandError> {
            Ok(None)
        }
        fn arguments(&self, _: &[&str], _: &str) -> Result<Vec<String>, CommandError> {
            Ok(vec![])
        }
    }

    struct Fx {
        home: tempfile::TempDir,
        env: EnvSnapshot,
    }
    impl Fx {
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            let env = EnvSnapshot::with_overrides([("AWMAN_HOME", home.path().to_str().unwrap())]);
            Self { home, env }
        }
        fn command(&self, url: &str, name: Option<&str>, token: Option<&str>) -> WorkerLinkCommand {
            WorkerLinkCommand {
                session: Session::for_tests_with_env(self.home.path(), self.env.clone()),
                url: url.into(),
                name: name.map(String::from),
                labels: BTreeMap::new(),
                token: token.map(|t| Secret(t.into())),
                cancel: Default::default(),
            }
        }
        fn config_text(&self) -> String {
            std::fs::read_to_string(self.home.path().join("config.json")).unwrap_or_default()
        }
        fn store(&self) -> LinkStore {
            LinkStore::new(&DataPaths::from_env(&self.env).unwrap())
        }
        fn token_files(&self) -> Vec<std::path::PathBuf> {
            let dir = DataPaths::from_env(&self.env)
                .unwrap()
                .root()
                .join("work_servers");
            std::fs::read_dir(dir)
                .map(|d| d.map(|e| e.unwrap().path()).collect())
                .unwrap_or_default()
        }
    }

    fn assert_private(path: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{path:?}");
        }
        let _ = path;
    }

    #[test]
    fn name_defaults_to_normalised_host_and_rejects_bad_names() {
        let fx = Fx::new();
        let c = fx.command("https://Work.Example.com:8443/x", None, Some("t"));
        assert_eq!(c.name().unwrap(), "work-example-com");
        for bad in ["_bad", "Has Caps", "a/b", "../x", ""] {
            assert!(
                fx.command("https://h.example", Some(bad), Some("t"))
                    .name()
                    .is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            fx.command("https://h.example", Some("in_fact-2"), Some("t"))
                .name()
                .unwrap(),
            "in_fact-2"
        );
    }

    #[test]
    fn awman_labels_and_malformed_labels_are_refused() {
        let ok = parse_labels(&["pool=laptops".into(), "example.com/zone=eu".into()]).unwrap();
        assert_eq!(ok["pool"], "laptops");
        for bad in [
            "awman/os=linux",
            "awman/agent.codex=true",
            "novalue",
            "k=",
            "BAD=x",
            "infact.kind=x",
        ] {
            assert!(parse_labels(&[bad.into()]).is_err(), "{bad}");
        }
        assert!(parse_labels(&["a=1".into(), "a=2".into()]).is_err());
    }

    #[tokio::test]
    async fn bad_name_and_awman_label_write_nothing() {
        let fx = Fx::new();
        let err = fx
            .command("https://work.example.com", Some("_bad"), Some("tok"))
            .run_with_frontend(Box::new(Recorder::default()))
            .await;
        assert!(err.is_err());
        assert!(fx.config_text().is_empty());
        assert!(fx.token_files().is_empty());
    }

    #[tokio::test]
    async fn static_token_link_writes_config_and_private_token() {
        let fx = Fx::new();
        let mut command = fx.command(
            "https://work.example.com/base",
            Some("team"),
            Some("refresh-secret"),
        );
        command.labels = BTreeMap::from([("pool".into(), "laptops".into())]);
        let rec = Recorder::default();
        let messages = rec.messages.clone();
        let outcome = command.run_with_frontend(Box::new(rec)).await.unwrap();
        assert_eq!(outcome.name, "team");
        let config =
            ConfigFile::load(&global_config_path(&fx.env).unwrap(), FileKind::Global).unwrap();
        let entry = &config.work_servers.unwrap()["team"];
        assert_eq!(entry.url.as_deref(), Some("https://work.example.com/base"));
        assert_eq!(entry.enabled, Some(true));
        assert_eq!(entry.labels.as_ref().unwrap()["pool"], "laptops");
        assert!(!fx.config_text().contains("refresh-secret"));
        assert_eq!(
            fx.store().read("team").unwrap().unwrap().refresh_token,
            "refresh-secret"
        );
        assert_private(&fx.token_files()[0]);
        assert!(messages
            .lock()
            .unwrap()
            .iter()
            .all(|m| !m.contains("refresh-secret")));
        assert!(!format!("{outcome:?}").contains("refresh-secret"));
    }

    #[tokio::test]
    async fn no_labels_writes_no_empty_labels_map() {
        let fx = Fx::new();
        fx.command("https://work.example.com", Some("team"), Some("t"))
            .run_with_frontend(Box::new(Recorder::default()))
            .await
            .unwrap();
        let doc: serde_json::Value = serde_json::from_str(&fx.config_text()).unwrap();
        assert!(doc["workServers"]["team"].get("labels").is_none(), "{doc}");
    }

    #[tokio::test]
    async fn token_dash_reads_stdin_and_never_echoes_it() {
        let fx = Fx::new();
        let rec = Recorder {
            stdin: Some("stdin-secret\n".into()),
            ..Default::default()
        };
        let messages = rec.messages.clone();
        let command = fx.command("https://work.example.com", Some("team"), Some("-"));
        assert!(!format!("{:?}", command.token).contains("-secret"));
        command.run_with_frontend(Box::new(rec)).await.unwrap();
        assert_eq!(
            fx.store().read("team").unwrap().unwrap().refresh_token,
            "stdin-secret"
        );
        assert!(!fx.config_text().contains("stdin-secret"));
        assert!(messages
            .lock()
            .unwrap()
            .iter()
            .all(|m| !m.contains("stdin-secret")));
    }

    #[tokio::test]
    async fn empty_stdin_token_is_refused_and_writes_nothing() {
        let fx = Fx::new();
        let rec = Recorder {
            stdin: Some("\n".into()),
            ..Default::default()
        };
        let command = fx.command("https://work.example.com", Some("team"), Some("-"));
        assert!(command.run_with_frontend(Box::new(rec)).await.is_err());
        assert!(fx.token_files().is_empty());
        assert!(fx.config_text().is_empty());
    }

    #[tokio::test]
    async fn folded_duplicate_names_are_refused() {
        let fx = Fx::new();
        fx.command("https://a.example.com", Some("in-fact"), Some("one"))
            .run_with_frontend(Box::new(Recorder::default()))
            .await
            .unwrap();
        for name in ["in-fact", "in_fact"] {
            let err = fx
                .command("https://b.example.com", Some(name), Some("two"))
                .run_with_frontend(Box::new(Recorder::default()))
                .await
                .err()
                .unwrap();
            assert!(err.to_string().contains("already linked"), "{err}");
        }
        assert_eq!(fx.token_files().len(), 1);
        assert_eq!(
            fx.store().read("in-fact").unwrap().unwrap().refresh_token,
            "one"
        );
    }

    #[tokio::test]
    async fn device_flow_polls_until_approved_and_stores_the_refresh_token() {
        let fx = Fx::new();
        let mock = Arc::new(awman_wsp::mock::MockWorkServer::new());
        let serving = mock.clone().serve().await.unwrap();
        let approver = {
            let mock = mock.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(350)).await;
                mock.approve_link("link_1", BTreeMap::new());
            })
        };
        let rec = Recorder::default();
        let messages = rec.messages.clone();
        let outcome = fx
            .command(&serving.base_url, Some("mocked"), None)
            .run_with_frontend(Box::new(rec))
            .await
            .unwrap();
        approver.await.unwrap();
        assert_eq!(outcome.url, serving.base_url);
        let stored = fx.store().read("mocked").unwrap().unwrap().refresh_token;
        assert!(!stored.is_empty());
        assert_private(&fx.token_files()[0]);
        let config = fx.config_text();
        assert!(config.contains(&serving.base_url), "{config}");
        assert!(!config.contains(&stored));
        let shown = messages.lock().unwrap().join("\n");
        assert!(
            shown.contains("link_1") && shown.contains("example.invalid/approve"),
            "{shown}"
        );
        assert!(!shown.contains(&stored));
    }

    #[tokio::test]
    async fn denied_device_flow_writes_nothing() {
        let fx = Fx::new();
        let mock = Arc::new(awman_wsp::mock::MockWorkServer::new());
        let serving = mock.clone().serve().await.unwrap();
        let denier = {
            let mock = mock.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                mock.deny_link("link_1");
            })
        };
        let err = fx
            .command(&serving.base_url, Some("mocked"), None)
            .run_with_frontend(Box::new(Recorder::default()))
            .await;
        denier.await.unwrap();
        assert!(err.is_err());
        assert!(fx.token_files().is_empty());
        assert!(fx.config_text().is_empty());
    }

    #[tokio::test]
    async fn cancelling_the_device_flow_writes_nothing() {
        let fx = Fx::new();
        let mock = Arc::new(awman_wsp::mock::MockWorkServer::new());
        let serving = mock.clone().serve().await.unwrap();
        let command = fx.command(&serving.base_url, Some("mocked"), None);
        let cancel = command.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel.cancel();
        });
        assert!(command
            .run_with_frontend(Box::new(Recorder::default()))
            .await
            .is_err());
        assert!(fx.token_files().is_empty());
    }

    #[test]
    fn server_text_is_sanitised_and_only_web_urls_open_a_browser() {
        assert_eq!(printable("AB\x1b[31mC\n"), "AB[31mC");
        assert_eq!(
            browser_target("https://srv.example/approve?c=1").as_deref(),
            Some("https://srv.example/approve?c=1")
        );
        for bad in [
            "file:///etc/passwd",
            "--help",
            "javascript:alert(1)",
            "ssh://h",
        ] {
            assert!(browser_target(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn secret_debug_is_redacted() {
        assert_eq!(
            format!("{:?}", Secret("hunter2".into())),
            "Secret(<redacted>)"
        );
    }
}
