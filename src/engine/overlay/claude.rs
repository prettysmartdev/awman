//! Claude's settings tree: what may be copied, what must be stripped, and
//! what a synthesised minimal tree looks like.
//!
//! Split out of `engine/overlay/mod.rs` by WI 0114 F-51. A child module of
//! `overlay`, so it reaches `OverlayEngine`'s private items unchanged.

use super::*;
/// Strip `oauthAccount` from `~/.claude.json`, inject
/// `projects["/workspace"]["hasTrustDialogAccepted"] = true` to suppress the
/// in-container trust dialog, and write the result to a `TempDir` whose
/// lifetime is owned by the caller. The sanitized path is `<tempdir>/claude.json`.
pub(crate) fn sanitize_claude_config(
    src: &Path,
) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let raw = std::fs::read_to_string(src)?;
    let mut value: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(obj) = &mut value {
        obj.remove("oauthAccount");

        // Mark `/workspace` as a trusted project so Claude does not prompt for
        // trust inside the container. Mirrors legacy
        // `oldsrc/runtime/mod.rs::sanitize_claude_config`.
        let projects = obj
            .entry("projects".to_string())
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if let serde_json::Value::Object(p) = projects {
            let project = p
                .entry("/workspace".to_string())
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
            if let serde_json::Value::Object(pobj) = project {
                pobj.insert(
                    "hasTrustDialogAccepted".into(),
                    serde_json::Value::Bool(true),
                );
            }
        }
    }

    let tmp_dir = tempfile::Builder::new().prefix("awman-claude-").tempdir()?;
    let dest = tmp_dir.path().join("claude.json");
    let body = serde_json::to_string_pretty(&value).unwrap_or(raw);
    std::fs::write(&dest, body)?;
    Ok((tmp_dir, dest))
}

/// Sanitize `~/.claude/`: filter out denylisted entries, optionally inject
/// the yolo-mode settings file, and suppress the LSP recommendation banner.
/// Returns the `TempDir` (cleaned on drop) and its path.
pub(crate) fn sanitize_claude_settings_dir(
    src: &Path,
    yolo: bool,
) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let tmp = tempfile::Builder::new()
        .prefix("awman-claude-dir-")
        .tempdir()?;
    let tmp_root = tmp.path().to_path_buf();
    // Mirror only the entries that are not on the denylist. The credential
    // guard is applied fail-closed: the top-level noise denylist is exact, but
    // the credential name is matched case-insensitively, symlinks and other
    // non-regular entries are never copied, and any file sharing the host
    // credential's inode identity is skipped at every depth (INV-2, BLOCKING-1).
    let denylist: std::collections::HashSet<&str> = CLAUDE_DENYLIST.iter().copied().collect();
    let host_credential = file_identity(&src.join(".credentials.json"));
    if let Ok(entries) = std::fs::read_dir(src) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if denylist.contains(name_str.as_ref()) || is_denied_credential_name(&name_str) {
                continue;
            }
            let src_path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&src_path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                // A symlink in ~/.claude being mounted RW would let the copy
                // follow an alias to the host credential (or any host path).
                continue;
            }
            let dest = tmp_root.join(&name);
            if meta.is_dir() {
                copy_claude_tree_secure(&src_path, &dest, &host_credential)?;
            } else if meta.is_file() {
                if file_identity(&src_path).is_some() && file_identity(&src_path) == host_credential
                {
                    continue;
                }
                std::fs::copy(&src_path, dest)?;
            }
            // Any other file type (fifo, socket, device) is never mounted.
        }
    }
    // Inject (or update) settings.json to suppress LSP banner and optionally
    // grant yolo bypass-permissions.
    let settings_path = tmp_root.join("settings.json");
    let mut settings: serde_json::Value = if settings_path.exists() {
        std::fs::read_to_string(&settings_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if let serde_json::Value::Object(obj) = &mut settings {
        // Set both LSP suppression keys for compatibility with different
        // Claude Code versions.
        obj.insert(
            "hasShownLspRecommendation".into(),
            serde_json::Value::Bool(true),
        );
        obj.insert(
            "lspRecommendationDismissed".into(),
            serde_json::Value::Bool(true),
        );
        if yolo {
            obj.insert(
                "skipDangerousModePermissionPrompt".into(),
                serde_json::Value::Bool(true),
            );
            obj.insert(
                "permissionMode".into(),
                serde_json::Value::String("bypassPermissions".into()),
            );
        }
    }
    let body = serde_json::to_string_pretty(&settings).unwrap_or_default();
    let _ = std::fs::write(&settings_path, body);
    Ok((tmp, tmp_root))
}

/// Synthesize a minimal `.claude.json` for first-time users: trust dialog
/// accepted for `/workspace`, no oauthAccount.
pub(crate) fn synthesize_minimal_claude_config(
) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let value = serde_json::json!({
        "projects": {
            "/workspace": {
                "hasTrustDialogAccepted": true
            }
        }
    });
    let tmp_dir = tempfile::Builder::new()
        .prefix("awman-claude-minimal-")
        .tempdir()?;
    let dest = tmp_dir.path().join("claude.json");
    let body = serde_json::to_string_pretty(&value).unwrap_or_default();
    std::fs::write(&dest, body)?;
    Ok((tmp_dir, dest))
}

/// Synthesize a minimal `~/.claude/` directory for first-time users with
/// LSP suppression and (optionally) yolo bypass.
pub(crate) fn synthesize_minimal_claude_settings_dir(
    yolo: bool,
) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let tmp = tempfile::Builder::new()
        .prefix("awman-claude-dir-minimal-")
        .tempdir()?;
    let tmp_root = tmp.path().to_path_buf();
    let mut settings = serde_json::json!({});
    if let serde_json::Value::Object(obj) = &mut settings {
        obj.insert(
            "hasShownLspRecommendation".into(),
            serde_json::Value::Bool(true),
        );
        obj.insert(
            "lspRecommendationDismissed".into(),
            serde_json::Value::Bool(true),
        );
        if yolo {
            obj.insert(
                "skipDangerousModePermissionPrompt".into(),
                serde_json::Value::Bool(true),
            );
            obj.insert(
                "permissionMode".into(),
                serde_json::Value::String("bypassPermissions".into()),
            );
        }
    }
    let body = serde_json::to_string_pretty(&settings).unwrap_or_default();
    std::fs::write(tmp_root.join("settings.json"), body)?;
    Ok((tmp, tmp_root))
}

/// Copy a host settings dir into a `TempDir` snapshot, then write each
/// `AgentSecretFile` into the staged tree (creating parent dirs as needed).
///
/// Reusable across any agent whose container expects an on-disk credential
/// file inside its settings dir. Currently used by antigravity to seed
/// `antigravity-cli/antigravity-oauth-token` alongside the host's `~/.gemini`
/// snapshot; structured so future agents (e.g. ones that store tokens in
/// libsecret on Linux) can drop straight in.
pub(crate) fn stage_settings_dir_with_secrets(
    src: &Path,
    secret_files: &[crate::engine::auth::keychain::AgentSecretFile],
    tmpdir_prefix: &str,
) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let tmp = tempfile::Builder::new().prefix(tmpdir_prefix).tempdir()?;
    let tmp_root = tmp.path().to_path_buf();
    copy_dir_all(src, &tmp_root)?;
    for f in secret_files {
        write_secret_file(&tmp_root, f)?;
    }
    Ok((tmp, tmp_root))
}

/// Build a fresh empty settings dir and plant the given secret files into it.
/// Used for first-time-user paths where the host has no settings dir on disk
/// but the agent's keychain entry is sufficient on its own.
pub(crate) fn synthesize_settings_dir_with_secrets(
    secret_files: &[crate::engine::auth::keychain::AgentSecretFile],
    tmpdir_prefix: &str,
) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let tmp = tempfile::Builder::new().prefix(tmpdir_prefix).tempdir()?;
    let tmp_root = tmp.path().to_path_buf();
    for f in secret_files {
        write_secret_file(&tmp_root, f)?;
    }
    Ok((tmp, tmp_root))
}
