//! Work-server half of `awman link`: device approval or a server-issued static token.
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
