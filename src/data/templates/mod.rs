//! Layer 0 template inclusions — embedded Dockerfile templates and audit
//! prompts. These exist as a single Layer-0 module so callers everywhere
//! can resolve the templates without having to redefine paths.

pub mod audit_prompts;

pub use audit_prompts::{init_audit_prompt, ready_audit_prompt};

/// The project base Dockerfile. Written by `amux init` and `amux ready`
/// when no `Dockerfile.dev` exists at the git root.
pub fn project_dockerfile_dev() -> &'static str {
    include_str!("../../../templates/Dockerfile.project")
}

/// Per-agent Dockerfile template (fallback when network download fails).
pub fn agent_dockerfile_for(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => include_str!("../../../templates/Dockerfile.claude"),
        "codex" => include_str!("../../../templates/Dockerfile.codex"),
        "opencode" => include_str!("../../../templates/Dockerfile.opencode"),
        "maki" => include_str!("../../../templates/Dockerfile.maki"),
        "gemini" => include_str!("../../../templates/Dockerfile.gemini"),
        "copilot" => include_str!("../../../templates/Dockerfile.copilot"),
        "crush" => include_str!("../../../templates/Dockerfile.crush"),
        "cline" => include_str!("../../../templates/Dockerfile.cline"),
        _ => return None,
    })
}

/// Returns `true` when the given content matches the bundled project base
/// template (ignoring leading/trailing whitespace). Used by the ready engine
/// to decide whether an audit should be offered.
pub fn dockerfile_matches_template(content: &str) -> bool {
    content.trim() == project_dockerfile_dev().trim()
}
