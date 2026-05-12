//! Download + extract the canonical aspec/ tarball from GitHub.

use std::io::Write;
use std::path::Path;

use thiserror::Error;

/// URL for downloading the aspec repo tarball.
pub const ASPEC_TARBALL_URL: &str =
    "https://api.github.com/repos/prettysmartdev/aspec/tarball/main";

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("network download failed: {0}")]
    DownloadFailed(String),
    #[error("tarball extraction failed: {0}")]
    ExtractFailed(String),
}

/// Download the aspec tarball into memory.
pub async fn download_aspec_tarball() -> Result<Vec<u8>, NetworkError> {
    let client = reqwest::Client::builder()
        .user_agent("amux")
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| NetworkError::DownloadFailed(format!("client init: {e}")))?;
    let resp = client
        .get(ASPEC_TARBALL_URL)
        .send()
        .await
        .map_err(|e| NetworkError::DownloadFailed(format!("GET {ASPEC_TARBALL_URL}: {e}")))?;
    if !resp.status().is_success() {
        return Err(NetworkError::DownloadFailed(format!(
            "HTTP {} when downloading aspec tarball",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| NetworkError::DownloadFailed(format!("read body: {e}")))?;
    Ok(bytes.to_vec())
}

/// Extract the `aspec/` directory from a gzipped tarball into `dest`.
///
/// The tarball from GitHub has a top-level directory like
/// `prettysmartdev-aspec-<sha>/`. Look for entries under `<top>/aspec/` and
/// strip that prefix.
pub fn extract_aspec_tarball(tarball_bytes: &[u8], dest: &Path) -> Result<(), NetworkError> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let decoder = GzDecoder::new(tarball_bytes);
    let mut archive = Archive::new(decoder);
    let mut extracted = 0u64;

    let entries = archive
        .entries()
        .map_err(|e| NetworkError::ExtractFailed(format!("read entries: {e}")))?;

    for entry in entries {
        let mut entry =
            entry.map_err(|e| NetworkError::ExtractFailed(format!("read entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| NetworkError::ExtractFailed(format!("read entry path: {e}")))?
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
                NetworkError::ExtractFailed(format!("mkdir {}: {e}", dest.display()))
            })?;
            continue;
        }
        let target = dest.join(&relative);
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| {
                NetworkError::ExtractFailed(format!("mkdir {}: {e}", target.display()))
            })?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    NetworkError::ExtractFailed(format!("mkdir {}: {e}", parent.display()))
                })?;
            }
            entry.unpack(&target).map_err(|e| {
                NetworkError::ExtractFailed(format!("unpack {}: {e}", target.display()))
            })?;
            extracted += 1;
        }
    }
    if extracted == 0 {
        return Err(NetworkError::ExtractFailed(
            "no aspec/ files found in tarball".into(),
        ));
    }
    let _ = std::io::stderr().flush();
    Ok(())
}
