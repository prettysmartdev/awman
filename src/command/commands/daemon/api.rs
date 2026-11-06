//! `awman daemon api`: actions and `--raw` requests over the daemon's
//! owner-authenticated local listener.
use super::actions::{self, Request};
use super::client::DaemonClient;
use crate::command::error::CommandError;
use crate::data::session::Session;
use serde_json::Value;

/// Parse `--data`: inline JSON, `@file`, or `-` for stdin. Read before any
/// request is sent, so a bad body never reaches the daemon.
pub fn data(input: &str) -> Result<Value, CommandError> {
    let text = if input == "-" {
        use std::io::Read;
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| CommandError::Other(format!("could not read --data from stdin: {e}")))?;
        text
    } else if let Some(path) = input.strip_prefix('@') {
        std::fs::read_to_string(path)
            .map_err(|e| CommandError::Other(format!("could not read --data file {path}: {e}")))?
    } else {
        input.to_string()
    };
    serde_json::from_str(&text)
        .map_err(|e| CommandError::Other(format!("invalid --data JSON: {e}")))
}

/// Run an action, expanding the two compositions (`jobs.cancel feature=`,
/// `grants.delete all`) into the requests they stand for.
pub async fn run_action(
    client: &DaemonClient,
    name: &str,
    kv: &[(String, String)],
) -> Result<Value, CommandError> {
    if name == "grants.delete" && kv.iter().any(|(k, _)| k == "all") {
        if kv.len() != 1 {
            return Err(CommandError::Other(
                "grants.delete all takes no other arguments".into(),
            ));
        }
        let mut deleted = Vec::new();
        for grant in client.all(&actions::plan("grants.list", &[])?).await? {
            if let Some(id) = grant["id"].as_str() {
                client
                    .call(&actions::plan(
                        "grants.delete",
                        &[("id".into(), id.into())],
                    )?)
                    .await?;
                deleted.push(id.to_string());
            }
        }
        return Ok(serde_json::json!({ "deleted": deleted }));
    }
    if name == "jobs.cancel" {
        if let Some((_, feature)) = kv.iter().find(|(k, _)| k == "feature") {
            if kv.len() != 1 {
                return Err(CommandError::Other(
                    "jobs.cancel takes either feature= or id=, not both".into(),
                ));
            }
            let ids = running_jobs(client, feature).await?;
            for id in &ids {
                client
                    .call(&actions::plan("jobs.cancel", &[("id".into(), id.clone())])?)
                    .await?;
            }
            return Ok(serde_json::json!({ "canceled": ids }));
        }
    }
    client.call(&actions::plan(name, kv)?).await
}

/// The IDs of every running job of `feature` (`worker` commands, `squad` runs).
pub async fn running_jobs(
    client: &DaemonClient,
    feature: &str,
) -> Result<Vec<String>, CommandError> {
    let ids = |items: Vec<Value>| -> Vec<String> {
        items
            .iter()
            .filter_map(|item| item["id"].as_str().map(str::to_string))
            .collect()
    };
    match feature {
        "worker" => Ok(ids(client
            .all(&Request::get("/v1/worker/commands").with_query("status", "running"))
            .await?)),
        "squad" => {
            let mut out = Vec::new();
            for task in ids(client.all(&Request::get("/v1/squad/tasks")).await?) {
                out.extend(ids(client
                    .all(
                        &Request::get(format!("/v1/squad/tasks/{task}/runs"))
                            .with_query("status", "running"),
                    )
                    .await?));
            }
            Ok(out)
        }
        other => Err(CommandError::Other(format!(
            "jobs.cancel: invalid feature={other:?} (expected worker or squad)"
        ))),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DaemonApiInput {
    List,
    Action {
        name: String,
        args: Vec<(String, String)>,
    },
    Raw(Request),
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
pub enum DaemonApiOutcome {
    List(String),
    Response(Value),
}

pub trait DaemonApiCommandFrontend: crate::data::message::UserMessageSink + Send + Sync {}
impl<T: crate::data::message::UserMessageSink + Send + Sync> DaemonApiCommandFrontend for T {}

pub struct DaemonApiCommand {
    session: Session,
    input: DaemonApiInput,
}

/// Turn the catalogue's `action args… [--list|--raw M|--data D]` into input.
/// Everything that can be refused locally is refused here.
pub fn parse_input(
    action: Option<String>,
    args: Vec<String>,
    list: bool,
    raw_method: Option<String>,
    data_flag: Option<String>,
) -> Result<DaemonApiInput, CommandError> {
    let invalid = |reason: &str| CommandError::Other(format!("awman daemon api: {reason}"));
    if list {
        if action.is_some() || !args.is_empty() || raw_method.is_some() || data_flag.is_some() {
            return Err(invalid(
                "--list takes no action, arguments, --raw or --data",
            ));
        }
        return Ok(DaemonApiInput::List);
    }
    if let Some(method) = raw_method {
        let path = action.ok_or_else(|| invalid("--raw needs METHOD and a /v1/… path"))?;
        if !args.is_empty() {
            return Err(invalid("--raw takes only METHOD and a path"));
        }
        let body = data_flag.map(|d| data(&d)).transpose()?;
        return Ok(DaemonApiInput::Raw(actions::raw(&method, &path, body)?));
    }
    if data_flag.is_some() {
        return Err(invalid("--data needs --raw"));
    }
    let name = action.ok_or_else(|| invalid("name an action, or pass --list or --raw"))?;
    let args = args
        .iter()
        .map(|arg| {
            if arg == "all" {
                return Ok(("all".to_string(), "true".to_string()));
            }
            arg.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| invalid(&format!("action arguments are key=value; got {arg:?}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DaemonApiInput::Action { name, args })
}

/// Refuse an unknown action, an unknown key or a mistyped value locally,
/// before connecting to the daemon. Compositions check their own selector.
pub fn validate(input: &DaemonApiInput) -> Result<(), CommandError> {
    let DaemonApiInput::Action { name, args } = input else {
        return Ok(());
    };
    let composition = (name == "grants.delete" && args.iter().any(|(k, _)| k == "all"))
        || (name == "jobs.cancel" && args.iter().any(|(k, _)| k == "feature"));
    if !composition {
        return actions::plan(name, args).map(|_| ());
    }
    match args.iter().find(|(k, _)| k == "feature") {
        Some((_, feature)) if !matches!(feature.as_str(), "worker" | "squad") => {
            Err(CommandError::Other(format!(
                "jobs.cancel: invalid feature={feature:?} (expected worker or squad)"
            )))
        }
        _ => Ok(()),
    }
}

impl DaemonApiCommand {
    pub fn from_input(ctx: &crate::command::dispatch::BuildContext) -> Result<Self, CommandError> {
        let input = parse_input(
            ctx.args.get("action").map(str::to_string),
            ctx.args.trailing("args").to_vec(),
            ctx.flags.bool("list"),
            ctx.flags.string("raw"),
            ctx.flags.string("data"),
        )?;
        Ok(Self::new(ctx.session.clone(), input))
    }

    pub fn new(session: Session, input: DaemonApiInput) -> Self {
        Self { session, input }
    }
}

#[async_trait::async_trait]
impl crate::command::commands::Command for DaemonApiCommand {
    type Frontend = Box<dyn DaemonApiCommandFrontend>;
    type Outcome = DaemonApiOutcome;
    async fn run_with_frontend(
        self,
        _frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let request = match self.input {
            DaemonApiInput::List => return Ok(DaemonApiOutcome::List(actions::list())),
            other => other,
        };
        validate(&request)?;
        let client = DaemonClient::connect(self.session.env())?;
        let value = match request {
            DaemonApiInput::List => unreachable!("handled above"),
            DaemonApiInput::Raw(request) => client.call(&request).await?,
            DaemonApiInput::Action { name, args } => run_action(&client, &name, &args).await?,
        };
        Ok(DaemonApiOutcome::Response(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn list_and_raw_and_actions_parse() {
        assert_eq!(
            parse_input(None, vec![], true, None, None).unwrap(),
            DaemonApiInput::List
        );
        assert_eq!(
            parse_input(
                Some("/v1/status".into()),
                vec![],
                false,
                Some("get".into()),
                None
            )
            .unwrap(),
            DaemonApiInput::Raw(actions::raw("GET", "/v1/status", None).unwrap())
        );
        assert_eq!(
            parse_input(
                Some("keys.create".into()),
                s(&["name=ci"]),
                false,
                None,
                None
            )
            .unwrap(),
            DaemonApiInput::Action {
                name: "keys.create".into(),
                args: vec![("name".into(), "ci".into())]
            }
        );
    }

    #[test]
    fn bad_input_is_refused_before_any_request() {
        for (action, args, list, raw, data) in [
            (Some("x"), vec![], true, None, None),
            (None, vec![], false, Some("GET"), None),
            (Some("/v1/status"), s(&["a=b"]), false, Some("GET"), None),
            (Some("features.list"), vec![], false, None, Some("{}")),
            (Some("features.get"), s(&["feature"]), false, None, None),
            (
                Some("features.get"),
                s(&["feature=nope"]),
                false,
                None,
                None,
            ),
            (Some("nope.list"), vec![], false, None, None),
            (
                Some("jobs.cancel"),
                s(&["feature=engine"]),
                false,
                None,
                None,
            ),
            (None, vec![], false, None, None),
        ] {
            assert!(
                parse_input(
                    action.map(str::to_string),
                    args.clone(),
                    list,
                    raw.map(str::to_string),
                    data.map(str::to_string)
                )
                .and_then(|input| validate(&input))
                .is_err(),
                "{action:?} {args:?}"
            );
        }
    }

    #[test]
    fn compositions_parse() {
        assert!(parse_input(
            Some("jobs.cancel".into()),
            s(&["feature=worker"]),
            false,
            None,
            None
        )
        .is_ok());
        assert!(parse_input(Some("grants.delete".into()), s(&["all"]), false, None, None).is_ok());
    }

    #[test]
    fn data_reads_inline_json_and_files() {
        assert_eq!(data(r#"{"a":1}"#).unwrap(), serde_json::json!({"a": 1}));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("body.json");
        std::fs::write(&file, r#"{"name":"ci"}"#).unwrap();
        assert_eq!(
            data(&format!("@{}", file.display())).unwrap(),
            serde_json::json!({"name": "ci"})
        );
        assert!(data("{not json").is_err());
        assert!(data("@/no/such/file").is_err());
    }

    #[tokio::test]
    async fn invalid_actions_are_refused_before_connecting() {
        let home = tempfile::tempdir().unwrap();
        let session = Session::for_tests_with_env(
            home.path(),
            crate::data::config::env::EnvSnapshot::with_overrides([(
                "AWMAN_HOME",
                home.path().to_str().unwrap(),
            )]),
        );
        let command = DaemonApiCommand::new(
            session,
            DaemonApiInput::Action {
                name: "keys.create".into(),
                args: vec![("name".into(), "Not Valid".into())],
            },
        );
        let error = crate::command::commands::Command::run_with_frontend(
            command,
            Box::new(crate::data::message::RecordingMessageSink::new()),
        )
        .await
        .unwrap_err();
        assert!(
            !matches!(error, CommandError::DaemonNotRunning),
            "validation must precede the connection: {error:?}"
        );
        assert!(error.to_string().contains("invalid name"), "{error}");
    }

    #[tokio::test]
    async fn no_daemon_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let session = Session::for_tests_with_env(
            home.path(),
            crate::data::config::env::EnvSnapshot::with_overrides([(
                "AWMAN_HOME",
                home.path().to_str().unwrap(),
            )]),
        );
        let command = DaemonApiCommand::new(
            session,
            DaemonApiInput::Raw(actions::raw("GET", "/v1/status", None).unwrap()),
        );
        let error = crate::command::commands::Command::run_with_frontend(
            command,
            Box::new(crate::data::message::RecordingMessageSink::new()),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, CommandError::DaemonNotRunning), "{error:?}");
    }
}
