//! Writing an agent's credential and secret files into a staged overlay.
//!
//! Split out of `engine/overlay/mod.rs` by WI 0114 F-51. A child module of
//! `overlay`, so it reaches `OverlayEngine`'s private items unchanged.

use super::*;
/// Write a single `AgentSecretFile` under the staged root, creating parent
/// directories. On Unix the file is opened with the requested mode so the
/// secret never lands on disk world-readable.
pub(crate) fn write_secret_file(
    staged_root: &Path,
    file: &crate::engine::auth::keychain::AgentSecretFile,
) -> std::io::Result<()> {
    let dest = staged_root.join(&file.relative_path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let mut handle = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(file.mode)
            .open(&dest)?;
        handle.write_all(&file.contents)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&dest, &file.contents)?;
    }
    Ok(())
}

/// Atomically replace a credential file in a staged settings directory.
///
/// The temporary file is deliberately created in `staged_root`: same-directory
/// `rename` is atomic and is visible through both Docker and Apple Containers'
/// existing RW bind mount. A missing staged root is a normal monitor race and
/// is reported as `Ok(false)`, never as a partial write to a recycled path.
pub fn write_credential_file_atomic(
    staged_root: &Path,
    file: &CredentialFile,
) -> std::io::Result<bool> {
    if !staged_root.is_dir() {
        return Ok(false);
    }
    if file.relative_path.is_absolute()
        || file
            .relative_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "credential path must be relative to staged root",
        ));
    }
    let target = staged_root.join(&file.relative_path);
    let Some(parent) = target.parent() else {
        return Ok(false);
    };
    if !parent.is_dir() {
        return Ok(false);
    }

    use std::io::Write as _;
    let mut temp = tempfile::NamedTempFile::new_in(staged_root)?;
    temp.write_all(&file.contents)?;
    temp.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(file.mode))?;
    }
    // `persist` is a same-filesystem rename. It replaces an existing target
    // without ever truncating that target in place.
    temp.persist(&target)
        .map_err(|error| error.error)
        .map(|_| true)
}

/// Derive the initial monitor fingerprint from the descriptor's materialized
/// Claude file without ever accepting a refresh-token field. This mirrors the
/// credential-model parser's allow-list shape.
pub(crate) fn credential_fingerprint_for_file(
    file: &CredentialFile,
) -> Result<CredentialFingerprint, EngineError> {
    #[derive(serde::Deserialize)]
    struct MaterializedClaudeCredential {
        #[serde(rename = "claudeAiOauth")]
        oauth: MaterializedClaudeOauth,
    }
    #[derive(serde::Deserialize)]
    struct MaterializedClaudeOauth {
        #[serde(rename = "accessToken")]
        access_token: String,
        #[serde(rename = "expiresAt", default)]
        expires_at: Option<u64>,
    }

    let parsed: MaterializedClaudeCredential =
        serde_json::from_slice(&file.contents).map_err(|_| {
            EngineError::Other(
                "refreshable credential materialization was not valid Claude JSON".into(),
            )
        })?;
    let expires_at = parsed
        .oauth
        .expires_at
        .map(|milliseconds| std::time::UNIX_EPOCH + std::time::Duration::from_millis(milliseconds));
    Ok(CredentialFingerprint::of(
        &crate::engine::auth::credential::CredentialSnapshot {
            secret: crate::engine::auth::credential::SecretString::new(parsed.oauth.access_token),
            expires_at,
            extra: Default::default(),
        },
    ))
}

/// Production binding for `AgentSecretFilesProvider`: reads file-form
/// keychain artifacts from the host OS keychain via
/// `engine::auth::keychain::agent_keychain_files`.
pub(crate) fn default_secret_files_provider() -> AgentSecretFilesProvider {
    std::sync::Arc::new(|agent: &AgentName| {
        crate::engine::auth::keychain::agent_keychain_files(agent)
    })
}

pub(crate) fn default_credential_provider(
    auth_resolver: AuthPathResolver,
) -> AgentCredentialFileProvider {
    std::sync::Arc::new(move |agent: &AgentName| {
        let spec = crate::engine::auth::keychain::refreshable_spec_for(agent)?;
        let source = (spec.source)(&auth_resolver);
        let snapshot = (spec.read)(&source).ok()?;
        Some((spec.materialize)(&snapshot))
    })
}
