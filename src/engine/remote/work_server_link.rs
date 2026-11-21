//! Work-server device transport. This module never reads browser grant storage.
use super::{HttpClientOptions, HttpCore};
use crate::data::config::{
    env::EnvSnapshot,
    fields::is_valid_map_name,
    file::{global_config_path, ConfigFile, FileKind},
};
use crate::data::fs::{
    private_files::{create_private_dir, write_private_json_atomic},
    DataPaths,
};
use crate::engine::{auth::device_flow::PollSchedule, error::EngineError};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A server refresh credential; deliberately unrelated to a browser grant.
#[derive(Serialize)]
#[serde(transparent)]
pub struct RefreshToken(String);
impl std::fmt::Debug for RefreshToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RefreshToken(<redacted>)")
    }
}
impl RefreshToken {
    pub fn new(value: String) -> Result<Self, EngineError> {
        if value.trim().is_empty()
            || value.starts_with("awb_")
            || value.chars().any(char::is_control)
        {
            return Err(EngineError::Auth(
                "invalid work-server refresh token".into(),
            ));
        }
        Ok(Self(value))
    }
}
#[derive(Deserialize)]
pub struct DeviceFlow {
    pub user_code: String,
    pub verification_uri: String,
    pub link_id: String,
    pub interval_ms: u64,
    #[serde(default = "default_expiry")]
    pub expires_in: u64,
}
fn default_expiry() -> u64 {
    300
}
pub struct DeviceFlowClient {
    url: reqwest::Url,
    http: reqwest::Client,
}
impl DeviceFlowClient {
    pub fn validate_url(url: &str) -> Result<reqwest::Url, EngineError> {
        let u =
            reqwest::Url::parse(url).map_err(|_| EngineError::Auth("invalid server URL".into()))?;
        if !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
            || u.host_str().is_none()
            || !(u.scheme() == "https"
                || (u.scheme() == "http"
                    && matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))))
        {
            return Err(EngineError::Auth(
                "server URL requires HTTPS (HTTP allowed only on loopback)".into(),
            ));
        }
        Ok(u)
    }
    pub fn new(url: &str) -> Result<Self, EngineError> {
        Ok(Self {
            url: Self::validate_url(url)?,
            http: HttpCore::client(&HttpClientOptions {
                refuse_redirects: true,
                ..HttpClientOptions::daemon()
            })?,
        })
    }
    /// Best-effort OS browser launch, without a shell or inherited daemon env.
    pub fn open_verification(env: &EnvSnapshot, uri: &str) {
        if env.test_isolation() || Self::validate_url(uri).is_err() {
            return;
        }
        #[cfg(target_os = "macos")]
        let name = "open";
        #[cfg(not(target_os = "macos"))]
        let name = "xdg-open";
        let mut command = crate::engine::host_cli::command(env, name);
        if let Ok(mut child) = command
            .arg(uri)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
    pub fn default_name(&self) -> String {
        let name: String = self
            .url
            .host_str()
            .unwrap_or("server")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .take(63)
            .collect();
        if is_valid_map_name(&name) {
            name
        } else {
            "server".into()
        }
    }
    async fn post(
        &self,
        suffix: &str,
    ) -> Result<(reqwest::StatusCode, serde_json::Value), EngineError> {
        let url = format!("{}/v1/{suffix}", self.url.as_str().trim_end_matches('/'));
        HttpCore::refuse_public_host_under_test_isolation(&url).map_err(EngineError::Auth)?;
        let r = self
            .http
            .post(url)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| EngineError::Auth(format!("work-server link request failed: {e}")))?;
        let status = r.status();
        if status.is_redirection() {
            return Err(EngineError::Auth(
                "work-server link redirects are not allowed".into(),
            ));
        }
        let v: serde_json::Value = r
            .json()
            .await
            .map_err(|_| EngineError::Auth("invalid work-server response".into()))?;
        Ok((status, v))
    }
    pub async fn begin(&self) -> Result<DeviceFlow, EngineError> {
        let (status, response) = self.post("link").await?;
        if !status.is_success() || response.get("error").is_some() {
            return Err(EngineError::Auth(format!("work-server returned {status}")));
        }
        serde_json::from_value(response)
            .map_err(|_| EngineError::Auth("invalid device-flow response".into()))
    }
    pub async fn finish(&self, flow: DeviceFlow) -> Result<RefreshToken, EngineError> {
        if flow.link_id.is_empty()
            || !flow
                .link_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        {
            return Err(EngineError::Auth("invalid link id".into()));
        }
        let lifetime = Duration::from_secs(flow.expires_in.min(3600));
        let mut schedule = PollSchedule::new(
            lifetime,
            Duration::from_millis(flow.interval_ms.min(60_000)),
        );
        // Bound the entire flow, including sleeps and stalled HTTP bodies.
        // The generic daemon HTTP timeout can exceed this authorization TTL.
        tokio::time::timeout(lifetime, async {
            loop {
                tokio::time::sleep(schedule.interval()).await;
                if schedule.expired() {
                    return Err(EngineError::Auth("work-server link expired".into()));
                }
                let (status, response) = self.post(&format!("link/{}/token", flow.link_id)).await?;
                if schedule.expired() {
                    return Err(EngineError::Auth("work-server link expired".into()));
                }
                if let Some(token) = response["refresh_token"].as_str() {
                    if !status.is_success() || response.get("error").is_some() {
                        return Err(EngineError::Auth("work-server link failed".into()));
                    }
                    return RefreshToken::new(token.into());
                }
                // Device-flow pending/slow_down responses may use 400 or 429;
                // transport/server errors must not be mistaken for those states.
                if !status.is_success()
                    && status != reqwest::StatusCode::BAD_REQUEST
                    && status != reqwest::StatusCode::TOO_MANY_REQUESTS
                {
                    return Err(EngineError::Auth(format!("work-server returned {status}")));
                }
                match response["error"].as_str().or(response["status"].as_str()) {
                    Some("authorization_pending" | "pending") => {}
                    Some("slow_down") => schedule.slow_down(),
                    _ => {
                        return Err(EngineError::Auth(
                            "work-server link denied or expired".into(),
                        ))
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| Err(EngineError::Auth("work-server link expired".into())))
    }
    pub fn store(
        &self,
        env: &EnvSnapshot,
        name: &str,
        labels: &[String],
        token: RefreshToken,
    ) -> Result<(), EngineError> {
        if !is_valid_map_name(name) {
            return Err(EngineError::Auth("invalid server name".into()));
        }
        let mut label_map = std::collections::BTreeMap::new();
        for label in labels {
            let (k, v) = label
                .split_once('=')
                .filter(|(k, _)| !k.is_empty())
                .ok_or_else(|| EngineError::Auth("labels must be k=v".into()))?;
            label_map.insert(k.to_string(), v.to_string());
        }
        let path = global_config_path(env)?;
        let mut config = ConfigFile::load(&path, FileKind::Global)?;
        let entry = config
            .work_servers
            .get_or_insert_with(Default::default)
            .entry(name.into())
            .or_default();
        entry.url = Some(self.url.as_str().trim_end_matches('/').into());
        entry.enabled = Some(true);
        entry.labels = Some(label_map);
        config.validate_semantics()?;
        let dir = DataPaths::from_env(env)?.root().join("work_servers");
        create_private_dir(&dir)?;
        write_private_json_atomic(
            &dir.join(format!("{name}.json")),
            &serde_json::json!({"url":self.url.as_str().trim_end_matches('/'),"refresh_token":token,"linked_at":chrono::Utc::now()}),
            || Ok(()),
        )?;
        config.save(&path, FileKind::Global)?;
        Ok(())
    }
}
