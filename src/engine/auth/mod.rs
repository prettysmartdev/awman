//! `engine::auth` — `AuthEngine`. Consolidates host-side agent credential
//! resolution and headless server authentication (API key generation,
//! hashing, comparison, persistence, refresh, TLS material).

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};
use subtle::ConstantTimeEq;

use crate::data::fs::auth_paths::AuthPathResolver;
use crate::data::fs::headless_paths::HeadlessPaths;
use crate::data::session::{AgentName, Session};
use crate::engine::error::EngineError;

pub mod keychain;

/// Status of an agent's host-side credential discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCredentialStatus {
    pub agent: AgentName,
    pub config_file_present: bool,
    pub settings_dir_present: bool,
    pub keychain_env_vars: Vec<String>,
}

/// Env-var pairs to inject into an agent container.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentCredentials {
    pub env_vars: Vec<(String, String)>,
}

/// Newtype around a generated API key (32-byte URL-safe base64).
#[derive(Debug, Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn from_string(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Newtype around an API key hash (hex-encoded SHA-256).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyHash(String);

impl ApiKeyHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn from_hex(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthOutcome {
    Authorized,
    Unauthorized,
}

/// PEM-encoded TLS material.
#[derive(Debug, Clone)]
pub struct TlsMaterial {
    pub cert_pem: String,
    pub key_pem: String,
    pub fingerprint_sha256_hex: String,
}

#[derive(Debug, Clone)]
pub struct AuthEngine {
    auth_paths: AuthPathResolver,
    headless_paths: HeadlessPaths,
}

impl AuthEngine {
    pub fn new(_session: &Session) -> Result<Self, EngineError> {
        let auth_paths = AuthPathResolver::from_process_env().map_err(EngineError::Data)?;
        let headless_paths = HeadlessPaths::from_process_env().map_err(EngineError::Data)?;
        Ok(Self {
            auth_paths,
            headless_paths,
        })
    }

    pub fn with_paths(auth_paths: AuthPathResolver, headless_paths: HeadlessPaths) -> Self {
        Self {
            auth_paths,
            headless_paths,
        }
    }

    // ── Agent credential discovery ──────────────────────────────────────────

    /// Inspect the host for the agent's credentials. Always returns a status
    /// (never errors when files are absent).
    pub fn list_agent_credentials(
        &self,
        agent: &AgentName,
    ) -> Result<AgentCredentialStatus, EngineError> {
        let paths = self.auth_paths.resolve(agent.as_str());
        let config_file_present = paths
            .config_file
            .as_ref()
            .map(|p| p.exists())
            .unwrap_or(false);
        let settings_dir_present = paths
            .settings_dir
            .as_ref()
            .map(|p| p.exists())
            .unwrap_or(false);
        let keychain = keychain::agent_keychain_credentials(agent);
        Ok(AgentCredentialStatus {
            agent: agent.clone(),
            config_file_present,
            settings_dir_present,
            keychain_env_vars: keychain.into_iter().map(|(k, _)| k).collect(),
        })
    }

    /// Look up keychain credentials only.
    pub fn agent_keychain_credentials(
        &self,
        agent: &AgentName,
    ) -> Result<AgentCredentials, EngineError> {
        Ok(AgentCredentials {
            env_vars: keychain::agent_keychain_credentials(agent),
        })
    }

    /// Composite resolver: keychain credentials scoped to the per-repo config.
    ///
    /// The decision to *use* keychain credentials silently vs prompting is a
    /// Layer 2 concern (governed by `auto_agent_auth_accepted`). This method
    /// only resolves the credentials.
    pub fn resolve_agent_auth(
        &self,
        _session: &Session,
        agent: &AgentName,
    ) -> Result<AgentCredentials, EngineError> {
        self.agent_keychain_credentials(agent)
    }

    // ── Headless API-key lifecycle ─────────────────────────────────────────

    /// Generate a fresh 32-byte API key, base64 URL-safe encoded.
    pub fn generate_api_key(&self) -> Result<ApiKey, EngineError> {
        let mut buf = [0u8; 32];
        SystemRandom::new()
            .fill(&mut buf)
            .map_err(|_| EngineError::Auth("failed to generate random bytes".into()))?;
        Ok(ApiKey(base64_url_encode(&buf)))
    }

    /// Hash an API key (SHA-256 → hex).
    pub fn hash_api_key(&self, key: &ApiKey) -> ApiKeyHash {
        let h = digest::digest(&digest::SHA256, key.0.as_bytes());
        ApiKeyHash(hex_encode(h.as_ref()))
    }

    /// Persist the hash to `<headless-root>/api_key.hash` with mode 0o600 on Unix.
    pub fn write_api_key_hash(&self, hash: &ApiKeyHash) -> Result<(), EngineError> {
        let path = self.headless_paths.api_key_hash_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| EngineError::io(parent, e))?;
        }
        write_file_secure(&path, hash.0.as_bytes())?;
        Ok(())
    }

    /// Read the persisted hash, or `None` when absent.
    pub fn read_api_key_hash(&self) -> Result<Option<ApiKeyHash>, EngineError> {
        let path = self.headless_paths.api_key_hash_file();
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(ApiKeyHash(s.trim().to_string()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(EngineError::io(path, e)),
        }
    }

    /// Constant-time API-key verification. Even when no hash exists on disk,
    /// the implementation performs a sentinel comparison so timing does not
    /// leak whether auth is configured.
    pub fn verify_api_key(&self, presented: &ApiKey) -> Result<AuthOutcome, EngineError> {
        let presented_hash = self.hash_api_key(presented);
        let on_disk = self.read_api_key_hash()?;
        let target = on_disk.unwrap_or_else(|| ApiKeyHash(SENTINEL_HASH.to_string()));

        // Constant-time hex comparison. Both inputs are equal length (64
        // hex chars from SHA-256); pad anyway for defense in depth.
        let a = presented_hash.0.as_bytes();
        let b = target.0.as_bytes();
        let len = a.len().max(b.len());
        let mut a_buf = vec![0u8; len];
        let mut b_buf = vec![0u8; len];
        a_buf[..a.len()].copy_from_slice(a);
        b_buf[..b.len()].copy_from_slice(b);
        if bool::from(a_buf.ct_eq(&b_buf)) {
            Ok(AuthOutcome::Authorized)
        } else {
            Ok(AuthOutcome::Unauthorized)
        }
    }

    /// Generate, persist, and return a fresh API key (rotation).
    pub fn refresh_api_key(&self) -> Result<ApiKey, EngineError> {
        let key = self.generate_api_key()?;
        let hash = self.hash_api_key(&key);
        self.write_api_key_hash(&hash)?;
        Ok(key)
    }

    // ── TLS material ───────────────────────────────────────────────────────

    /// Generate a self-signed certificate for the bind IP (placeholder until
    /// 0070 wires the actual self-signed flow with `rcgen` or similar). For
    /// now this generates a deterministic placeholder so callers can wire up
    /// their TLS plumbing in 0068/0069.
    pub fn ensure_self_signed_tls(&self, _bind_ip: IpAddr) -> Result<TlsMaterial, EngineError> {
        Err(EngineError::NotImplemented(
            "self-signed TLS material is implemented in a later WI",
        ))
    }

    /// Load TLS material from explicit paths.
    pub fn load_tls_from_paths(
        &self,
        cert: &Path,
        key: &Path,
    ) -> Result<TlsMaterial, EngineError> {
        let cert_pem = std::fs::read_to_string(cert).map_err(|e| EngineError::io(cert, e))?;
        let key_pem = std::fs::read_to_string(key).map_err(|e| EngineError::io(key, e))?;
        let h = digest::digest(&digest::SHA256, cert_pem.as_bytes());
        Ok(TlsMaterial {
            cert_pem,
            key_pem,
            fingerprint_sha256_hex: hex_encode(h.as_ref()),
        })
    }
}

/// Sentinel hash used by `verify_api_key` when no on-disk hash exists.
/// 64 hex zeros.
const SENTINEL_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn base64_url_encode(bytes: &[u8]) -> String {
    const CHARSET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(CHARSET[((n >> 18) & 0x3F) as usize] as char);
        out.push(CHARSET[((n >> 12) & 0x3F) as usize] as char);
        out.push(CHARSET[((n >> 6) & 0x3F) as usize] as char);
        out.push(CHARSET[(n & 0x3F) as usize] as char);
        i += 3;
    }
    if i < bytes.len() {
        let rem = bytes.len() - i;
        let mut n: u32 = 0;
        for j in 0..rem {
            n |= (bytes[i + j] as u32) << (16 - 8 * j);
        }
        out.push(CHARSET[((n >> 18) & 0x3F) as usize] as char);
        out.push(CHARSET[((n >> 12) & 0x3F) as usize] as char);
        if rem == 2 {
            out.push(CHARSET[((n >> 6) & 0x3F) as usize] as char);
        }
    }
    out
}

fn write_file_secure(path: &Path, content: &[u8]) -> Result<PathBuf, EngineError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| EngineError::io(path, e))?;
        std::io::Write::write_all(&mut f, content).map_err(|e| EngineError::io(path, e))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content).map_err(|e| EngineError::io(path, e))?;
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fs::auth_paths::AuthPathResolver;
    use crate::data::fs::headless_paths::HeadlessPaths;

    fn engine_with(home: &Path, headless_root: &Path) -> AuthEngine {
        AuthEngine::with_paths(
            AuthPathResolver::at_home(home),
            HeadlessPaths::at_root(headless_root),
        )
    }

    #[test]
    fn generate_then_verify_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join("h");
        std::fs::create_dir_all(&head).unwrap();
        let e = engine_with(tmp.path(), &head);
        let key = e.generate_api_key().unwrap();
        let hash = e.hash_api_key(&key);
        e.write_api_key_hash(&hash).unwrap();
        let outcome = e.verify_api_key(&key).unwrap();
        assert_eq!(outcome, AuthOutcome::Authorized);
    }

    #[test]
    fn verify_wrong_key_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join("h");
        std::fs::create_dir_all(&head).unwrap();
        let e = engine_with(tmp.path(), &head);
        let key = e.generate_api_key().unwrap();
        let hash = e.hash_api_key(&key);
        e.write_api_key_hash(&hash).unwrap();
        let bogus = ApiKey::from_string("not-the-key");
        assert_eq!(e.verify_api_key(&bogus).unwrap(), AuthOutcome::Unauthorized);
    }

    #[test]
    fn verify_with_no_hash_rejects_constant_time() {
        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join("h");
        let e = engine_with(tmp.path(), &head);
        let key = ApiKey::from_string("anything");
        assert_eq!(e.verify_api_key(&key).unwrap(), AuthOutcome::Unauthorized);
    }

    #[test]
    fn read_api_key_hash_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join("headless");
        let e = engine_with(tmp.path(), &head);
        assert!(e.read_api_key_hash().unwrap().is_none());
    }

    #[test]
    fn hash_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join("h");
        let e = engine_with(tmp.path(), &head);
        let key = ApiKey::from_string("my-test-key");
        let h1 = e.hash_api_key(&key);
        let h2 = e.hash_api_key(&key);
        assert_eq!(h1.as_str(), h2.as_str());
    }

    #[test]
    fn verify_uses_sentinel_when_hash_absent_so_timing_path_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join("h");
        let e = engine_with(tmp.path(), &head);
        // Even without a stored hash the verify path must complete without panic
        // (it compares against the sentinel). Outcome must be Unauthorized.
        let key = ApiKey::from_string("guess-attempt");
        let outcome = e.verify_api_key(&key).unwrap();
        assert_eq!(outcome, AuthOutcome::Unauthorized);
    }

    #[test]
    fn write_then_read_api_key_hash_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join("h");
        std::fs::create_dir_all(&head).unwrap();
        let e = engine_with(tmp.path(), &head);
        let key = e.generate_api_key().unwrap();
        let hash = e.hash_api_key(&key);
        e.write_api_key_hash(&hash).unwrap();
        let read_back = e.read_api_key_hash().unwrap().unwrap();
        assert_eq!(hash.as_str(), read_back.as_str());
    }
}
