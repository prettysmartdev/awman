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
            if feature == "worker" {
                // WI 0143: a pulled job ends with its lease (the runner releases
                // it with the partial result); a lease with no cancelled job is
                // given back here.
                let released = release_held_leases(client, &ids).await?;
                return Ok(serde_json::json!({ "canceled": ids, "released": released }));
            }
            return Ok(serde_json::json!({ "canceled": ids }));
        }
    }
    client.call(&actions::plan(name, kv)?).await
}

/// Release every active lease whose job is not in `cancelled`, with
/// `reason=error`. A lease that ended meanwhile (`409 lease_ended`) is
/// skipped; a daemon without the lease routes has none to release.
pub async fn release_held_leases(
    client: &DaemonClient,
    cancelled: &[String],
) -> Result<Vec<String>, CommandError> {
    release_held_leases_with_reason(client, cancelled, "error").await
}
pub async fn release_held_leases_for_shutdown(
    client: &DaemonClient,
) -> Result<Vec<String>, CommandError> {
    let mut released = Vec::new();
    loop {
        let batch = release_held_leases_with_reason(client, &[], "shutdown").await?;
        if batch.is_empty() {
            return Ok(released);
        }
        released.extend(batch);
    }
}
async fn release_held_leases_with_reason(
    client: &DaemonClient,
    cancelled: &[String],
    reason: &str,
) -> Result<Vec<String>, CommandError> {
    let leases = match client
        .all(&actions::plan(
            "leases.list",
            &[("state".into(), "active".into())],
        )?)
        .await
    {
        Ok(leases) => leases,
        Err(CommandError::DaemonHttpStatus { status: 404, .. }) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut released = Vec::new();
    for lease in leases {
        let Some(id) = lease["id"].as_str() else {
            continue;
        };
        if lease["job_id"]
            .as_str()
            .is_some_and(|job| cancelled.iter().any(|c| c == job))
        {
            continue;
        }
        let request = actions::plan(
            "leases.release",
            &[("id".into(), id.into()), ("reason".into(), reason.into())],
        )?;
        match client.call(&request).await {
            Ok(_) => released.push(id.to_string()),
            Err(CommandError::DaemonHttpStatus {
                status: 409,
                ref body,
            }) if body.contains("lease_ended") => {}
            Err(e) => return Err(e),
        }
    }
    Ok(released)
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
            DaemonApiInput::Action { name, args }
                if matches!(
                    name.as_str(),
                    "launch_contexts.create" | "launch_contexts.refresh"
                ) =>
            {
                let mut request = actions::plan(&name, &args)?;
                let body = request.body.get_or_insert_with(|| serde_json::json!({}));
                body["root"] = serde_json::to_value(self.session.env().cwd()).unwrap();
                body["env"] = serde_json::to_value(self.session.env().to_hello_env()).unwrap();
                client.call(&request).await?
            }
            DaemonApiInput::Action { name, args } => run_action(&client, &name, &args).await?,
        };
        Ok(DaemonApiOutcome::Response(value))
    }
}
