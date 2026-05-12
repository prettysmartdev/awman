//! Per-agent Dockerfile download helper.
//!
//! Downloads `Dockerfile.<agent>` from the canonical GitHub raw URL into
//! `<git_root>/.amux/Dockerfile.<agent>`. Falls back to the bundled template
//! at `src/data/templates/Dockerfile.<agent>` when the network is unavailable
//! and a bundled copy is shipped in the binary.

use std::path::Path;

use crate::engine::error::EngineError;

/// GitHub raw URL prefix for amux-shipped Dockerfiles.
pub const DOCKERFILE_RAW_URL_PREFIX: &str =
    "https://raw.githubusercontent.com/prettysmartdev/amux/main/templates";

/// Construct the canonical raw URL for an agent Dockerfile.
pub fn dockerfile_url_for(agent: &str) -> String {
    format!("{DOCKERFILE_RAW_URL_PREFIX}/Dockerfile.{agent}")
}

/// Write `body` to `dest` atomically (tmp file + rename) so a partial failure
/// cannot leave a corrupt file behind.
fn atomic_write(dest: &Path, body: &[u8]) -> Result<(), EngineError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| EngineError::io(parent.to_path_buf(), e))?;
    }
    let tmp = dest.with_extension("tmp");
    std::fs::write(&tmp, body).map_err(|e| EngineError::io(tmp.clone(), e))?;
    std::fs::rename(&tmp, dest).map_err(|e| EngineError::io(dest.to_path_buf(), e))?;
    Ok(())
}

/// Download an agent Dockerfile to `dest`. On network failure, falls back to
/// the bundled template baked into the binary (when one exists for this
/// agent). Returns `EngineError::AgentDockerfileDownloadFailed` only when no
/// bundled fallback is available.
///
/// `project_base_tag` is substituted for `{{AMUX_BASE_IMAGE}}` in the
/// downloaded (or bundled) Dockerfile content.
pub async fn download_agent_dockerfile(
    agent: &str,
    dest: &Path,
    project_base_tag: &str,
) -> Result<(), EngineError> {
    let url = dockerfile_url_for(agent);
    let client_result = reqwest::Client::builder()
        .user_agent("amux")
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build();

    let download_attempt: Result<Vec<u8>, String> = match client_result {
        Err(e) => Err(format!("client init: {e}")),
        Ok(client) => match client.get(&url).send().await {
            Err(e) => Err(format!("GET {url}: {e}")),
            Ok(resp) => {
                if !resp.status().is_success() {
                    Err(format!("HTTP {} when downloading {}", resp.status(), url))
                } else {
                    resp.bytes()
                        .await
                        .map(|b| b.to_vec())
                        .map_err(|e| format!("read body for {url}: {e}"))
                }
            }
        },
    };

    match download_attempt {
        Ok(body) => {
            let body_str = String::from_utf8_lossy(&body);
            let substituted = body_str.replace("{{AMUX_BASE_IMAGE}}", project_base_tag);
            atomic_write(dest, substituted.as_bytes())
        }
        Err(network_error) => {
            // Fall back to bundled template when one exists.
            if let Some(bundled) = crate::data::templates::agent_dockerfile_for(agent) {
                let substituted = bundled.replace("{{AMUX_BASE_IMAGE}}", project_base_tag);
                atomic_write(dest, substituted.as_bytes())
            } else {
                Err(EngineError::AgentDockerfileDownloadFailed {
                    agent: agent.to_string(),
                    message: network_error,
                })
            }
        }
    }
}
