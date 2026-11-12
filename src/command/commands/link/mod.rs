//! Link policy. Browser grants and work-server credentials never share stores.
use crate::command::commands::daemon::{actions::Request, client::DaemonClient};
use crate::command::{
    dispatch::{BuildContext, CommandFrontend},
    error::CommandError,
};
use crate::data::{
    message::{MessageLevel, UserMessage, UserMessageSink},
    session::Session,
};
use crate::engine::auth::device_flow::ShortCode;
use crate::engine::remote::work_server_link::{DeviceFlowClient, RefreshToken};
pub trait LinkFrontend: UserMessageSink + Send + Sync {
    fn interactive(&self) -> bool;
    fn confirm(&mut self) -> Result<bool, CommandError>;
    fn token_input(&mut self) -> Result<String, CommandError>;
}
impl<T: CommandFrontend> LinkFrontend for T {
    fn interactive(&self) -> bool {
        self.input_available()
    }
    fn confirm(&mut self) -> Result<bool, CommandError> {
        self.confirm_browser_link()
    }
    fn token_input(&mut self) -> Result<String, CommandError> {
        self.read_link_token()
    }
}
pub fn invalid(reason: &str) -> CommandError {
    CommandError::InvalidArgumentValue {
        command: vec!["link".into()],
        argument: "target".into(),
        reason: reason.into(),
    }
}
#[derive(Debug, PartialEq)]
pub enum LinkTarget {
    Browser(String),
    WorkServer(String),
}
impl LinkTarget {
    pub fn parse(target: &str) -> Result<Self, CommandError> {
        if target.to_ascii_lowercase().starts_with("web-") {
            return ShortCode::parse(target, "web")
                .map(|c| Self::Browser(c.as_str().into()))
                .ok_or_else(|| invalid("expected web-XXXX-XXXX or https://server.example"));
        }
        DeviceFlowClient::validate_url(target)
            .map_err(|_| invalid("expected web-XXXX-XXXX or https://server.example"))?;
        Ok(Self::WorkServer(target.trim_end_matches('/').into()))
    }
}
pub struct LinkCommand {
    session: Session,
    target: LinkTarget,
    name: Option<String>,
    labels: Vec<String>,
    token: Option<String>,
}
impl LinkCommand {
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let target = LinkTarget::parse(ctx.args.get("target").unwrap_or(""))?;
        let name = ctx.flags.string("name");
        let labels = ctx.flags.strs("label").to_vec();
        let token = ctx.flags.string("token");
        if matches!(target, LinkTarget::Browser(_))
            && (name.is_some() || !labels.is_empty() || token.is_some())
        {
            return Err(invalid(
                "--name, --label and --token are not valid for a browser code",
            ));
        }
        Ok(Self {
            session: ctx.session.clone(),
            target,
            name,
            labels,
            token,
        })
    }
}
fn say(f: &mut dyn LinkFrontend, text: String) {
    f.write_message(UserMessage {
        level: MessageLevel::Info,
        text,
    });
}
#[async_trait::async_trait]
impl super::Command for LinkCommand {
    type Frontend = Box<dyn LinkFrontend>;
    type Outcome = String;
    async fn run_with_frontend(self, mut f: Self::Frontend) -> Result<String, CommandError> {
        match self.target {
            LinkTarget::Browser(code) => {
                if !f.interactive() {
                    return Err(invalid("run this command in an interactive terminal"));
                }
                let client = DaemonClient::connect(self.session.env())?;
                let view = client
                    .call(&Request::get("/v1/browser-links").with_query("code", &code))
                    .await?;
                let status = client.call(&Request::get("/v1/status")).await?;
                let listener = view["listener"].as_str().unwrap_or("");
                let details = &status["listeners"]["network"][listener];
                let expiry = view["expires_at"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| {
                        (t.with_timezone(&chrono::Utc) - chrono::Utc::now())
                            .num_seconds()
                            .max(0)
                    })
                    .unwrap_or(0);
                let safe = |s: &str| {
                    s.chars()
                        .flat_map(|c| {
                            if c.is_control() {
                                c.escape_default().collect::<Vec<_>>()
                            } else {
                                vec![c]
                            }
                        })
                        .collect::<String>()
                };
                say(&mut *f, format!("Browser link request {code}\n  origin:   {}\n  listener: {} ({}, {})\n  scope:    engine\n  expires:  in {}m{}s\nApprove this browser? [y/N]", safe(view["origin"].as_str().unwrap_or("")), safe(listener), details["listen"].as_str().unwrap_or("unknown"), details["transport"].as_str().unwrap_or("loopback_http"), expiry/60, expiry%60));
                let approve = f.confirm()?;
                client
                    .call(&Request {
                        method: "POST".into(),
                        path: format!(
                            "/v1/browser-links/{}/decision",
                            view["id"]
                                .as_str()
                                .ok_or_else(|| invalid("invalid link response"))?
                        ),
                        query: vec![],
                        body: Some(serde_json::json!({"approve":approve})),
                    })
                    .await?;
                Ok(if approve {
                    "Approved. The browser will finish connecting."
                } else {
                    "Denied."
                }
                .into())
            }
            LinkTarget::WorkServer(url) => {
                let client = DeviceFlowClient::new(&url)?;
                let token = match self.token {
                    Some(t) => RefreshToken::new(if t == "-" { f.token_input()? } else { t })?,
                    None => {
                        let flow = client.begin().await?;
                        say(
                            &mut *f,
                            format!(
                                "Open {} and enter code {}",
                                flow.verification_uri, flow.user_code
                            ),
                        );
                        DeviceFlowClient::open_verification(
                            self.session.env(),
                            &flow.verification_uri,
                        );
                        client.finish(flow).await?
                    }
                };
                let name = self.name.unwrap_or_else(|| client.default_name());
                client.store(self.session.env(), &name, &self.labels, token)?;
                Ok(format!("Linked work server {name}."))
            }
        }
    }
}
