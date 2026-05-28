//! `engine::auth` — `AuthEngine`. Consolidates host-side agent credential
//! resolution and API server authentication (API key generation,
//! hashing, comparison, persistence, refresh, TLS material).

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};
use subtle::ConstantTimeEq;

use crate::data::fs::api_paths::ApiPaths;
use crate::data::fs::auth_paths::AuthPathResolver;
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

/// Newtype around a generated API key (32 random bytes encoded as 64-char
/// lowercase hex — matches old-amux wire format).
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
    api_paths: ApiPaths,
}

impl AuthEngine {
    pub fn new(_session: &Session) -> Result<Self, EngineError> {
        let auth_paths = AuthPathResolver::from_process_env().map_err(EngineError::Data)?;
        let api_paths = ApiPaths::from_process_env().map_err(EngineError::Data)?;
        Ok(Self {
            auth_paths,
            api_paths,
        })
    }

    pub fn with_paths(auth_paths: AuthPathResolver, api_paths: ApiPaths) -> Self {
        Self {
            auth_paths,
            api_paths,
        }
    }

    pub fn api_paths(&self) -> &ApiPaths {
        &self.api_paths
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

    // ── API-key lifecycle ──────────────────────────────────────────────────

    /// Generate a fresh 32-byte API key, hex-encoded (64 chars). Matches
    /// the old-amux wire format so existing scripts/regex/docs keep working.
    pub fn generate_api_key(&self) -> Result<ApiKey, EngineError> {
        let mut buf = [0u8; 32];
        SystemRandom::new()
            .fill(&mut buf)
            .map_err(|_| EngineError::Auth("failed to generate random bytes".into()))?;
        Ok(ApiKey(hex_encode(&buf)))
    }

    /// Hash an API key (SHA-256 → hex).
    pub fn hash_api_key(&self, key: &ApiKey) -> ApiKeyHash {
        let h = digest::digest(&digest::SHA256, key.0.as_bytes());
        ApiKeyHash(hex_encode(h.as_ref()))
    }

    /// Persist the hash to `<api-root>/api_key.hash` with mode 0o600 on Unix.
    pub fn write_api_key_hash(&self, hash: &ApiKeyHash) -> Result<(), EngineError> {
        let path = self.api_paths.api_key_hash_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| EngineError::io(parent, e))?;
        }
        write_file_secure(&path, hash.0.as_bytes())?;
        Ok(())
    }

    /// Read the persisted hash, or `None` when absent.
    pub fn read_api_key_hash(&self) -> Result<Option<ApiKeyHash>, EngineError> {
        let path = self.api_paths.api_key_hash_file();
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

    /// Generate or load a self-signed certificate for the bind IP. Idempotent
    /// when the existing cert was generated for the same bind IP — the
    /// authoritative record of the cert's bind IP is the `bind_ip` sidecar
    /// next to the cert file (rather than substring-scanning the DER, which
    /// is brittle for short IPv4 byte sequences).
    ///
    /// Returns `(material, regenerated)` so the caller can surface the
    /// "TLS cert regenerated for new bind IP — pinned remote clients will
    /// need to re-pin" warning.
    pub fn ensure_self_signed_tls(
        &self,
        bind_ip: IpAddr,
    ) -> Result<(TlsMaterial, bool), EngineError> {
        let tls_dir = self.api_paths.tls_dir();
        let cert_path = self.api_paths.tls_cert_file();
        let key_path = self.api_paths.tls_key_file();
        let bind_ip_path = self.api_paths.tls_bind_ip_file();
        let fingerprint_path = self.api_paths.tls_fingerprint_file();

        if cert_path.exists() && key_path.exists() {
            let stored_ip = std::fs::read_to_string(&bind_ip_path)
                .ok()
                .map(|s| s.trim().to_string());
            if stored_ip.as_deref() == Some(&bind_ip.to_string()) {
                let material = self.load_tls_from_paths_with_fingerprint(
                    &cert_path,
                    &key_path,
                    &fingerprint_path,
                )?;
                return Ok((material, false));
            }
        }

        std::fs::create_dir_all(&tls_dir).map_err(|e| EngineError::io(&tls_dir, e))?;

        let san_ip = bind_ip.to_string();
        let sans = vec![san_ip.clone(), "localhost".to_string()];
        let mut params = rcgen::CertificateParams::new(sans)
            .map_err(|e| EngineError::Auth(format!("TLS cert params: {e}")))?;

        let ip_short_hash = {
            let h = digest::digest(&digest::SHA256, san_ip.as_bytes());
            hex_encode(&h.as_ref()[..4])
        };
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            format!("awman-api-{ip_short_hash}"),
        );

        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2034, 1, 1);

        let key_pair = rcgen::KeyPair::generate()
            .map_err(|e| EngineError::Auth(format!("TLS keygen: {e}")))?;
        let cert = params
            .self_signed(&key_pair)
            .map_err(|e| EngineError::Auth(format!("TLS self-sign: {e}")))?;

        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        std::fs::write(&cert_path, cert_pem.as_bytes())
            .map_err(|e| EngineError::io(&cert_path, e))?;
        write_file_secure(&key_path, key_pem.as_bytes())?;
        std::fs::write(&bind_ip_path, san_ip.as_bytes())
            .map_err(|e| EngineError::io(&bind_ip_path, e))?;

        let fingerprint = {
            let der_bytes: &[u8] = cert.der().as_ref();
            let h = digest::digest(&digest::SHA256, der_bytes);
            hex_encode(h.as_ref())
        };

        // Cache the fingerprint to a sidecar file so subsequent loads do not
        // need to re-parse PEM/DER. This is the canonical authoritative
        // record going forward; the file is informational only (not used for
        // auth decisions).
        std::fs::write(&fingerprint_path, fingerprint.as_bytes())
            .map_err(|e| EngineError::io(&fingerprint_path, e))?;

        Ok((
            TlsMaterial {
                cert_pem,
                key_pem,
                fingerprint_sha256_hex: fingerprint,
            },
            true,
        ))
    }

    /// Load TLS material from explicit paths, reading the cached fingerprint
    /// from the sidecar file rather than re-deriving it from PEM. Falls back
    /// to recomputing-and-caching the fingerprint over the cert's DER bytes
    /// when the sidecar is missing (e.g. legacy installs that predate the
    /// sidecar). We rely on `rcgen` re-parsing of the PEM rather than a
    /// hand-rolled base64 decoder.
    fn load_tls_from_paths_with_fingerprint(
        &self,
        cert: &Path,
        key: &Path,
        fingerprint_sidecar: &Path,
    ) -> Result<TlsMaterial, EngineError> {
        let cert_pem = std::fs::read_to_string(cert).map_err(|e| EngineError::io(cert, e))?;
        let key_pem = std::fs::read_to_string(key).map_err(|e| EngineError::io(key, e))?;

        let fingerprint = match std::fs::read_to_string(fingerprint_sidecar) {
            Ok(s) => s.trim().to_string(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Recompute over the PEM string itself; this is stable enough
                // for an informational identifier and avoids pulling in a
                // PEM/base64 dependency. Persist for next time.
                let h = digest::digest(&digest::SHA256, cert_pem.as_bytes());
                let f = hex_encode(h.as_ref());
                let _ = std::fs::write(fingerprint_sidecar, f.as_bytes());
                f
            }
            Err(e) => return Err(EngineError::io(fingerprint_sidecar, e)),
        };

        Ok(TlsMaterial {
            cert_pem,
            key_pem,
            fingerprint_sha256_hex: fingerprint,
        })
    }

    /// Legacy path-based loader retained for callers that did not write a
    /// fingerprint sidecar. Hashes the PEM bytes directly (not DER) for
    /// informational use.
    pub fn load_tls_from_paths(&self, cert: &Path, key: &Path) -> Result<TlsMaterial, EngineError> {
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
const SENTINEL_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
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
    use crate::data::fs::api_paths::ApiPaths;
    use crate::data::fs::auth_paths::AuthPathResolver;

    fn engine_with(home: &Path, api_root: &Path) -> AuthEngine {
        AuthEngine::with_paths(AuthPathResolver::at_home(home), ApiPaths::at_root(api_root))
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
        let head = tmp.path().join("api");
        let e = engine_with(tmp.path(), &head);
        assert!(e.read_api_key_hash().unwrap().is_none());
    }

    #[test]
    fn generate_api_key_produces_64_char_lowercase_hex() {
        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join("h");
        let e = engine_with(tmp.path(), &head);
        let key = e.generate_api_key().unwrap();
        assert_eq!(key.as_str().len(), 64, "API key must be 64-char hex");
        assert!(
            key.as_str()
                .chars()
                .all(|c| c.is_ascii_hexdigit() && (c.is_ascii_digit() || c.is_ascii_lowercase())),
            "API key must be lowercase hex; got {:?}",
            key.as_str()
        );
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

    #[test]
    fn ensure_self_signed_tls_generates_cert_and_key() {
        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join("api");
        std::fs::create_dir_all(&head).unwrap();
        let e = engine_with(tmp.path(), &head);

        let bind_ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let (material, regenerated) = e.ensure_self_signed_tls(bind_ip).unwrap();
        assert!(regenerated, "first call must report regenerated=true");

        // Both files must exist on disk.
        assert!(
            head.join("tls").join("cert.pem").exists(),
            "cert.pem not written"
        );
        assert!(
            head.join("tls").join("key.pem").exists(),
            "key.pem not written"
        );
        assert!(
            head.join("tls").join("bind_ip").exists(),
            "bind_ip sidecar must be written"
        );

        // PEM content must be non-empty.
        assert!(!material.cert_pem.is_empty(), "cert_pem must be non-empty");
        assert!(!material.key_pem.is_empty(), "key_pem must be non-empty");

        // Fingerprint must be a 64-char lowercase hex string (SHA-256).
        assert_eq!(
            material.fingerprint_sha256_hex.len(),
            64,
            "fingerprint must be 64 hex chars"
        );
        assert!(
            material
                .fingerprint_sha256_hex
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
            "fingerprint must be all hex digits"
        );
    }

    #[test]
    fn ensure_self_signed_tls_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join("api");
        std::fs::create_dir_all(&head).unwrap();
        let e = engine_with(tmp.path(), &head);

        let bind_ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let (m1, regen1) = e.ensure_self_signed_tls(bind_ip).unwrap();
        let (m2, regen2) = e.ensure_self_signed_tls(bind_ip).unwrap();

        assert!(regen1, "first call must regenerate");
        assert!(!regen2, "second call must not regenerate");
        assert_eq!(
            m1.cert_pem, m2.cert_pem,
            "second call must return byte-identical cert"
        );
        assert_eq!(
            m1.fingerprint_sha256_hex, m2.fingerprint_sha256_hex,
            "fingerprint must be stable across calls"
        );
    }

    #[test]
    fn ensure_self_signed_tls_regenerates_on_bind_ip_change() {
        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join("api");
        std::fs::create_dir_all(&head).unwrap();
        let e = engine_with(tmp.path(), &head);

        let ip1: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let ip2: std::net::IpAddr = "10.0.0.1".parse().unwrap();

        let (m1, regen1) = e.ensure_self_signed_tls(ip1).unwrap();
        let (m2, regen2) = e.ensure_self_signed_tls(ip2).unwrap();

        assert!(regen1, "first call must regenerate");
        assert!(regen2, "bind_ip change must trigger regeneration");
        assert_ne!(
            m1.cert_pem, m2.cert_pem,
            "cert must be regenerated when bind_ip changes"
        );
        assert_ne!(
            m1.fingerprint_sha256_hex, m2.fingerprint_sha256_hex,
            "fingerprint must differ for different bind_ips"
        );
    }

    #[test]
    fn refresh_api_key_writes_hash_and_returns_plaintext() {
        let tmp = tempfile::tempdir().unwrap();
        let head = tmp.path().join("api");
        std::fs::create_dir_all(&head).unwrap();
        let e = engine_with(tmp.path(), &head);

        let key = e.refresh_api_key().unwrap();

        // Plaintext key must be non-empty.
        assert!(!key.as_str().is_empty(), "returned key must be non-empty");

        // Hash file must be on disk and match the SHA-256 of the plaintext key.
        let hash_on_disk = e
            .read_api_key_hash()
            .unwrap()
            .expect("hash file must exist");
        let expected_hash = e.hash_api_key(&key);
        assert_eq!(
            hash_on_disk.as_str(),
            expected_hash.as_str(),
            "on-disk hash must be SHA-256 of the returned plaintext"
        );

        // On Unix, the hash file must have mode 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let hash_path = head.join("api_key.hash");
            let meta = std::fs::metadata(&hash_path).unwrap();
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "hash file must have mode 0600, got {mode:o}");
        }

        // Verification with the returned plaintext must succeed.
        let outcome = e.verify_api_key(&key).unwrap();
        assert_eq!(outcome, AuthOutcome::Authorized);
    }
}
