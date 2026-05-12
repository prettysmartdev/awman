//! Per-platform keychain credential resolution.
//!
//! macOS uses `security find-generic-password`. Linux/Windows return no
//! credentials (agents authenticate via env vars in `envPassthrough`).

use std::process::Command;

use crate::data::session::AgentName;

/// Look up host-keychain credentials for the agent.
///
/// Returns the `(env_key, value)` pairs that should be injected into the
/// agent container at launch. Empty when no credentials are configured —
/// this is not an error.
pub fn agent_keychain_credentials(agent: &AgentName) -> Vec<(String, String)> {
    if cfg!(target_os = "macos") {
        match agent.as_str() {
            "claude" => claude_keychain_credentials(),
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    }
}

/// macOS-only: look up the Claude Code OAuth credential and extract its
/// access token via the JSON path `claudeAiOauth.accessToken`.
fn claude_keychain_credentials() -> Vec<(String, String)> {
    let out = match Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return Vec::new();
    }
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let token = parsed
        .get("claudeAiOauth")
        .and_then(|v| v.get("accessToken"))
        .and_then(|v| v.as_str());
    match token {
        Some(t) => vec![("CLAUDE_CODE_OAUTH_TOKEN".to_string(), t.to_string())],
        None => Vec::new(),
    }
}
