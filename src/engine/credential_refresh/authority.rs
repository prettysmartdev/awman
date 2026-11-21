//! Cross-process source serialization, shared across data homes.
#[cfg(unix)]
use crate::data::fs::private_files::{create_private_dir, lock_private_timeout, LockMode};
use crate::engine::auth::credential::HostCredentialSource;
use sha2::{Digest, Sha256};
use std::{fs::File, time::Duration};
pub fn source_key(source: &HostCredentialSource, provider: &str) -> String {
    #[cfg(unix)]
    let owner = nix::unistd::geteuid().as_raw().to_string();
    #[cfg(not(unix))]
    let owner = String::from("owner");
    let kind = match source {
        HostCredentialSource::HostFile { .. } => "file",
        HostCredentialSource::MacosKeychain { .. } => "keychain",
    };
    let locator = match source {
        HostCredentialSource::HostFile { path } => {
            stable_locator(path).to_string_lossy().into_owned()
        }
        HostCredentialSource::MacosKeychain { service, account } => format!(
            "{}:{service}{}:{}",
            service.len(),
            account.unwrap_or_default().len(),
            account.unwrap_or_default()
        ),
    };
    let mut hash = Sha256::new();
    for part in [owner.as_str(), kind, provider, locator.as_str()] {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
// Resolve existing ancestors, never the credential leaf. Atomic file removal
// must not change the serialization identity, even before the first login.
fn stable_locator(path: &std::path::Path) -> std::path::PathBuf {
    // Source selection resolves paths against its explicit environment.
    let absolute = path.to_path_buf();
    let mut parent = absolute.parent().unwrap_or(&absolute).to_path_buf();
    let mut suffix = vec![absolute.file_name().unwrap_or_default().to_os_string()];
    loop {
        if let Ok(canonical) = parent.canonicalize() {
            return suffix
                .into_iter()
                .rev()
                .fold(canonical, |p, part| p.join(part));
        }
        let Some(name) = parent.file_name() else {
            return absolute;
        };
        suffix.push(name.to_os_string());
        if !parent.pop() {
            return absolute;
        }
    }
}
/// Prefer the user's private runtime/temp directory; otherwise use the explicit
/// home. Never reserve a victim's predictable name in a shared /tmp directory.
pub fn lock_root(
    env: &crate::data::config::env::EnvSnapshot,
    home: &std::path::Path,
) -> std::path::PathBuf {
    #[cfg(not(unix))]
    let _ = env;
    #[cfg(unix)]
    for name in ["XDG_RUNTIME_DIR", "TMPDIR"] {
        if let Some(path) = env.get(name).map(std::path::PathBuf::from) {
            if create_private_dir(&path).is_ok() {
                return path.join("awman-credential-authorities");
            }
        }
    }
    home.join(".cache/awman-credential-authorities")
}
pub async fn lock_source_at(
    root: &std::path::Path,
    source: &HostCredentialSource,
    provider: &str,
    budget: Duration,
) -> Result<File, &'static str> {
    #[cfg(not(unix))]
    {
        let _ = (root, source, provider, budget);
        return Err("safe_authority_lock_unavailable");
    }
    #[cfg(unix)]
    {
        let key = source_key(source, provider);
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || {
            create_private_dir(&root)
                .map_err(|_| "authority_lock_root_refused_remove_foreign_or_insecure_directory")?;
            lock_private_timeout(&root.join(key), LockMode::Exclusive, budget, || Ok(()))
                .map_err(|_| "authority_lock_unavailable")?
                .ok_or("authority_lock_timeout")
        })
        .await
        .map_err(|_| "authority_lock_unavailable")?
    }
}
