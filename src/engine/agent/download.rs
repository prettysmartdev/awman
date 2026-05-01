//! Per-agent Dockerfile download helper.
//!
//! Downloads `Dockerfile.<agent>` from the canonical GitHub raw URL into
//! `<git_root>/.amux/Dockerfile.<agent>`. Real wiring (network calls,
//! progress reporting, retries) lands in 0070; this module captures the URL
//! map so the rest of the engine can target it.

use std::path::Path;

use crate::engine::error::EngineError;

/// GitHub raw URL prefix for amux-shipped Dockerfiles.
pub const DOCKERFILE_RAW_URL_PREFIX: &str =
    "https://raw.githubusercontent.com/qwibitai/amux/main/.amux";

/// Construct the canonical raw URL for an agent Dockerfile.
pub fn dockerfile_url_for(agent: &str) -> String {
    format!("{DOCKERFILE_RAW_URL_PREFIX}/Dockerfile.{agent}")
}

/// Download an agent Dockerfile to `dest`. Real network wiring lands in 0070;
/// for now this returns `EngineError::NotImplemented` if invoked.
pub async fn download_agent_dockerfile(_agent: &str, _dest: &Path) -> Result<(), EngineError> {
    Err(EngineError::NotImplemented(
        "download_agent_dockerfile lands with full network wiring in a later WI",
    ))
}
