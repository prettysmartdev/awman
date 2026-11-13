//! Kit emission — renders a per-agent `spec.yaml` plus the bundled
//! `apply-session-config.sh` startup script into a kit directory.
//!
//! Called from the Layer 1 ready flow (`awman ready` for the sbx runtime).
//! Credential **values** are never written into kit files — only credential
//! mappings/config, which already live in the embedded templates.

use std::path::Path;

use crate::data::templates::{sbx_apply_script_for, sbx_kit_template_for};
use crate::engine::error::EngineError;

/// Placeholder substituted with awman's version when rendering kit templates.
const AWMAN_VERSION_PLACEHOLDER: &str = "{{AWMAN_VERSION}}";

/// Renders kit specs and startup scripts for sbx agents.
pub(super) struct DSbxKitEmitter {
    awman_version: String,
}

impl Default for DSbxKitEmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl DSbxKitEmitter {
    pub(super) fn new() -> Self {
        Self {
            awman_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Render the kit for `agent` into `dest` (the kit directory):
    /// - `<dest>/spec.yaml`
    /// - `<dest>/files/home/.awman/apply-session-config.sh` (mode 0755)
    pub(super) fn emit_for_agent(&self, agent: &str, dest: &Path) -> Result<(), EngineError> {
        let template = sbx_kit_template_for(agent).ok_or_else(|| {
            EngineError::Sandbox(format!("no sbx kit template bundled for agent '{agent}'"))
        })?;
        let script = sbx_apply_script_for(agent).ok_or_else(|| {
            EngineError::Sandbox(format!("no sbx startup script bundled for agent '{agent}'"))
        })?;

        // 1. Render and write spec.yaml.
        let rendered = template.replace(AWMAN_VERSION_PLACEHOLDER, &self.awman_version);
        std::fs::create_dir_all(dest).map_err(|e| EngineError::io(dest, e))?;
        let spec_path = dest.join("spec.yaml");
        std::fs::write(&spec_path, rendered).map_err(|e| EngineError::io(&spec_path, e))?;

        // 2. Write the per-launch startup script into files/home/.awman/.
        let script_dir = dest.join("files").join("home").join(".awman");
        std::fs::create_dir_all(&script_dir).map_err(|e| EngineError::io(&script_dir, e))?;
        let script_path = script_dir.join("apply-session-config.sh");
        std::fs::write(&script_path, script).map_err(|e| EngineError::io(&script_path, e))?;
        set_executable(&script_path)?;

        Ok(())
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), EngineError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| EngineError::io(path, e))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|e| EngineError::io(path, e))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), EngineError> {
    // No executable bit on Windows; the kit's `commands.startup` invokes the
    // script via `bash`, so the mode is irrelevant there.
    Ok(())
}
