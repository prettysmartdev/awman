//! Worker-initiated HTTPS transport for WSP v1.
use crate::types::*;
use reqwest::{Method, StatusCode};
use serde::{de::DeserializeOwned, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

#[derive(thiserror::Error)]
pub enum Error {
    #[error("work server URL must be HTTPS (or HTTP loopback for tests)")]
    InsecureUrl,
    #[error("invalid work server URL")]
    InvalidUrl,
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("work server returned {status}: {code}")]
    Server {
        status: StatusCode,
        code: String,
        message: String,
    },
    #[error("work server link was revoked")]
    LinkRevoked,
    #[error("link approval is pending")]
    LinkPending,
    #[error("link approval was denied")]
    LinkDenied,
    #[error("work server conflict: {code}")]
    Conflict { code: String, message: String },
    #[cfg(feature = "terminal")]
    #[error("terminal transport failed")]
    TerminalTransport(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("work server returned an invalid response")]
    InvalidResponse,
}

/// A client holds its refresh token only in memory. The owner stores it in an
/// owner-only state file; it must never be placed in config or a URL.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: reqwest::Url,
    refresh_token: Arc<String>,
    access: Arc<Mutex<Option<AccessToken>>>,
    link_polls: Arc<Mutex<HashMap<String, (u64, tokio::time::Instant)>>>,
    lease_epochs: Arc<Mutex<HashMap<String, u64>>>,
    pinned_epoch: Option<(String, u64)>,
}

impl Client {
    pub fn new(base: &str, refresh_token: String) -> Result<Self, Error> {
        let base = reqwest::Url::parse(base).map_err(|_| Error::InvalidUrl)?;
        let loopback = base
            .host_str()
            .is_some_and(|h| matches!(h, "localhost" | "127.0.0.1" | "[::1]"));
        if base.scheme() != "https" && !(base.scheme() == "http" && loopback) {
            return Err(Error::InsecureUrl);
        }
        if base.username() != ""
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(Error::InvalidUrl);
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(40))
                .build()?,
            lease_epochs: Arc::new(Mutex::new(HashMap::new())),
            pinned_epoch: None,
            base,
            refresh_token: Arc::new(refresh_token),
            access: Arc::new(Mutex::new(None)),
            link_polls: Arc::default(),
        })
    }

    pub(crate) fn url(&self, suffix: &str) -> Result<reqwest::Url, Error> {
        let mut url = self.base.clone();
        let (path, query) = suffix
            .split_once('?')
            .map_or((suffix, None), |(p, q)| (p, Some(q)));
        let mut segments = url.path_segments_mut().map_err(|_| Error::InvalidUrl)?;
        segments.pop_if_empty();
        for segment in path.trim_start_matches('/').split('/') {
            segments.push(segment);
        }
        drop(segments);
        url.set_query(query);
        Ok(url)
    }

    pub(crate) async fn token(&self) -> Result<String, Error> {
        let mut access = self.access.lock().await;
        if let Some(token) = access.as_ref() {
            let still_valid = token
                .expires_at
                .is_none_or(|at| at > chrono::Utc::now() + chrono::Duration::seconds(30));
            if still_valid && !token.access_token.is_empty() {
                return Ok(token.access_token.clone());
            }
        }
        let response = self
            .http
            .post(self.url("v1/token")?)
            .header("x-wsp-version", "1")
            .json(&TokenRequest {
                refresh_token: self.refresh_token.as_ref().clone(),
            })
            .send()
            .await?;
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(Error::LinkRevoked);
        }
        let token: AccessToken = decode(response).await?;
        if token.access_token.is_empty() {
            return Err(Error::InvalidResponse);
        }
        let value = token.access_token.clone();
        *access = Some(token);
        Ok(value)
    }

    /// Device linking is deliberately unauthenticated; a server decides who
    /// may approve the code before it returns a refresh token.
    pub async fn start_link(&self) -> Result<LinkStart, Error> {
        let start: LinkStart = decode(
            self.http
                .post(self.url("v1/link")?)
                .header("x-wsp-version", "1")
                .send()
                .await?,
        )
        .await?;
        self.defer_link_poll(&start.link_id, start.interval_ms)
            .await;
        Ok(start)
    }

    async fn defer_link_poll(&self, id: &str, interval_ms: u64) {
        let interval_ms = interval_ms.clamp(1, 600_000);
        self.link_polls.lock().await.insert(
            id.to_owned(),
            (
                interval_ms,
                tokio::time::Instant::now() + std::time::Duration::from_millis(interval_ms),
            ),
        );
    }

    /// `None` means approval is pending. All other non-success responses use
    /// WSP's `{error, code}` shape.
    pub async fn poll_link(&self, link_id: &str) -> Result<Option<LinkToken>, Error> {
        let link_id = path_id(link_id)?;
        let poll = self.link_polls.lock().await.get(link_id).copied();
        if let Some((_, at)) = poll {
            tokio::time::sleep_until(at).await;
        }
        let response = self
            .http
            .post(self.url(&format!("v1/link/{link_id}/token"))?)
            .header("x-wsp-version", "1")
            .send()
            .await?;
        if matches!(
            response.status(),
            StatusCode::ACCEPTED | StatusCode::NO_CONTENT
        ) {
            let pending: LinkPending = response.json().await.unwrap_or_default();
            let interval = pending
                .interval_ms
                .or(poll.map(|(ms, _)| ms))
                .unwrap_or(100);
            self.defer_link_poll(link_id, interval).await;
            return Ok(None);
        }
        let token: LinkToken = decode(response).await?;
        if token.refresh_token.is_empty() {
            return Err(Error::InvalidResponse);
        }
        self.link_polls.lock().await.remove(link_id);
        Ok(Some(token))
    }

    async fn send<T: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: Option<&T>,
    ) -> Result<reqwest::Response, Error> {
        // Capture before refresh/retry: an in-flight old run must keep its epoch.
        let epoch = if let Some(rest) = path.strip_prefix("v1/leases/") {
            Some(
                self.lease_epoch(rest.split('/').next().ok_or(Error::InvalidUrl)?)
                    .await,
            )
        } else {
            None
        };
        for retry in 0..2 {
            let token = self.token().await?;
            let mut request = self
                .http
                .request(method.clone(), self.url(path)?)
                .header("x-wsp-version", "1")
                .bearer_auth(&token);
            if let Some(epoch) = epoch {
                request = request.header("x-wsp-epoch", epoch.to_string());
            }
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request.send().await?;
            if response.status() == StatusCode::UNAUTHORIZED && retry == 0 {
                self.invalidate_access(&token).await;
                continue;
            }
            if !response.status().is_success() {
                return Err(server_error(response).await);
            }
            return Ok(response);
        }
        unreachable!("bounded retry returns on its second request")
    }

    /// Pin a lease handle to its claim-time epoch, including after another run
    /// reclaims the same ID. Engine lease handles should use this clone.
    pub fn with_lease_epoch(&self, id: &str, epoch: u64) -> Self {
        let mut client = self.clone();
        client.pinned_epoch = Some((id.to_owned(), epoch));
        client
    }

    pub(crate) async fn lease_epoch(&self, id: &str) -> u64 {
        if let Some((pinned_id, epoch)) = &self.pinned_epoch {
            if pinned_id == id {
                return *epoch;
            }
        }
        // Unknown IDs are fenced; never omit the header and adopt the server's epoch.
        self.lease_epochs.lock().await.get(id).copied().unwrap_or(0)
    }

    /// Seed the access-token cache when resuming an authenticated connection.
    /// Expired tokens still take the ordinary refresh path before a request.
    pub async fn with_access_token_cache(self, token: AccessToken) -> Self {
        *self.access.lock().await = Some(token);
        self
    }
    pub(crate) async fn invalidate_access(&self, rejected: &str) {
        let mut access = self.access.lock().await;
        if access.as_ref().is_some_and(|a| a.access_token == rejected) {
            *access = None;
        }
    }

    pub async fn register(&self, request: &RegisterRequest) -> Result<RegisterResponse, Error> {
        decode(
            self.send(Method::POST, "v1/workers/register", Some(request))
                .await?,
        )
        .await
    }

    pub async fn queue(
        &self,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<List<QueueItem>, Error> {
        let mut path = format!("v1/queue?limit={}", limit.clamp(1, 500));
        if let Some(cursor) = cursor {
            path.push_str("&cursor=");
            path.push_str(&url_encode(cursor));
        }
        decode(self.send::<()>(Method::GET, &path, None).await?).await
    }

    pub async fn claim(&self, item_id: Option<&str>, wait_ms: u64) -> Result<Option<Lease>, Error> {
        let path = format!("v1/claims?wait_ms={}", wait_ms.min(30_000));
        let request = ClaimRequest {
            item_id: item_id.map(str::to_owned),
        };
        let response = self.send(Method::POST, &path, Some(&request)).await?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if response.status() != StatusCode::CREATED {
            return Err(Error::InvalidResponse);
        }
        // Retain the fence before decoding requirements. Unknown fields or enum
        // values must never become a weaker executable lease, nor strand a
        // successfully claimed item merely because its requirements are newer.
        let body: serde_json::Value = decode(response).await?;
        #[derive(serde::Deserialize)]
        struct Identity {
            id: String,
            epoch: u64,
        }
        let identity: Identity =
            serde_json::from_value(body["lease"].clone()).map_err(|_| Error::InvalidResponse)?;
        let lease = match serde_json::from_value::<ClaimResponse>(body) {
            Ok(response) => response.lease,
            Err(_) => {
                self.with_lease_epoch(&identity.id, identity.epoch)
                    .release(
                        &identity.id,
                        &ReleaseRequest::<serde_json::Value> {
                            reason: ReleaseReason::Ineligible,
                            message: Some("unsupported or invalid lease requirements".into()),
                            result: None,
                        },
                    )
                    .await?;
                return Err(Error::Conflict {
                    code: codes::INELIGIBLE.into(),
                    message: "unsupported or invalid lease requirements".into(),
                });
            }
        };
        self.lease_epochs
            .lock()
            .await
            .insert(lease.id.clone(), lease.epoch);
        Ok(Some(lease))
    }

    pub async fn claim_outcome(
        &self,
        item_id: Option<&str>,
        wait_ms: u64,
    ) -> Result<ClaimOutcome, Error> {
        Ok(match self.claim(item_id, wait_ms).await? {
            Some(lease) => ClaimOutcome::Leased(lease),
            None => ClaimOutcome::Empty,
        })
    }
    pub async fn heartbeat(
        &self,
        id: &str,
        request: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse, Error> {
        let id = path_id(id)?;
        decode(
            self.send(
                Method::POST,
                &format!("v1/leases/{id}/heartbeat"),
                Some(request),
            )
            .await?,
        )
        .await
    }

    pub async fn events<E: Serialize>(&self, id: &str, events: Vec<E>) -> Result<(), Error> {
        let id = path_id(id)?;
        self.send(
            Method::POST,
            &format!("v1/leases/{id}/events"),
            Some(&EventBatch { events }),
        )
        .await?;
        Ok(())
    }

    /// Like events(), exposing the newly accepted count when the server supplies
    /// it. An empty 2xx body acknowledges the batch and returns its length;
    /// this fallback cannot distinguish duplicates already stored by the server.
    pub async fn events_accepted<E: Serialize>(
        &self,
        id: &str,
        events: Vec<E>,
    ) -> Result<usize, Error> {
        let id = path_id(id)?;
        let count = events.len();
        let response = self
            .send(
                Method::POST,
                &format!("v1/leases/{id}/events"),
                Some(&EventBatch { events }),
            )
            .await?;
        let bytes = response.bytes().await?;
        if bytes.is_empty() {
            return Ok(count);
        }
        Ok(serde_json::from_slice::<EventsResponse>(&bytes)
            .map_err(|_| Error::InvalidResponse)?
            .accepted)
    }

    pub async fn result<R: Serialize>(
        &self,
        id: &str,
        result: &LeaseResult<R>,
    ) -> Result<(), Error> {
        let id = path_id(id)?;
        self.send(
            Method::POST,
            &format!("v1/leases/{id}/result"),
            Some(result),
        )
        .await?;
        Ok(())
    }

    pub async fn release<R: Serialize>(
        &self,
        id: &str,
        request: &ReleaseRequest<R>,
    ) -> Result<(), Error> {
        let id = path_id(id)?;
        self.send(
            Method::POST,
            &format!("v1/leases/{id}/release"),
            Some(request),
        )
        .await?;
        Ok(())
    }

    pub async fn credentials(&self, id: &str) -> Result<Credentials, Error> {
        let id = path_id(id)?;
        decode(
            self.send::<()>(Method::POST, &format!("v1/leases/{id}/credentials"), None)
                .await?,
        )
        .await
    }
}

pub(crate) fn path_id(id: &str) -> Result<&str, Error> {
    if id.is_empty()
        || matches!(id, "." | "..")
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(Error::InvalidUrl);
    }
    Ok(id)
}

fn url_encode(value: &str) -> String {
    // URL path segments and query values are both encoded to keep IDs opaque.
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn server_error(response: reqwest::Response) -> Error {
    let status = response.status();
    match response.json::<ApiError>().await {
        Ok(error) => response_error(status, error),
        Err(_) => Error::InvalidResponse,
    }
}

pub(crate) fn response_error(status: StatusCode, error: ApiError) -> Error {
    match error {
        error if status == StatusCode::CONFLICT => Error::Conflict {
            code: error.code,
            message: error.error,
        },
        error if error.code == codes::LINK_DENIED => Error::LinkDenied,
        error if error.code == codes::LINK_PENDING => Error::LinkPending,
        error => Error::Server {
            status,
            code: error.code,
            message: error.error,
        },
    }
}

async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, Error> {
    if !response.status().is_success() {
        return Err(server_error(response).await);
    }
    response.json().await.map_err(|_| Error::InvalidResponse)
}

impl Error {
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Server { code, .. } | Self::Conflict { code, .. } => Some(code),
            Self::LinkRevoked => Some(codes::UNAUTHORIZED),
            Self::LinkDenied => Some(codes::LINK_DENIED),
            Self::LinkPending => Some(codes::LINK_PENDING),
            _ => None,
        }
    }
    pub fn is_taken(&self) -> bool {
        self.code() == Some(codes::TAKEN)
    }
    pub fn is_lease_lost(&self) -> bool {
        self.code() == Some(codes::LEASE_LOST)
    }
    pub fn is_ineligible(&self) -> bool {
        self.code() == Some(codes::INELIGIBLE)
    }
}
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("tokens", &"[redacted]")
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server { status, code, .. } => f
                .debug_struct("Server")
                .field("status", status)
                .field("code", code)
                .field("message", &"[redacted]")
                .finish(),
            Self::Conflict { code, .. } => f
                .debug_struct("Conflict")
                .field("code", code)
                .field("message", &"[redacted]")
                .finish(),
            Self::Transport(_) => f.write_str("Transport([redacted])"),
            #[cfg(feature = "terminal")]
            Self::TerminalTransport(_) => f.write_str("TerminalTransport([redacted])"),
            _ => f.write_str(&self.to_string()),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_policy_refuses_remote_http_credentials_queries_and_fragments() {
        for url in [
            "http://example.com",
            "https://user:pass@example.com",
            "https://example.com?token=secret",
            "https://example.com#secret",
        ] {
            assert!(Client::new(url, String::new()).is_err(), "{url}");
        }
    }

    #[test]
    fn typed_response_failures_preserve_conflict_and_authorization_codes() {
        for (status, code) in [
            (StatusCode::CONFLICT, "lease_lost"),
            (StatusCode::CONFLICT, "taken"),
            (StatusCode::FORBIDDEN, "ineligible"),
            (StatusCode::UNAUTHORIZED, "unauthorized"),
        ] {
            let error = response_error(
                status,
                ApiError {
                    error: "private server detail".into(),
                    code: code.into(),
                },
            );
            assert_eq!(error.code(), Some(code));
            assert!(!format!("{error:?}").contains("private server detail"));
        }
    }
}
