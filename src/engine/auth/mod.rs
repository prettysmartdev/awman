//! `engine::auth` — `AuthEngine`. Consolidates host-side agent credential
//! resolution and the daemon network listener's credentials: named API keys
//! ([`api_keys::ApiKeyStore`], hashes only) and its self-signed TLS identity
//! ([`tls`], SANs from `daemon.network.tlsSans`).

use std::path::PathBuf;
use std::time::SystemTime;

use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};

use crate::data::config::agents::AgentAuthMode;
use crate::data::fs::api_paths::ApiPaths;
use crate::data::fs::auth_paths::AuthPathResolver;
use crate::data::session::{AgentName, Session};
use crate::engine::error::EngineError;

pub mod api_keys;
pub mod credential;
pub mod github_token;
pub mod host_secret_command;
pub mod keychain;
pub mod redaction;
pub mod tls;

pub use api_keys::{ApiKeyStore, KeyVerdict};
pub use tls::TlsIdentity;

use credential::{CredentialFingerprint, CredentialReadError, RefreshableCredentialSpec};

/// Map an awman credential env-var name to its well-known service name, or
/// `None` when awman has no mapping for it.
///
/// This is the canonical source of the credential→service table. The Docker
/// Sandbox driver ([`crate::engine::sandbox::dsbx::auth`]) re-exports this
/// function rather than maintaining its own copy, so all runtimes share one
/// definition. Callers that need to detect "does this declared env var cover
/// the same auth service as a keychain credential?" use this function to
/// compare services by name rather than by hardcoded var pairs.
///
/// Service names match the Docker Sandboxes well-known service table at
/// docs.docker.com/ai/sandboxes/security/credentials/ (June 2026).
pub fn service_for_credential(key: &str) -> Option<&'static str> {
    match key {
        // Anthropic — API key (all runtimes) and OAuth token (container runtimes;
        // the sbx driver skips the OAuth token for a separate reason: sbx has no
        // OAuth support yet, tracked at docker/sbx-releases#11).  Both vars
        // authenticate the same provider so both map to the same service here.
        "ANTHROPIC_API_KEY" | "CLAUDE_CODE_OAUTH_TOKEN" => Some("anthropic"),
        "OPENAI_API_KEY" => Some("openai"),
        "COPILOT_GITHUB_TOKEN" | "GH_TOKEN" | "GITHUB_TOKEN" => Some("github"),
        "GEMINI_API_KEY" | "GOOGLE_API_KEY" => Some("google"),
        "AWS_ACCESS_KEY_ID" | "AWS_SECRET_ACCESS_KEY" => Some("aws"),
        "GROQ_API_KEY" => Some("groq"),
        "MISTRAL_API_KEY" => Some("mistral"),
        _ => None,
    }
}

/// Status of an agent's host-side credential discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCredentialStatus {
    pub agent: AgentName,
    pub config_file_present: bool,
    pub settings_dir_present: bool,
    pub keychain_env_vars: Vec<String>,
    /// True when this agent has a refresh descriptor (file-delivered,
    /// refreshable credential).
    pub refreshable: bool,
    /// Absolute expiry of the host credential, when it is readable and states
    /// one. Populated only for refreshable agents.
    pub expires_at: Option<SystemTime>,
    /// Why the host credential could not be read, when it could not. `None`
    /// when the credential read fine (or the agent is not refreshable).
    pub read_error: Option<CredentialReadError>,
}

/// How an agent's resolved credentials are delivered to its runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CredentialDelivery {
    /// Today's behaviour: values ride `-e KEY` plus the child's environment.
    /// Every agent without a descriptor, every sandbox-class runtime, and
    /// `auth: passthrough` / `auth: none`.
    #[default]
    Env,
    /// Claude on container-class runtimes under `auth: keychain`. The staged
    /// credential file carries the secret; `env_vars` is empty in this case.
    ///
    /// NOTE for downstream steps: the [`RefreshableCredentialDelivery`] carried
    /// here is produced by [`AuthEngine::resolve_agent_auth`] **before** the
    /// overlay is staged, so its path fields are placeholders — it is a
    /// *discriminant only*. The authoritative `RefreshableCredentialDelivery`
    /// (with real `staged_path`/`staged_root`/`initial_fingerprint`) is built by
    /// the overlay/agent-engine step from the staged `CredentialFile`. Never
    /// read this variant's paths.
    File(RefreshableCredentialDelivery),
}

/// Credentials resolved for one agent launch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentCredentials {
    /// Env-var pairs to inject. MUST be empty when `delivery` is `File` — that
    /// emptiness is the mechanical guarantee that `CLAUDE_CODE_OAUTH_TOKEN`
    /// cannot be emitted alongside the credential file and mask it (Claude ranks
    /// the env var above the file).
    pub env_vars: Vec<(String, String)>,
    pub delivery: CredentialDelivery,
}

/// A file-delivered credential for one container launch. Carries **no secret**:
/// the secret lives only in the staged file. The value travels
/// auth → agent engine → container options → backend `build()` → lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshableCredentialDelivery {
    /// Agent this credential belongs to.
    pub agent: AgentName,
    /// Descriptor lookup key, equal to `agent.as_str()`. Kept separate so the
    /// monitor can find the `&'static RefreshableCredentialSpec` without holding
    /// a reference across the option bag.
    pub spec_agent: &'static str,
    /// The env-var name this credential would occupy under legacy env delivery.
    /// Used ONLY for the `service_for_credential` dedup lookup.
    pub credential_env_key: &'static str,
    /// ABSOLUTE path of the staged credential file on the host — the staged
    /// `~/.claude` TempDir root joined with `CredentialFile::relative_path`.
    /// This path IS allowed in logs and `-v` mount rendering; its CONTENTS are
    /// not (INV-5). Empty on the discriminant-only value the auth layer emits.
    pub staged_path: PathBuf,
    /// Directory the atomic rename must happen inside (the staged `~/.claude`
    /// root). Always `staged_path.parent()`; stored explicitly so the monitor
    /// never has to re-derive it. Empty on the discriminant-only value.
    pub staged_root: PathBuf,
    /// Fingerprint of the snapshot the file was first written from. Zeroed on
    /// the discriminant-only value the auth layer emits.
    pub initial_fingerprint: CredentialFingerprint,
}

impl RefreshableCredentialDelivery {
    /// A discriminant-only value with placeholder paths, produced by
    /// [`AuthEngine::resolve_agent_auth`] to flag that an agent takes
    /// file delivery. The staging step replaces it with the authoritative
    /// value; the path fields here MUST NOT be consumed.
    fn discriminant(agent: &AgentName, spec: &'static RefreshableCredentialSpec) -> Self {
        Self {
            agent: agent.clone(),
            spec_agent: spec.agent,
            credential_env_key: spec.credential_env_key,
            staged_path: PathBuf::new(),
            staged_root: PathBuf::new(),
            initial_fingerprint: CredentialFingerprint::zeroed(),
        }
    }
}

/// Newtype around a generated API key (32 random bytes encoded as 64-char
/// lowercase hex). Its `Debug` output never shows the key.
#[derive(Clone)]
pub struct ApiKey(String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

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

/// PEM-encoded TLS material.
#[derive(Clone, PartialEq, Eq)]
pub struct TlsMaterial {
    pub cert_pem: String,
    pub key_pem: String,
    pub fingerprint_sha256_hex: String,
}

/// The private key never reaches a log line through `{:?}`.
impl std::fmt::Debug for TlsMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsMaterial")
            .field("cert_pem", &self.cert_pem)
            .field("key_pem", &"<redacted>")
            .field("fingerprint_sha256_hex", &self.fingerprint_sha256_hex)
            .finish()
    }
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

    // ── Workflow prerequisite credentials (WI 0149) ─────────────────────────

    /// A resolver for the GitHub token sources a workflow prerequisite may use.
    pub fn github_token_resolver(
        &self,
        opts: github_token::GitHubTokenResolverOptions,
    ) -> github_token::GitHubTokenResolver {
        github_token::GitHubTokenResolver::new(opts)
    }

    /// The sole constructor of a prerequisite host command; see
    /// [`host_secret_command`] for the hardening it applies.
    pub fn host_secret_command(
        &self,
        opts: host_secret_command::HostSecretCommandOptions,
    ) -> Result<host_secret_command::HostSecretCommand, host_secret_command::HostSecretCommandError>
    {
        host_secret_command::HostSecretCommand::new(opts)
    }

    /// Whether the host `gh` is installed and logged in to `host` (shared by
    /// `poll_ci` and the prerequisite preflight).
    pub fn host_gh_authenticated(host: &str) -> bool {
        github_token::GitHubTokenResolver::host_gh_authenticated(host)
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
        // For refreshable agents, read the descriptor's source to surface
        // time-to-expiry / readability for `awman ready`. Never holds the secret
        // past this scope. Hermetic in tests: an absent host file/keychain entry
        // yields `read_error = Some(NotFound)`, not a panic.
        let (refreshable, expires_at, read_error) = match keychain::refreshable_spec_for(agent) {
            Some(spec) => {
                let source = (spec.source)(&self.auth_paths);
                match (spec.read)(&source) {
                    Ok(snap) => (true, (spec.expiry)(&snap), None),
                    Err(err) => (true, None, Some(err)),
                }
            }
            None => (false, None, None),
        };
        Ok(AgentCredentialStatus {
            agent: agent.clone(),
            config_file_present,
            settings_dir_present,
            keychain_env_vars: keychain.into_iter().map(|(k, _)| k).collect(),
            refreshable,
            expires_at,
            read_error,
        })
    }

    /// Look up keychain credentials only. Always **env** delivery.
    pub fn agent_keychain_credentials(
        &self,
        agent: &AgentName,
    ) -> Result<AgentCredentials, EngineError> {
        Ok(AgentCredentials {
            env_vars: keychain::agent_keychain_credentials(agent),
            delivery: CredentialDelivery::Env,
        })
    }

    /// Kill-switch env delivery for a refreshable agent: read the descriptor's
    /// cross-platform host source and deliver the access token via its env key.
    ///
    /// This is what `authRefresh.enabled: false` uses instead of the staged
    /// credential file. Because the host `.credentials.json` is always
    /// denylisted from the mount (INV-2), a non-macOS container would otherwise
    /// receive no credential at all in disabled mode; reading the access token
    /// here and delivering it as `CLAUDE_CODE_OAUTH_TOKEN` restores the
    /// pre-WI-0107 env behaviour on every platform (HIGH-6). The secret rides
    /// the existing name-only `-e KEY` transport, never argv (INV-5). Falls back
    /// to the legacy keychain env path if the descriptor source is unreadable.
    fn refreshable_env_credentials(
        &self,
        agent: &AgentName,
    ) -> Result<AgentCredentials, EngineError> {
        if let Some(spec) = keychain::refreshable_spec_for(agent) {
            let source = (spec.source)(&self.auth_paths);
            match (spec.read)(&source) {
                Ok(snapshot) => {
                    return Ok(AgentCredentials {
                        env_vars: vec![(
                            spec.credential_env_key.to_string(),
                            snapshot.secret.expose().to_string(),
                        )],
                        delivery: CredentialDelivery::Env,
                    });
                }
                Err(reason) => {
                    tracing::warn!(
                        agent = %agent,
                        reason = %reason,
                        "authRefresh disabled: could not read host credential for env delivery; \
                         falling back to legacy keychain env path"
                    );
                }
            }
        }
        self.agent_keychain_credentials(agent)
    }

    /// Env-only credential resolution, ignoring refresh descriptors. The
    /// sandbox/sbx path calls this so its behaviour is unchanged by construction
    /// (a `File` delivery can never reach a sandbox runtime — INV-10).
    pub fn agent_env_credentials(
        &self,
        agent: &AgentName,
    ) -> Result<AgentCredentials, EngineError> {
        Ok(AgentCredentials {
            env_vars: keychain::agent_keychain_credentials(agent),
            delivery: CredentialDelivery::Env,
        })
    }

    /// Composite resolver: keychain credentials gated on the repo `auth` mode.
    ///
    /// - `keychain` (default) — inject host keychain credentials. Part-A dedup
    ///   is applied later at `ResolvedContainerOptions::resolve` time.
    /// - `passthrough` — skip keychain injection entirely; the repo must
    ///   supply credentials via `env(VAR)` overlays.  Declared `env()` overlays
    ///   still apply.
    /// - `none` — no KEYCHAIN credential injection; declared `env()` overlays
    ///   still apply.
    ///
    /// The Layer-2 `auto_agent_auth_accepted` consent flag is a separate concern;
    /// this method only resolves *which* credentials to inject.
    pub fn resolve_agent_auth(
        &self,
        session: &Session,
        agent: &AgentName,
    ) -> Result<AgentCredentials, EngineError> {
        match session.effective_config().auth_mode() {
            AgentAuthMode::Keychain => {
                if session.effective_config().auth_refresh().enabled {
                    if let Some(spec) = keychain::refreshable_spec_for(agent) {
                        // Refreshable, file-delivered credential (Claude on
                        // container-class runtimes). `env_vars` is emptied so the
                        // OAuth token cannot be emitted on the container path and
                        // mask the staged file; the discriminant flags file delivery
                        // to the agent engine, which builds the authoritative
                        // `RefreshableCredentialDelivery` from the staged file.
                        //
                        Ok(AgentCredentials {
                            env_vars: Vec::new(),
                            delivery: CredentialDelivery::File(
                                RefreshableCredentialDelivery::discriminant(agent, spec),
                            ),
                        })
                    } else {
                        self.agent_keychain_credentials(agent)
                    }
                } else {
                    // Kill switch: env delivery, no staged credential file and
                    // no monitor lease. The host `.credentials.json` is ALWAYS
                    // denylisted from the mount (INV-2), so a refreshable agent
                    // cannot rely on the file being present in the container —
                    // read the descriptor's cross-platform source and deliver
                    // the access token as an env var instead. On non-macOS this
                    // reads `~/.claude/.credentials.json`; on macOS the source is
                    // the keychain, matching the legacy env path exactly (HIGH-6).
                    self.refreshable_env_credentials(agent)
                }
            }
            AgentAuthMode::Passthrough | AgentAuthMode::None => Ok(AgentCredentials::default()),
        }
    }
}

/// Constant-time comparison of two hex digests.
///
/// Both are 64 hex characters from SHA-256 in every real call; the buffers are
/// padded to a common length anyway, so a caller that ever passes something
/// shorter compares in constant time rather than returning early on length.
pub(crate) fn constant_time_hex_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    let a = a.as_bytes();
    let b = b.as_bytes();
    let len = a.len().max(b.len());
    let mut a_buf = vec![0u8; len];
    let mut b_buf = vec![0u8; len];
    a_buf[..a.len()].copy_from_slice(a);
    b_buf[..b.len()].copy_from_slice(b);
    bool::from(a_buf.ct_eq(&b_buf))
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Generate a fresh API key: 32 random bytes as 64 lowercase hex characters.
pub(crate) fn generate_key() -> Result<ApiKey, EngineError> {
    let mut buf = [0u8; 32];
    SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| EngineError::Auth("failed to generate random bytes".into()))?;
    Ok(ApiKey(hex_encode(&buf)))
}

/// SHA-256 of a presented or minted key, lowercase hex. Keys are 256-bit
/// random values, so a fast unsalted digest is sufficient.
pub(crate) fn hash_key(key: &str) -> String {
    hex_encode(digest::digest(&digest::SHA256, key.as_bytes()).as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::fs::api_paths::ApiPaths;
    use crate::data::fs::auth_paths::AuthPathResolver;
    use std::path::Path;

    fn engine_with(home: &Path, api_root: &Path) -> AuthEngine {
        AuthEngine::with_paths(AuthPathResolver::at_home(home), ApiPaths::at_root(api_root))
    }

    /// A custom agent name (no first-party entry) takes every `_ =>` path:
    /// no host artefacts, no keychain credentials, nothing refreshable.
    #[test]
    fn a_custom_agent_name_has_no_host_credentials() {
        let tmp = tempfile::tempdir().expect("scratch");
        let engine = engine_with(tmp.path(), tmp.path());
        let agent = AgentName::new("pi").unwrap();
        let status = engine.list_agent_credentials(&agent).unwrap();
        assert!(!status.config_file_present);
        assert!(!status.settings_dir_present);
        assert!(status.keychain_env_vars.is_empty());
        assert!(!status.refreshable);
        assert!(keychain::refreshable_spec_for(&agent).is_none());
        let creds = engine.agent_env_credentials(&agent).unwrap();
        assert!(creds.env_vars.is_empty());
    }

    // ── Part B: resolve_agent_auth auth-mode gating ───────────────────────────

    use crate::data::config::file::{CONFIG_FILENAME as REPO_CONFIG_FILENAME, REPO_CONFIG_SUBDIR};
    use crate::data::config::EnvSnapshot;
    use crate::data::fs::data_paths::AWMAN_HOME;
    use crate::data::session::{Session, SessionOpenOptions};

    /// Build an isolated session with the given repo config JSON written to disk.
    ///
    /// Uses a temp home dir (via `AWMAN_HOME`) so no process-global env
    /// is read or mutated.
    fn session_with_repo_config(
        git_root: &std::path::Path,
        home_dir: &std::path::Path,
        repo_cfg: &serde_json::Value,
    ) -> Session {
        // Write repo config.
        let awman_dir = git_root.join(REPO_CONFIG_SUBDIR);
        std::fs::create_dir_all(&awman_dir).unwrap();
        let cfg_json = serde_json::to_string_pretty(repo_cfg).unwrap();
        std::fs::write(awman_dir.join(REPO_CONFIG_FILENAME), cfg_json).unwrap();

        let env = EnvSnapshot::with_overrides([(AWMAN_HOME, home_dir.to_str().unwrap())]);

        let resolver = crate::data::session::StaticGitRootResolver::new(git_root);
        Session::open(
            git_root.to_path_buf(),
            &resolver,
            SessionOpenOptions {
                env: Some(env),
                ..Default::default()
            },
        )
        .unwrap()
    }

    /// `passthrough` mode: `resolve_agent_auth` returns an empty credential set
    /// without touching the host keychain (hermetic even on macOS).
    #[test]
    fn passthrough_mode_yields_empty_credentials() {
        let git_tmp = tempfile::tempdir().unwrap();
        let home_tmp = tempfile::tempdir().unwrap();
        let session = session_with_repo_config(
            git_tmp.path(),
            home_tmp.path(),
            &serde_json::json!({"agents": {"auth": {"mode": "passthrough"}}}),
        );
        let engine = engine_with(home_tmp.path(), home_tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let creds = engine.resolve_agent_auth(&session, &agent).unwrap();
        assert!(
            creds.env_vars.is_empty(),
            "passthrough mode must return no keychain credentials; got: {:?}",
            creds.env_vars
        );
    }

    /// `none` mode: same result — no credential injection.
    #[test]
    fn none_mode_yields_empty_credentials() {
        let git_tmp = tempfile::tempdir().unwrap();
        let home_tmp = tempfile::tempdir().unwrap();
        let session = session_with_repo_config(
            git_tmp.path(),
            home_tmp.path(),
            &serde_json::json!({"agents": {"auth": {"mode": "none"}}}),
        );
        let engine = engine_with(home_tmp.path(), home_tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let creds = engine.resolve_agent_auth(&session, &agent).unwrap();
        assert!(
            creds.env_vars.is_empty(),
            "none mode must return no keychain credentials; got: {:?}",
            creds.env_vars
        );
    }

    /// `keychain` explicitly set: same as default — attempt keychain lookup
    /// (may be empty on CI without a keychain; the important thing is that it
    /// does NOT return early with empty credentials the way passthrough/none do).
    /// We verify via the successful `Ok` return, not the credential count.
    #[test]
    fn keychain_mode_attempts_keychain_lookup() {
        let git_tmp = tempfile::tempdir().unwrap();
        let home_tmp = tempfile::tempdir().unwrap();
        let session = session_with_repo_config(
            git_tmp.path(),
            home_tmp.path(),
            &serde_json::json!({"agents": {"auth": {"mode": "keychain"}}}),
        );
        let engine = engine_with(home_tmp.path(), home_tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        // Must not error regardless of whether the keychain has credentials.
        let result = engine.resolve_agent_auth(&session, &agent);
        assert!(
            result.is_ok(),
            "keychain mode must not error when keychain is absent; got: {result:?}"
        );
    }

    /// Default (no `auth` field in repo config): same as `keychain`.
    #[test]
    fn default_mode_behaves_like_keychain() {
        let git_tmp = tempfile::tempdir().unwrap();
        let home_tmp = tempfile::tempdir().unwrap();
        let session =
            session_with_repo_config(git_tmp.path(), home_tmp.path(), &serde_json::json!({}));
        let engine = engine_with(home_tmp.path(), home_tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let result = engine.resolve_agent_auth(&session, &agent);
        assert!(
            result.is_ok(),
            "default mode must not error; got: {result:?}"
        );
    }

    // ── Part C: File vs Env delivery discriminant ────────────────────────────

    /// Claude has a refresh descriptor: under `keychain` mode with refresh
    /// enabled (the default), it must take `File` delivery with no env vars —
    /// the discriminant `delivery-overlay` later replaces with the
    /// authoritative `RefreshableCredentialDelivery`.
    #[test]
    fn claude_keychain_mode_with_refresh_enabled_takes_file_delivery() {
        let git_tmp = tempfile::tempdir().unwrap();
        let home_tmp = tempfile::tempdir().unwrap();
        let session = session_with_repo_config(
            git_tmp.path(),
            home_tmp.path(),
            &serde_json::json!({"agents": {"auth": {"mode": "keychain"}}}),
        );
        let engine = engine_with(home_tmp.path(), home_tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let creds = engine.resolve_agent_auth(&session, &agent).unwrap();
        assert!(
            matches!(creds.delivery, CredentialDelivery::File(_)),
            "claude under keychain mode with refresh enabled must take File delivery; got {:?}",
            creds.delivery
        );
        assert!(
            creds.env_vars.is_empty(),
            "File delivery must carry no env vars, so the OAuth token cannot be \
             emitted alongside the staged credential file"
        );
    }

    /// An agent with no refresh descriptor (every agent except claude) keeps
    /// today's `Env` delivery under `keychain` mode, even with refresh
    /// enabled — the descriptor-lookup fallback.
    #[test]
    fn agent_without_descriptor_falls_back_to_env_delivery() {
        let git_tmp = tempfile::tempdir().unwrap();
        let home_tmp = tempfile::tempdir().unwrap();
        let session = session_with_repo_config(
            git_tmp.path(),
            home_tmp.path(),
            &serde_json::json!({"agents": {"auth": {"mode": "keychain"}}}),
        );
        let engine = engine_with(home_tmp.path(), home_tmp.path());
        for name in [
            "codex",
            "gemini",
            "opencode",
            "antigravity",
            "totallymadeup",
        ] {
            let agent = crate::data::session::AgentName::new(name).unwrap();
            let creds = engine.resolve_agent_auth(&session, &agent).unwrap();
            assert_eq!(
                creds.delivery,
                CredentialDelivery::Env,
                "{name} has no refresh descriptor and must keep Env delivery"
            );
        }
    }

    /// `authRefresh.enabled: false` is the kill switch: even claude, which has
    /// a descriptor, falls back to legacy Env delivery — the pre-WI-0107
    /// behaviour is restored exactly.
    #[test]
    fn auth_refresh_disabled_restores_env_delivery_for_claude() {
        let git_tmp = tempfile::tempdir().unwrap();
        let home_tmp = tempfile::tempdir().unwrap();
        let session = session_with_repo_config(
            git_tmp.path(),
            home_tmp.path(),
            &serde_json::json!({"agents": {"auth": {"mode": "keychain", "refresh": {"enabled": false}}}}),
        );
        let engine = engine_with(home_tmp.path(), home_tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let creds = engine.resolve_agent_auth(&session, &agent).unwrap();
        assert_eq!(
            creds.delivery,
            CredentialDelivery::Env,
            "authRefresh.enabled=false must restore legacy env delivery even for claude"
        );
    }

    /// HIGH-6 (INV-2 corollary): with the kill switch on, a non-macOS container
    /// must still be authenticated. The host `.credentials.json` is always
    /// denylisted from the mount, so disabled mode reads the access token from
    /// the descriptor's host-file source and delivers it as
    /// `CLAUDE_CODE_OAUTH_TOKEN` — the container is never left credential-less.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn auth_refresh_disabled_delivers_access_token_via_env_on_non_macos() {
        let git_tmp = tempfile::tempdir().unwrap();
        let home_tmp = tempfile::tempdir().unwrap();
        // Plant a host credential the descriptor's HostFile source will read.
        let claude_dir = home_tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-DISABLED-SENTINEL","refreshToken":"must-not-appear"}}"#,
        )
        .unwrap();

        let session = session_with_repo_config(
            git_tmp.path(),
            home_tmp.path(),
            &serde_json::json!({"agents": {"auth": {"mode": "keychain", "refresh": {"enabled": false}}}}),
        );
        let engine = engine_with(home_tmp.path(), home_tmp.path());
        let agent = crate::data::session::AgentName::new("claude").unwrap();
        let creds = engine.resolve_agent_auth(&session, &agent).unwrap();
        assert_eq!(creds.delivery, CredentialDelivery::Env);
        assert_eq!(
            creds.env_vars,
            vec![(
                "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
                "sk-ant-oat-DISABLED-SENTINEL".to_string()
            )],
            "disabled mode must deliver the access token via env so the container is authenticated"
        );
    }
}
