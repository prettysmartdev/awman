//! `RemoteClient` — typed HTTP client for talking to a remote amux headless
//! server. Constructed fresh per `RemoteCommand` invocation; not exported
//! beyond `command/commands/`.

use std::time::Duration;

use serde::Deserialize;

use crate::command::error::CommandError;
use crate::data::session::Session;
use crate::engine::auth::ApiKey;

pub struct RemoteClient {
    base_url: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

pub trait RemoteEventSink: Send + Sync {
    fn on_event(&mut self, event_type: &str, data: &str);
    fn on_done(&mut self);
}

impl RemoteClient {
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    pub const READ_TIMEOUT: Duration = Duration::from_secs(600);

    pub fn new(base_url: &str, api_key: Option<&ApiKey>) -> Result<Self, CommandError> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Self::CONNECT_TIMEOUT)
            .timeout(Self::READ_TIMEOUT);
        if let Some(key) = api_key {
            let mut headers = reqwest::header::HeaderMap::new();
            let auth_value = format!("Bearer {}", key.as_str());
            let value = reqwest::header::HeaderValue::from_str(&auth_value)
                .map_err(|e| CommandError::Other(format!("invalid api key header: {e}")))?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
            builder = builder.default_headers(headers);
        }
        let http = builder
            .build()
            .map_err(|e| CommandError::RemoteTransport(e.to_string()))?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
        })
    }

    /// API-key resolution per spec §6.5: explicit > AMUX_API_KEY > global
    /// config (only when target_addr matches global default_addr).
    pub fn resolve_api_key(
        session: &Session,
        target_addr: &str,
        explicit: Option<&str>,
    ) -> Result<Option<ApiKey>, CommandError> {
        if let Some(explicit) = explicit {
            let trimmed = explicit.trim();
            if !trimmed.is_empty() {
                return Ok(Some(ApiKey::from_string(trimmed)));
            }
        }
        if let Some(env) = session.env().api_key() {
            let trimmed = env.trim();
            if !trimmed.is_empty() {
                return Ok(Some(ApiKey::from_string(trimmed)));
            }
        }
        // Compare canonicalized URLs against the global config default.
        let global = session.global_config();
        if let Some(remote) = global.remote.as_ref() {
            if let (Some(default_addr), Some(default_key)) =
                (remote.default_addr.as_deref(), remote.default_api_key.as_deref())
            {
                if canonicalize_url(target_addr) == canonicalize_url(default_addr) {
                    return Ok(Some(ApiKey::from_string(default_key)));
                }
            }
        }
        Ok(None)
    }

    pub async fn send_command(
        &self,
        path: &[&str],
        flags: &[(&str, serde_json::Value)],
    ) -> Result<RemoteResponse, CommandError> {
        let url = format!("{}/v1/{}", self.base_url, path.join("/"));
        let mut body = serde_json::Map::new();
        for (k, v) in flags {
            body.insert(k.to_string(), v.clone());
        }
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;
        let status = resp.status().as_u16();
        let body = resp
            .json::<serde_json::Value>()
            .await
            .map_err(Self::map_reqwest_error)?;
        if status >= 400 {
            return Err(CommandError::RemoteHttpStatus {
                status,
                body: body.to_string(),
            });
        }
        Ok(RemoteResponse { status, body })
    }

    pub async fn get(&self, path: &[&str]) -> Result<RemoteResponse, CommandError> {
        let url = format!("{}/v1/{}", self.base_url, path.join("/"));
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;
        let status = resp.status().as_u16();
        let body = resp
            .json::<serde_json::Value>()
            .await
            .map_err(Self::map_reqwest_error)?;
        if status >= 400 {
            return Err(CommandError::RemoteHttpStatus {
                status,
                body: body.to_string(),
            });
        }
        Ok(RemoteResponse { status, body })
    }

    pub async fn delete(&self, path: &[&str]) -> Result<RemoteResponse, CommandError> {
        let url = format!("{}/v1/{}", self.base_url, path.join("/"));
        let resp = self
            .http
            .delete(&url)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;
        let status = resp.status().as_u16();
        let body = resp
            .json::<serde_json::Value>()
            .await
            .unwrap_or(serde_json::json!({}));
        if status >= 400 {
            return Err(CommandError::RemoteHttpStatus {
                status,
                body: body.to_string(),
            });
        }
        Ok(RemoteResponse { status, body })
    }

    /// Stream SSE events from the remote server. Disables the read timeout
    /// so long-running commands don't hit the 600s ceiling.
    pub async fn stream_command(
        &self,
        path: &[&str],
        flags: &[(&str, serde_json::Value)],
        _sink: &mut dyn RemoteEventSink,
    ) -> Result<(), CommandError> {
        // Streaming is wired up in 0070 against a real headless server; this
        // entry point exists so the API surface is stable.
        let _ = (path, flags);
        Err(CommandError::NotImplemented(
            "RemoteClient::stream_command",
        ))
    }

    pub fn map_reqwest_error(e: reqwest::Error) -> CommandError {
        if e.is_timeout() {
            CommandError::RemoteTimeout
        } else if e.is_connect() {
            CommandError::RemoteConnectionRefused(e.to_string())
        } else {
            CommandError::RemoteTransport(e.to_string())
        }
    }
}

/// Canonicalize a URL for the default-addr comparison rule (§6.5):
///   - lowercase scheme
///   - lowercase host
///   - elide default ports (80/http, 443/https)
///   - normalize trailing slash
fn canonicalize_url(s: &str) -> String {
    let s = s.trim();
    let (scheme_part, rest) = match s.split_once("://") {
        Some(t) => t,
        None => return s.to_lowercase(),
    };
    let scheme = scheme_part.to_lowercase();
    let (host_part, path_part) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match host_part.split_once(':') {
        Some((h, p)) => (h.to_lowercase(), Some(p.to_string())),
        None => (host_part.to_lowercase(), None),
    };
    let port_render = match (scheme.as_str(), port.as_deref()) {
        ("http", Some("80")) | ("https", Some("443")) | (_, None) => String::new(),
        (_, Some(p)) => format!(":{p}"),
    };
    let path_render = if path_part == "/" { "" } else { path_part.as_str() };
    format!("{scheme}://{host}{port_render}{path_render}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::EnvSnapshot;
    use crate::data::session::{Session, SessionOpenOptions};

    // ─── URL canonicalize helpers ─────────────────────────────────────────────

    #[test]
    fn url_canonicalize_default_port_elided() {
        assert_eq!(canonicalize_url("http://1.2.3.4:80/"), "http://1.2.3.4");
        assert_eq!(canonicalize_url("https://example.com:443/"), "https://example.com");
    }

    #[test]
    fn url_canonicalize_case_insensitive_scheme_and_host() {
        assert_eq!(
            canonicalize_url("HTTP://Example.COM/"),
            "http://example.com"
        );
    }

    #[test]
    fn url_canonicalize_distinguishes_schemes() {
        assert_ne!(
            canonicalize_url("https://example.com/"),
            canonicalize_url("http://example.com/"),
        );
    }

    // ─── Test-session helpers ─────────────────────────────────────────────────

    fn make_session(env: EnvSnapshot) -> (tempfile::TempDir, Session) {
        let tmp = tempfile::tempdir().unwrap();
        let opts = SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        };
        let session = Session::open_at_git_root(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            opts,
        )
        .unwrap();
        (tmp, session)
    }

    fn make_session_with_global_config(config_json: &str) -> (tempfile::TempDir, Session) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.json"), config_json).unwrap();
        let env = EnvSnapshot::with_overrides([(
            "AMUX_CONFIG_HOME",
            tmp.path().to_str().unwrap(),
        )]);
        let opts = SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        };
        let session = Session::open_at_git_root(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            opts,
        )
        .unwrap();
        (tmp, session)
    }

    // ─── resolve_api_key tests ────────────────────────────────────────────────

    #[test]
    fn resolve_api_key_explicit_takes_priority_over_env_and_config() {
        let env = EnvSnapshot::with_overrides([("AMUX_API_KEY", "env-key")]);
        let (_tmp, session) = make_session(env);
        let result =
            RemoteClient::resolve_api_key(&session, "http://localhost:9876", Some("explicit-key"));
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().unwrap().as_str(),
            "explicit-key",
            "explicit key must win over env"
        );
    }

    #[test]
    fn resolve_api_key_env_var_used_when_no_explicit() {
        let env = EnvSnapshot::with_overrides([("AMUX_API_KEY", "env-key")]);
        let (_tmp, session) = make_session(env);
        let result =
            RemoteClient::resolve_api_key(&session, "http://localhost:9876", None);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().unwrap().as_str(),
            "env-key",
            "env var must be used when no explicit key"
        );
    }

    #[test]
    fn resolve_api_key_global_config_matched_by_default_addr() {
        let config_json = r#"{"remote":{"defaultAddr":"http://localhost:9876","defaultAPIKey":"config-key"}}"#;
        let (_tmp, session) = make_session_with_global_config(config_json);
        let result =
            RemoteClient::resolve_api_key(&session, "http://localhost:9876", None);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().unwrap().as_str(),
            "config-key",
            "global config key must be returned when target_addr matches default_addr"
        );
    }

    #[test]
    fn resolve_api_key_global_config_not_used_when_addr_differs() {
        let config_json = r#"{"remote":{"defaultAddr":"http://other-host:9876","defaultAPIKey":"config-key"}}"#;
        let (_tmp, session) = make_session_with_global_config(config_json);
        let result =
            RemoteClient::resolve_api_key(&session, "http://localhost:9876", None);
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "config key must NOT be returned when addr does not match"
        );
    }

    #[test]
    fn resolve_api_key_returns_none_when_no_source_available() {
        let (_tmp, session) = make_session(EnvSnapshot::empty());
        let result =
            RemoteClient::resolve_api_key(&session, "http://localhost:9876", None);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none(), "must return None when no key source exists");
    }

    #[test]
    fn resolve_api_key_explicit_blank_falls_through_to_env() {
        let env = EnvSnapshot::with_overrides([("AMUX_API_KEY", "env-key")]);
        let (_tmp, session) = make_session(env);
        // An explicit empty string should fall through to env.
        let result =
            RemoteClient::resolve_api_key(&session, "http://localhost:9876", Some("   "));
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().unwrap().as_str(),
            "env-key",
            "blank explicit key must fall through to env"
        );
    }

    // ─── send_command tests (mock HTTP server) ────────────────────────────────

    #[tokio::test]
    async fn send_command_200_response_returns_parsed_remote_response() {
        use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/v1/status"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true})),
            )
            .mount(&server)
            .await;

        let client = RemoteClient::new(&server.uri(), None).unwrap();
        let result = client.send_command(&["status"], &[]).await;
        assert!(result.is_ok(), "200 must return Ok: {result:?}");
        let response = result.unwrap();
        assert_eq!(response.status, 200);
    }

    #[tokio::test]
    async fn send_command_400_response_maps_to_remote_http_status_error() {
        use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/v1/exec/workflow"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "bad request"})),
            )
            .mount(&server)
            .await;

        let client = RemoteClient::new(&server.uri(), None).unwrap();
        let result = client.send_command(&["exec", "workflow"], &[]).await;
        assert!(
            matches!(result, Err(CommandError::RemoteHttpStatus { status: 400, .. })),
            "400 must map to RemoteHttpStatus, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn send_command_500_response_maps_to_remote_http_status_error() {
        use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/v1/status"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_json(serde_json::json!({"error": "internal server error"})),
            )
            .mount(&server)
            .await;

        let client = RemoteClient::new(&server.uri(), None).unwrap();
        let result = client.send_command(&["status"], &[]).await;
        assert!(
            matches!(result, Err(CommandError::RemoteHttpStatus { status: 500, .. })),
            "500 must map to RemoteHttpStatus, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn stream_command_returns_not_implemented() {
        let client = RemoteClient::new("http://localhost:9876", None).unwrap();
        struct NoopSink;
        impl RemoteEventSink for NoopSink {
            fn on_event(&mut self, _event_type: &str, _data: &str) {}
            fn on_done(&mut self) {}
        }
        let result = client
            .stream_command(&["status"], &[], &mut NoopSink)
            .await;
        assert!(
            matches!(result, Err(CommandError::NotImplemented(_))),
            "stream_command must return NotImplemented until 0070: {result:?}"
        );
    }

    #[tokio::test]
    async fn map_reqwest_error_connection_refused_maps_to_remote_connection_refused() {
        // Port 1 is reserved and should never have anything listening.
        let client = RemoteClient::new("http://127.0.0.1:1", None).unwrap();
        let result = client.send_command(&["status"], &[]).await;
        assert!(
            matches!(result, Err(CommandError::RemoteConnectionRefused(_))),
            "connection refused must map to RemoteConnectionRefused, got: {result:?}"
        );
    }
}
