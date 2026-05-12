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
        Self::new_with_pinned_cert(base_url, api_key, None)
    }

    /// Construct a client that additionally trusts a specific PEM-encoded
    /// certificate. Used when talking to a loopback amux headless server with
    /// a self-signed cert: the cert PEM is loaded from the local `tls/`
    /// directory and added as a trusted root, effectively pinning by identity.
    /// For non-loopback targets, the caller MUST NOT pass `pinned_cert_pem` —
    /// standard webpki verification stays in force.
    pub fn new_with_pinned_cert(
        base_url: &str,
        api_key: Option<&ApiKey>,
        pinned_cert_pem: Option<&str>,
    ) -> Result<Self, CommandError> {
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
        if let Some(pem) = pinned_cert_pem {
            let cert = reqwest::Certificate::from_pem(pem.as_bytes())
                .map_err(|e| CommandError::Other(format!("invalid pinned cert: {e}")))?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder
            .build()
            .map_err(|e| CommandError::RemoteTransport(e.to_string()))?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
        })
    }

    /// Returns `true` when `addr` resolves to a loopback host (`127.0.0.1`,
    /// `::1`, `localhost`). Used to decide whether the locally-stored
    /// self-signed cert should be trusted.
    pub fn is_loopback_addr(addr: &str) -> bool {
        let trimmed = addr.trim();
        let after_scheme = trimmed
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(trimmed);
        let host_part = after_scheme
            .split_once('/')
            .map(|(h, _)| h)
            .unwrap_or(after_scheme);
        let host = host_part
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_part);
        let host = host.trim_start_matches('[').trim_end_matches(']');
        matches!(host, "127.0.0.1" | "::1" | "localhost")
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
            if let (Some(default_addr), Some(default_key)) = (
                remote.default_addr.as_deref(),
                remote.default_api_key.as_deref(),
            ) {
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
        self.send_command_with_headers(path, flags, &[]).await
    }

    /// Like `send_command` but also attaches request headers — used to set
    /// `x-amux-session` on `POST /v1/commands` (the server reads the session
    /// from the header, not the body).
    pub async fn send_command_with_headers(
        &self,
        path: &[&str],
        flags: &[(&str, serde_json::Value)],
        headers: &[(&str, &str)],
    ) -> Result<RemoteResponse, CommandError> {
        let url = format!("{}/v1/{}", self.base_url, path.join("/"));
        let mut body = serde_json::Map::new();
        for (k, v) in flags {
            body.insert(k.to_string(), v.clone());
        }
        let mut req = self.http.post(&url).json(&serde_json::Value::Object(body));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await.map_err(Self::map_reqwest_error)?;
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

    /// Stream SSE events from the remote server progressively. Reads byte
    /// chunks as they arrive, splits on the `\n\n` event separator, and
    /// dispatches each event to the sink the moment it is parsed. The
    /// per-request timeout is set to 24h so a long-running command doesn't
    /// get cut off by the default client read timeout.
    pub async fn stream_command(
        &self,
        path: &[&str],
        _flags: &[(&str, serde_json::Value)],
        sink: &mut dyn RemoteEventSink,
    ) -> Result<(), CommandError> {
        use futures_util::StreamExt;

        let url = format!("{}/v1/{}", self.base_url, path.join("/"));

        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(86400))
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        if resp.status().as_u16() >= 400 {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(CommandError::RemoteHttpStatus { status, body });
        }

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res.map_err(|e| CommandError::RemoteTransport(e.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Pull every complete `\n\n`-delimited event block out of the buffer
            // and dispatch it. Whatever's left after the final separator stays
            // in the buffer until more bytes arrive.
            while let Some(pos) = buffer.find("\n\n") {
                let event_block = buffer[..pos].to_string();
                buffer.drain(..pos + 2);
                if Self::dispatch_sse_event(&event_block, sink) {
                    return Ok(());
                }
            }
        }

        // Stream ended without [amux:done] — emit any partial event then close.
        if !buffer.trim().is_empty() {
            let trailing = std::mem::take(&mut buffer);
            if Self::dispatch_sse_event(&trailing, sink) {
                return Ok(());
            }
        }
        sink.on_done();
        Ok(())
    }

    /// Parse one `\n\n`-delimited SSE event block and forward it to the sink.
    /// Returns `true` when the block was the `[amux:done]` sentinel (caller
    /// should stop streaming).
    fn dispatch_sse_event(block: &str, sink: &mut dyn RemoteEventSink) -> bool {
        if block.trim().is_empty() {
            return false;
        }
        let mut event_type = "message";
        let mut data_lines: Vec<&str> = Vec::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                event_type = rest;
            } else if let Some(rest) = line.strip_prefix("event:") {
                event_type = rest;
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data_lines.push(rest);
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest);
            }
        }
        let data = data_lines.join("\n");
        if data == "[amux:done]" {
            sink.on_done();
            return true;
        }
        sink.on_event(event_type, &data);
        false
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
    let path_render = if path_part == "/" {
        ""
    } else {
        path_part.as_str()
    };
    format!("{scheme}://{host}{port_render}{path_render}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::EnvSnapshot;
    use crate::data::session::{Session, SessionOpenOptions};

    // ─── is_loopback_addr ─────────────────────────────────────────────────────

    #[test]
    fn loopback_addr_recognizes_ipv4_and_ipv6_and_localhost() {
        assert!(RemoteClient::is_loopback_addr("https://127.0.0.1:9876"));
        assert!(RemoteClient::is_loopback_addr("http://127.0.0.1:9876/"));
        assert!(RemoteClient::is_loopback_addr("https://localhost"));
        assert!(RemoteClient::is_loopback_addr("https://[::1]:9876"));
    }

    #[test]
    fn loopback_addr_rejects_remote_hosts() {
        assert!(!RemoteClient::is_loopback_addr("https://example.com:9876"));
        assert!(!RemoteClient::is_loopback_addr("http://10.0.0.1"));
        assert!(!RemoteClient::is_loopback_addr("https://my-host"));
    }

    // ─── URL canonicalize helpers ─────────────────────────────────────────────

    #[test]
    fn url_canonicalize_default_port_elided() {
        assert_eq!(canonicalize_url("http://1.2.3.4:80/"), "http://1.2.3.4");
        assert_eq!(
            canonicalize_url("https://example.com:443/"),
            "https://example.com"
        );
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
        let session =
            Session::open_at_git_root(tmp.path().to_path_buf(), tmp.path().to_path_buf(), opts)
                .unwrap();
        (tmp, session)
    }

    fn make_session_with_global_config(config_json: &str) -> (tempfile::TempDir, Session) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.json"), config_json).unwrap();
        let env = EnvSnapshot::with_overrides([("AMUX_CONFIG_HOME", tmp.path().to_str().unwrap())]);
        let opts = SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        };
        let session =
            Session::open_at_git_root(tmp.path().to_path_buf(), tmp.path().to_path_buf(), opts)
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
        let result = RemoteClient::resolve_api_key(&session, "http://localhost:9876", None);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().unwrap().as_str(),
            "env-key",
            "env var must be used when no explicit key"
        );
    }

    #[test]
    fn resolve_api_key_global_config_matched_by_default_addr() {
        let config_json =
            r#"{"remote":{"defaultAddr":"http://localhost:9876","defaultAPIKey":"config-key"}}"#;
        let (_tmp, session) = make_session_with_global_config(config_json);
        let result = RemoteClient::resolve_api_key(&session, "http://localhost:9876", None);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().unwrap().as_str(),
            "config-key",
            "global config key must be returned when target_addr matches default_addr"
        );
    }

    #[test]
    fn resolve_api_key_global_config_not_used_when_addr_differs() {
        let config_json =
            r#"{"remote":{"defaultAddr":"http://other-host:9876","defaultAPIKey":"config-key"}}"#;
        let (_tmp, session) = make_session_with_global_config(config_json);
        let result = RemoteClient::resolve_api_key(&session, "http://localhost:9876", None);
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "config key must NOT be returned when addr does not match"
        );
    }

    #[test]
    fn resolve_api_key_returns_none_when_no_source_available() {
        let (_tmp, session) = make_session(EnvSnapshot::empty());
        let result = RemoteClient::resolve_api_key(&session, "http://localhost:9876", None);
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "must return None when no key source exists"
        );
    }

    #[test]
    fn resolve_api_key_explicit_blank_falls_through_to_env() {
        let env = EnvSnapshot::with_overrides([("AMUX_API_KEY", "env-key")]);
        let (_tmp, session) = make_session(env);
        // An explicit empty string should fall through to env.
        let result = RemoteClient::resolve_api_key(&session, "http://localhost:9876", Some("   "));
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
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
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
            matches!(
                result,
                Err(CommandError::RemoteHttpStatus { status: 400, .. })
            ),
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
            matches!(
                result,
                Err(CommandError::RemoteHttpStatus { status: 500, .. })
            ),
            "500 must map to RemoteHttpStatus, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn stream_command_parses_sse_events_and_calls_sink() {
        use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

        let sse_body = "data: hello world\n\ndata: second line\n\ndata: [amux:done]\n\n";

        let server = MockServer::start().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/v1/commands/cmd-1/logs/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body),
            )
            .mount(&server)
            .await;

        let client = RemoteClient::new(&server.uri(), None).unwrap();

        struct CollectSink {
            events: Vec<(String, String)>,
            done: bool,
        }
        impl RemoteEventSink for CollectSink {
            fn on_event(&mut self, event_type: &str, data: &str) {
                self.events.push((event_type.to_string(), data.to_string()));
            }
            fn on_done(&mut self) {
                self.done = true;
            }
        }

        let mut sink = CollectSink {
            events: Vec::new(),
            done: false,
        };
        let result = client
            .stream_command(&["commands", "cmd-1", "logs", "stream"], &[], &mut sink)
            .await;
        assert!(result.is_ok(), "stream_command should succeed: {result:?}");
        assert!(sink.done, "on_done must be called");
        assert_eq!(sink.events.len(), 2);
        assert_eq!(sink.events[0].1, "hello world");
        assert_eq!(sink.events[1].1, "second line");
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
