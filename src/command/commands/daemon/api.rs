//! Transport for `awman daemon api`. Rendering and catalogue integration are frontend work.
use super::actions::{self, Request};
use crate::command::error::CommandError;
use crate::data::session::Session;
use crate::engine::auth::ApiKey;
use crate::engine::remote::{HttpCore, HttpResponse};
use serde_json::Value;

pub struct DaemonApiClient {
    core: Option<HttpCore>,
    local: Option<std::path::PathBuf>,
}
impl DaemonApiClient {
    pub fn new(session: &Session, target: Option<&str>) -> Result<Self, CommandError> {
        if target.is_none()
            && session
                .config()
                .remote
                .as_ref()
                .and_then(|r| r.default.as_ref())
                .is_none()
        {
            let api_paths = crate::data::fs::ApiPaths::from_env(session.env())?.daemon();
            let api = api_paths.local_endpoint();
            let squad_paths = crate::data::fs::SquadPaths::from_env(session.env())?.daemon();
            let squad = squad_paths.local_endpoint();
            let running = |paths: &crate::data::fs::DaemonPaths| {
                crate::engine::daemon::supervisor::check_already_running(&paths.pid_file())
                    .ok()
                    .flatten()
                    .is_some()
            };
            let endpoint = if running(&api_paths) {
                api
            } else if running(&squad_paths) {
                squad
            } else if api.exists() {
                api
            } else {
                squad
            };
            return Ok(Self::local(endpoint));
        }
        let target = session.effective_config().remote_target(target)?;
        let key = target.api_key.as_deref().map(ApiKey::from_string);
        let pinned = if HttpCore::is_loopback_addr(&target.url) {
            crate::data::fs::ApiPaths::from_env(session.env())
                .ok()
                .and_then(|paths| std::fs::read_to_string(paths.tls_cert_file()).ok())
        } else {
            None
        };
        Ok(Self {
            local: None,
            core: Some(HttpCore::new_with_pinned_cert(
                &target.url,
                "v1",
                key.as_ref(),
                pinned.as_deref(),
            )?),
        })
    }
    pub fn from_core(core: HttpCore) -> Self {
        Self {
            core: Some(core),
            local: None,
        }
    }
    pub fn local(endpoint: std::path::PathBuf) -> Self {
        Self {
            core: None,
            local: Some(endpoint),
        }
    }
    pub async fn send(&self, request: &Request) -> Result<HttpResponse, CommandError> {
        if let Some(endpoint) = &self.local {
            let mut url = reqwest::Url::parse(&format!("http://localhost{}", request.path))
                .map_err(|e| CommandError::RemoteTransport(e.to_string()))?;
            url.query_pairs_mut()
                .extend_pairs(request.query.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            let path = format!(
                "{}{}",
                url.path(),
                url.query().map(|q| format!("?{q}")).unwrap_or_default()
            );
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                crate::engine::remote::local::send(
                    endpoint,
                    &request.method,
                    &path,
                    request.body.as_ref(),
                ),
            )
            .await
            .map_err(|_| {
                CommandError::RemoteTransport("local daemon request timed out".into())
            })??;
            if response.status >= 400 {
                return Err(CommandError::RemoteHttpStatus {
                    status: response.status,
                    body: response.body.to_string(),
                });
            }
            return Ok(response);
        }
        let core = self.core.as_ref().expect("remote transport");
        // Use a versionless join so --raw sends exactly the requested /v1 path.
        let mut url = reqwest::Url::parse(&format!("{}{}", core.base_url(), request.path))
            .map_err(|e| CommandError::RemoteTransport(e.to_string()))?;
        url.query_pairs_mut()
            .extend_pairs(request.query.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|e| CommandError::RemoteTransport(e.to_string()))?;
        let mut req = core.http().request(method, url);
        if let Some(body) = &request.body {
            req = req.json(body)
        }
        let response = req.send().await.map_err(HttpCore::map_reqwest_error)?;
        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(HttpCore::map_reqwest_error)?;
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).to_string().into())
        };
        if status >= 400 {
            return Err(CommandError::RemoteHttpStatus {
                status,
                body: body.to_string(),
            });
        }
        Ok(HttpResponse { status, body })
    }
    async fn all(&self, request: &Request) -> Result<Vec<Value>, CommandError> {
        let mut request = request.clone();
        let mut result = Vec::new();
        loop {
            let response = self.send(&request).await?;
            let page: crate::data::wire::List<Value> = serde_json::from_value(response.body)
                .map_err(|e| CommandError::RemoteTransport(e.to_string()))?;
            result.extend(page.items);
            if let Some(cursor) = page.next_cursor {
                request.query.retain(|(k, _)| k != "cursor");
                request.query.push(("cursor".into(), cursor));
            } else {
                return Ok(result);
            }
        }
    }
    pub async fn action(&self, name: &str, kv: &[(String, String)]) -> Result<Value, CommandError> {
        if name == "grants.delete" && kv.iter().any(|(k, v)| k == "all" && v == "true") {
            let grants = self.all(&actions::plan("grants.list", &[])?).await?;
            let mut deleted = Vec::new();
            for grant in grants {
                if let Some(id) = grant["id"].as_str() {
                    self.send(&actions::plan(
                        "grants.delete",
                        &[("id".into(), id.into())],
                    )?)
                    .await?;
                    deleted.push(id.to_string());
                }
            }
            return Ok(serde_json::json!({"deleted":deleted}));
        }
        if name == "jobs.cancel" {
            if let Some((_, feature)) = kv.iter().find(|(k, _)| k == "feature") {
                let mut ids = Vec::new();
                match feature.as_str() {
                    "worker" => {
                        for item in self
                            .all(&actions::plan(
                                "commands.list",
                                &[("status".into(), "running".into())],
                            )?)
                            .await?
                        {
                            if let Some(id) = item["id"].as_str() {
                                ids.push(id.to_string());
                            }
                        }
                    }
                    "squad" => {
                        for task in self.all(&actions::plan("tasks.list", &[])?).await? {
                            if let Some(id) = task["id"].as_str() {
                                for run in self
                                    .all(&actions::plan(
                                        "runs.list",
                                        &[
                                            ("id".into(), id.into()),
                                            ("status".into(), "running".into()),
                                        ],
                                    )?)
                                    .await?
                                {
                                    if let Some(id) = run["id"].as_str() {
                                        ids.push(id.to_string());
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        return Err(CommandError::Other(
                            "feature must be worker or squad".into(),
                        ))
                    }
                }
                for id in &ids {
                    self.send(&actions::plan("jobs.cancel", &[("id".into(), id.clone())])?)
                        .await?;
                }
                return Ok(serde_json::json!({"canceled":ids}));
            }
        }
        Ok(self.send(&actions::plan(name, kv)?).await?.body)
    }
}
/// Parse --data JSON, @file, or stdin before performing any request.
pub fn data(input: &str) -> Result<Value, CommandError> {
    let text = if input == "-" {
        use std::io::Read;
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| CommandError::Other(e.to_string()))?;
        text
    } else if let Some(path) = input.strip_prefix('@') {
        std::fs::read_to_string(path).map_err(|e| CommandError::Other(e.to_string()))?
    } else {
        input.to_string()
    };
    serde_json::from_str(&text)
        .map_err(|e| CommandError::Other(format!("invalid --data JSON: {e}")))
}

#[derive(Debug, Clone)]
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
    target: Option<String>,
    input: DaemonApiInput,
}
impl DaemonApiCommand {
    pub fn from_input(ctx: &crate::command::dispatch::BuildContext) -> Result<Self, CommandError> {
        let action = ctx.args.get("action").map(str::to_string);
        let args = ctx.args.trailing("args");
        let invalid = |reason: &str| CommandError::InvalidFlagValue {
            command: ctx.path().iter().map(|s| s.to_string()).collect(),
            flag: "raw".into(),
            reason: reason.into(),
        };
        let input = if ctx.flags.bool("list") {
            if action.is_some() || !args.is_empty() {
                return Err(invalid("--list accepts no action or args"));
            }
            DaemonApiInput::List
        } else if let Some(method) = ctx.flags.string("raw") {
            let path = action.ok_or_else(|| invalid("--raw requires METHOD and /v1/path"))?;
            if !args.is_empty() {
                return Err(invalid("--raw accepts only METHOD and path"));
            }
            let body = ctx.flags.string("data").map(|s| data(&s)).transpose()?;
            DaemonApiInput::Raw(actions::raw(&method, &path, body)?)
        } else {
            if ctx.flags.string("data").is_some() {
                return Err(invalid("--data requires --raw"));
            }
            let name = action.ok_or_else(|| invalid("provide an action, --list, or --raw"))?;
            let args = args
                .iter()
                .map(|arg| {
                    if arg == "all" {
                        return Ok(("all".into(), "true".into()));
                    }
                    arg.split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .ok_or_else(|| invalid("action arguments must be key=value"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            DaemonApiInput::Action { name, args }
        };
        Ok(Self::new(
            ctx.session.clone(),
            ctx.remote_target.clone(),
            input,
        ))
    }
    pub fn new(session: Session, target: Option<String>, input: DaemonApiInput) -> Self {
        Self {
            session,
            target,
            input,
        }
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
        if matches!(self.input, DaemonApiInput::List) {
            return Ok(DaemonApiOutcome::List(actions::list()));
        }
        let client = DaemonApiClient::new(&self.session, self.target.as_deref())?;
        let value = match self.input {
            DaemonApiInput::List => unreachable!(),
            DaemonApiInput::Raw(request) => client.send(&request).await?.body,
            DaemonApiInput::Action { name, args } => client.action(&name, &args).await?,
        };
        Ok(DaemonApiOutcome::Response(value))
    }
}
