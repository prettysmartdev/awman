//! Layer 0 template inclusions — embedded Dockerfile templates and audit
//! prompts. These exist as a single Layer-0 module so callers everywhere
//! can resolve the templates without having to redefine paths.

pub mod audit_prompts;

pub use audit_prompts::{init_audit_prompt, ready_audit_prompt};

/// The project base Dockerfile. Written by `awman init` and `awman ready`
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
        "gemini" => include_str!("../../../templates/Dockerfile.gemini"),
        "copilot" => include_str!("../../../templates/Dockerfile.copilot"),
        "cline" => include_str!("../../../templates/Dockerfile.cline"),
        "antigravity" => include_str!("../../../templates/Dockerfile.antigravity"),
        _ => return None,
    })
}

/// Returns `true` when the given content matches the bundled project base
/// template (ignoring leading/trailing whitespace). Used by the ready engine
/// to decide whether an audit should be offered.
pub fn dockerfile_matches_template(content: &str) -> bool {
    content.trim() == project_dockerfile_dev().trim()
}

/// Per-agent Docker Sandbox kit YAML template (`spec.yaml` source).
///
/// Additive sibling of [`agent_dockerfile_for`] — the sbx kit templates live
/// alongside the Dockerfile templates in `templates/` and are embedded with
/// the same `include_str!` mechanism. A caller that wants a Dockerfile calls
/// `agent_dockerfile_for`; a caller that wants a kit calls this. The two
/// families never collide because of the `sbx-kit.` filename prefix.
pub fn sbx_kit_template_for(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => include_str!("../../../templates/sbx-kit.claude.yaml"),
        "codex" => include_str!("../../../templates/sbx-kit.codex.yaml"),
        "opencode" => include_str!("../../../templates/sbx-kit.opencode.yaml"),
        "gemini" => include_str!("../../../templates/sbx-kit.gemini.yaml"),
        "copilot" => include_str!("../../../templates/sbx-kit.copilot.yaml"),
        "cline" => include_str!("../../../templates/sbx-kit.cline.yaml"),
        "antigravity" => include_str!("../../../templates/sbx-kit.antigravity.yaml"),
        _ => return None,
    })
}

/// Per-agent Docker Sandbox startup script (`apply-session-config.sh` source).
///
/// Written into the emitted kit's `files/home/.awman/` directory by the kit
/// emitter at `awman ready` time and re-run by the kit's `commands.startup`
/// on every sandbox restart.
pub fn sbx_apply_script_for(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => include_str!("../../../templates/sbx-apply.claude.sh"),
        "codex" => include_str!("../../../templates/sbx-apply.codex.sh"),
        "opencode" => include_str!("../../../templates/sbx-apply.opencode.sh"),
        "gemini" => include_str!("../../../templates/sbx-apply.gemini.sh"),
        "copilot" => include_str!("../../../templates/sbx-apply.copilot.sh"),
        "cline" => include_str!("../../../templates/sbx-apply.cline.sh"),
        "antigravity" => include_str!("../../../templates/sbx-apply.antigravity.sh"),
        _ => return None,
    })
}
