//! `DaemonClient`: the one way a CLI command reaches the running daemon.
//!
//! Every local caller (`awman daemon status|stop|api`, `config set` live
//! apply, `awman squad …`) talks to the daemon over its owner-only local
//! listener (Unix socket or named pipe). There is no bearer key and no TCP:
//! the transport proves the caller is the daemon's owner.
use super::actions::Request;
use crate::command::error::CommandError;
use crate::data::config::env::EnvSnapshot;
use crate::data::fs::DataPaths;
use crate::engine::remote::local::{discover, LocalClient, LocalEndpoint};
use crate::engine::remote::HttpResponse;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::time::Duration;

/// A connection to the running daemon's local listener.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    local: LocalClient,
    pid: u32,
}

impl DaemonClient {
    /// The running daemon for `env`'s home, or `None` when none is running.
    pub fn discover(env: &EnvSnapshot) -> Result<Option<Self>, CommandError> {
        let paths = DataPaths::from_env(env)?.daemon();
        Ok(discover(&paths)?.map(Self::from_endpoint))
    }

    /// The running daemon, or [`CommandError::DaemonNotRunning`].
    pub fn connect(env: &EnvSnapshot) -> Result<Self, CommandError> {
        Self::discover(env)?.ok_or(CommandError::DaemonNotRunning)
    }

    pub fn from_endpoint(endpoint: LocalEndpoint) -> Self {
        Self {
            local: LocalClient::new(endpoint.address),
            pid: endpoint.pid,
        }
    }

    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self {
            local: self.local.with_timeout(timeout),
            ..self
        }
    }

    /// The daemon's PID, from its live pidfile.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Send `request` and return the response whatever its status.
    pub async fn send(&self, request: &Request) -> Result<HttpResponse, CommandError> {
        Ok(self
            .local
            .send(&request.method, &request.target(), request.body.as_ref())
            .await?)
    }

    /// Send `request`; a 4xx/5xx answer is [`CommandError::DaemonHttpStatus`]
    /// carrying the response body.
    pub async fn call(&self, request: &Request) -> Result<Value, CommandError> {
        let response = self.send(request).await?;
        if response.status >= 400 {
            return Err(CommandError::DaemonHttpStatus {
                status: response.status,
                body: body_text(&response.body),
            });
        }
        Ok(response.body)
    }

    /// [`Self::call`], deserialized.
    pub async fn call_as<T: DeserializeOwned>(&self, request: &Request) -> Result<T, CommandError> {
        serde_json::from_value(self.call(request).await?)
            .map_err(|e| CommandError::Other(format!("unexpected daemon response: {e}")))
    }

    /// Every item of a `{items, next_cursor}` list, following each cursor.
    pub async fn all(&self, request: &Request) -> Result<Vec<Value>, CommandError> {
        let mut request = request.clone();
        let mut items = Vec::new();
        loop {
            let page: crate::data::wire::List<Value> = self.call_as(&request).await?;
            items.extend(page.items);
            match page.next_cursor {
                Some(cursor) => {
                    request.query.retain(|(k, _)| k != "cursor");
                    request.query.push(("cursor".into(), cursor));
                }
                None => return Ok(items),
            }
        }
    }
}

/// A response body as the text a user sees: strings verbatim, JSON compact.
pub fn body_text(body: &Value) -> String {
    match body {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_daemon_is_daemon_not_running() {
        let home = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([("AWMAN_HOME", home.path().to_str().unwrap())]);
        assert!(DaemonClient::discover(&env).unwrap().is_none());
        assert!(matches!(
            DaemonClient::connect(&env).unwrap_err(),
            CommandError::DaemonNotRunning
        ));
    }

    #[test]
    fn error_bodies_render_as_text() {
        assert_eq!(body_text(&Value::String("x".into())), "x");
        assert_eq!(body_text(&Value::Null), "");
        assert_eq!(
            body_text(&serde_json::json!({"error":"e","code":"c"})),
            r#"{"code":"c","error":"e"}"#
        );
    }
}
