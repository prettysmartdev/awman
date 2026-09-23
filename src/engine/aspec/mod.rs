//! `engine::aspec` — fetching the canonical `aspec/` template.
//!
//! Layer 1 (WI 0114 F-29, decision Q1). The download was a Layer 0 "network
//! helper", which is the one thing `src/data/` may not do. The URL itself is
//! still a Layer 0 constant — it is a fixed external address, not a decision
//! this engine makes.

use std::io::Write;
use std::path::Path;

use thiserror::Error;

use crate::data::network::ASPEC_TARBALL_URL;
use crate::engine::remote::{HttpClientOptions, HttpCore};

#[derive(Debug, Error)]
pub enum AspecError {
    #[error("network download failed: {0}")]
    DownloadFailed(String),
    #[error("tarball extraction failed: {0}")]
    ExtractFailed(String),
}

/// Downloads and unpacks the `aspec/` template tarball.
///
/// Constructed over a URL so a test (or a fork) can point it somewhere else;
/// [`AspecDownloader::canonical`] uses the shipped [`ASPEC_TARBALL_URL`].
pub struct AspecDownloader {
    url: String,
}

impl AspecDownloader {
    /// Time allowed to establish the connection to the tarball host.
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    /// Time allowed for the whole download.
    const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }

    /// The downloader for the canonical aspec repository tarball.
    pub fn canonical() -> Self {
        Self::new(ASPEC_TARBALL_URL)
    }

    /// The URL this downloader fetches from.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Download the tarball into memory.
    pub async fn download(&self) -> Result<Vec<u8>, AspecError> {
        let client = HttpCore::client(
            &HttpClientOptions::default()
                .with_timeouts(Self::CONNECT_TIMEOUT, Self::READ_TIMEOUT)
                .with_user_agent("awman"),
        )
        .map_err(|e| AspecError::DownloadFailed(format!("client init: {e}")))?;
        let url = &self.url;
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| AspecError::DownloadFailed(format!("GET {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(AspecError::DownloadFailed(format!(
                "HTTP {} when downloading aspec tarball",
                resp.status()
            )));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AspecError::DownloadFailed(format!("read body: {e}")))?;
        Ok(bytes.to_vec())
    }

    /// Extract the `aspec/` directory from a gzipped tarball into `dest`.
    ///
    /// The tarball from GitHub has a top-level directory like
    /// `prettysmartdev-aspec-<sha>/`. Look for entries under `<top>/aspec/` and
    /// strip that prefix.
    pub fn extract(tarball_bytes: &[u8], dest: &Path) -> Result<(), AspecError> {
        use flate2::read::GzDecoder;
        use tar::Archive;

        let decoder = GzDecoder::new(tarball_bytes);
        let mut archive = Archive::new(decoder);
        let mut extracted = 0u64;

        let entries = archive
            .entries()
            .map_err(|e| AspecError::ExtractFailed(format!("read entries: {e}")))?;

        for entry in entries {
            let mut entry =
                entry.map_err(|e| AspecError::ExtractFailed(format!("read entry: {e}")))?;
            let path = entry
                .path()
                .map_err(|e| AspecError::ExtractFailed(format!("read entry path: {e}")))?
                .into_owned();
            let path_str = path.to_string_lossy().to_string();
            let components: Vec<&str> = path_str.split('/').collect();
            if components.len() < 2 {
                continue;
            }
            if components[1] != "aspec" {
                continue;
            }
            let relative: String = components[2..].join("/");
            if relative.is_empty() {
                std::fs::create_dir_all(dest).map_err(|e| {
                    AspecError::ExtractFailed(format!("mkdir {}: {e}", dest.display()))
                })?;
                continue;
            }
            let target = dest.join(&relative);
            if entry.header().entry_type().is_dir() {
                std::fs::create_dir_all(&target).map_err(|e| {
                    AspecError::ExtractFailed(format!("mkdir {}: {e}", target.display()))
                })?;
            } else {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        AspecError::ExtractFailed(format!("mkdir {}: {e}", parent.display()))
                    })?;
                }
                entry.unpack(&target).map_err(|e| {
                    AspecError::ExtractFailed(format!("unpack {}: {e}", target.display()))
                })?;
                extracted += 1;
            }
        }
        if extracted == 0 {
            return Err(AspecError::ExtractFailed(
                "no aspec/ files found in tarball".into(),
            ));
        }
        let _ = std::io::stderr().flush();
        Ok(())
    }
}
