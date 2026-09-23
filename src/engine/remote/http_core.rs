//! `HttpCore` — the single HTTP-transport implementation for talking to an
//! awman HTTP daemon.
//!
//! Layer 1 (WI 0114 F-28, decision Q1): HTTP is a network primitive, and
//! sitting in Layer 2 meant the engine-layer HTTP users below it
//! (`engine::agent::download`, `engine::workflow::poll_ci`) could not reuse
//! the one configured client and each built their own.
//!
//! Every route-specific client in the codebase (today `RemoteClient` against
//! the API server; the squad client in WI 0101 Part 3) is a thin typed façade
//! over one `HttpCore`. There is exactly ONE `reqwest::Client` construction in
//! the tree, and it lives here: the connect/read timeouts, the bearer-token
//! default header, the pinned-cert root, trailing-slash trimming, the uniform
//! `>= 400 → EngineError::RemoteHttpStatus` mapping, and the
//! timeout/connect/transport error classification are all defined once.
//!
//! The API version path segment (`v1`) is the `prefix` field rather than a
//! hardcoded literal, so a second daemon can point the same core at its own
//! prefix.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::engine::auth::ApiKey;
use crate::engine::error::EngineError;

/// Every knob the one `reqwest::Client` builder honours.
///
/// `None` timeouts mean "no timeout", which is `reqwest`'s own default — the
/// CI poller relies on that. Constructing this is the *only* way to get an
/// async HTTP client in awman (WI 0114 F-28): every async
/// `reqwest::Client` in the tree is built by [`HttpCore::client`].
/// `engine::issue::github` still constructs its own `reqwest::blocking`
/// client, which this options type does not cover.
#[derive(Debug, Clone, Default)]
pub struct HttpClientOptions {
    /// Time allowed to establish the TCP/TLS connection.
    pub connect_timeout: Option<Duration>,
    /// Time allowed for the full request/response once connected.
    pub read_timeout: Option<Duration>,
    /// `User-Agent` header sent on every request.
    pub user_agent: Option<String>,
    /// Bearer token sent as `Authorization` on every request.
    pub bearer: Option<String>,
    /// PEM certificate added as an extra trusted root. For a loopback awman
    /// daemon with a self-signed cert this pins by identity; standard webpki
    /// verification stays in force for everything else.
    pub pinned_cert_pem: Option<String>,
    /// Skip certificate verification entirely.
    ///
    /// **Loopback only.** The `api server status` probe talks to `127.0.0.1`
    /// over the daemon's self-signed cert before it knows which cert that is.
    /// Never set this for a target that leaves the machine.
    pub accept_invalid_certs: bool,
}

impl HttpClientOptions {
    /// The daemon-client defaults: a 10s connect and a 600s read timeout.
    pub fn daemon() -> Self {
        Self {
            connect_timeout: Some(HttpCore::CONNECT_TIMEOUT),
            read_timeout: Some(HttpCore::READ_TIMEOUT),
            ..Self::default()
        }
    }

    pub fn with_timeouts(mut self, connect: Duration, read: Duration) -> Self {
        self.connect_timeout = Some(connect);
        self.read_timeout = Some(read);
        self
    }

    pub fn with_user_agent(mut self, agent: impl Into<String>) -> Self {
        self.user_agent = Some(agent.into());
        self
    }

    pub fn with_bearer(mut self, key: &ApiKey) -> Self {
        self.bearer = Some(key.as_str().to_string());
        self
    }

    pub fn with_pinned_cert(mut self, pem: impl Into<String>) -> Self {
        self.pinned_cert_pem = Some(pem.into());
        self
    }
}

/// Generic HTTP transport over a base URL and a version prefix.
pub struct HttpCore {
    base_url: String,
    prefix: &'static str,
    http: reqwest::Client,
}

/// A decoded HTTP response: the numeric status plus the JSON body.
#[derive(Debug, Clone, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

impl HttpCore {
    /// Time allowed to establish the TCP/TLS connection.
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    /// Time allowed for the full request/response once connected.
    pub const READ_TIMEOUT: Duration = Duration::from_secs(600);

    /// Construct a core for `base_url` under `prefix` (e.g. `"v1"`), optionally
    /// carrying a bearer API key on every request.
    pub fn new(
        base_url: &str,
        prefix: &'static str,
        key: Option<&ApiKey>,
    ) -> Result<Self, EngineError> {
        Self::new_with_pinned_cert(base_url, prefix, key, None)
    }

    /// Like [`HttpCore::new`] but additionally trusts a specific PEM-encoded
    /// certificate. Used when talking to a loopback awman daemon with a
    /// self-signed cert: the cert PEM is loaded from the local `tls/` directory
    /// and added as a trusted root, effectively pinning by identity. For
    /// non-loopback targets, the caller MUST NOT pass `pinned_cert_pem` —
    /// standard webpki verification stays in force.
    pub fn new_with_pinned_cert(
        base_url: &str,
        prefix: &'static str,
        key: Option<&ApiKey>,
        pinned_cert_pem: Option<&str>,
    ) -> Result<Self, EngineError> {
        let mut options = HttpClientOptions::daemon();
        if let Some(key) = key {
            options = options.with_bearer(key);
        }
        if let Some(pem) = pinned_cert_pem {
            options = options.with_pinned_cert(pem);
        }
        let http = Self::client(&options)?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            prefix,
            http,
        })
    }

    /// The one `reqwest::Client` builder in the tree.
    ///
    /// Everything async in awman that speaks HTTP — the daemon clients here,
    /// the per-agent Dockerfile download, the CI poller, the `api server
    /// status` probe, the aspec tarball download — constructs its client
    /// through this function, so the timeout, TLS and header policy has one
    /// definition (WI 0114 F-28). The synchronous issue provider
    /// (`engine::issue::github`) is the one holdout and uses
    /// `reqwest::blocking`.
    pub fn client(options: &HttpClientOptions) -> Result<reqwest::Client, EngineError> {
        let mut builder = reqwest::Client::builder();
        if let Some(connect) = options.connect_timeout {
            builder = builder.connect_timeout(connect);
        }
        if let Some(read) = options.read_timeout {
            builder = builder.timeout(read);
        }
        if let Some(agent) = options.user_agent.as_deref() {
            builder = builder.user_agent(agent);
        }
        if let Some(token) = options.bearer.as_deref() {
            let mut headers = reqwest::header::HeaderMap::new();
            let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| EngineError::Other(format!("invalid api key header: {e}")))?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
            builder = builder.default_headers(headers);
        }
        if let Some(pem) = options.pinned_cert_pem.as_deref() {
            let cert = reqwest::Certificate::from_pem(pem.as_bytes())
                .map_err(|e| EngineError::Other(format!("invalid pinned cert: {e}")))?;
            builder = builder.add_root_certificate(cert);
        }
        if options.accept_invalid_certs {
            builder = builder.danger_accept_invalid_certs(true);
        }
        builder
            .build()
            .map_err(|e| EngineError::RemoteTransport(e.to_string()))
    }

    /// The trailing-slash-trimmed base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The version prefix segment (e.g. `"v1"`).
    pub fn prefix(&self) -> &'static str {
        self.prefix
    }

    /// The underlying `reqwest::Client`, for façades that need to issue a
    /// request the three verb helpers do not cover (a typed JSON body, a
    /// per-request timeout override, an SSE stream). Sharing the client is what
    /// keeps a single HTTP client implementation in the tree.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Build the full URL for `path`: `{base_url}/{prefix}/{joined path}`.
    pub fn url(&self, path: &[&str]) -> String {
        format!("{}/{}/{}", self.base_url, self.prefix, path.join("/"))
    }

    /// `GET {base}/{prefix}/{path}` — decode the JSON body, mapping `>= 400` to
    /// [`EngineError::RemoteHttpStatus`].
    pub async fn get(&self, path: &[&str]) -> Result<HttpResponse, EngineError> {
        let url = self.url(path);
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
            return Err(EngineError::RemoteHttpStatus {
                status,
                body: body.to_string(),
            });
        }
        Ok(HttpResponse { status, body })
    }

    /// `DELETE {base}/{prefix}/{path}` — like [`HttpCore::get`] but tolerates a
    /// non-JSON body by falling back to `{}`.
    pub async fn delete(&self, path: &[&str]) -> Result<HttpResponse, EngineError> {
        let url = self.url(path);
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
            return Err(EngineError::RemoteHttpStatus {
                status,
                body: body.to_string(),
            });
        }
        Ok(HttpResponse { status, body })
    }

    /// `POST {base}/{prefix}/{path}` with a JSON object body built from `flags`
    /// and the given request `headers`. Maps `>= 400` to
    /// [`EngineError::RemoteHttpStatus`].
    pub async fn post_command(
        &self,
        path: &[&str],
        flags: &[(&str, Value)],
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, EngineError> {
        let url = self.url(path);
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
            return Err(EngineError::RemoteHttpStatus {
                status,
                body: body.to_string(),
            });
        }
        Ok(HttpResponse { status, body })
    }

    /// Classify a `reqwest` error into the appropriate `EngineError`:
    /// timeouts → `RemoteTimeout`, connect failures → `RemoteConnectionRefused`,
    /// everything else → `RemoteTransport`.
    pub fn map_reqwest_error(e: reqwest::Error) -> EngineError {
        if e.is_timeout() {
            EngineError::RemoteTimeout
        } else if e.is_connect() {
            EngineError::RemoteConnectionRefused(e.to_string())
        } else {
            EngineError::RemoteTransport(e.to_string())
        }
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
}
