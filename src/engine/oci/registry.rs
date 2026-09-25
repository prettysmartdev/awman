//! OCI registry source: pull one platform's image over the distribution API
//! and write it out as an OCI image layout archive.
//!
//! * **Identity.** Every manifest and blob is hashed and compared with the
//!   digest that named it (and with `Docker-Content-Digest` when the
//!   registry sends one). A reference pinned by digest must resolve to
//!   exactly that digest.
//! * **Platform.** An image index is resolved to the manifest for the
//!   requested platform; the config's own `os`/`architecture` must agree.
//! * **Auth.** Anonymous, `Basic`, or `Bearer` token flows. Credentials come
//!   only from the explicit [`RegistryAuthSource`] configured for the host:
//!   named environment variables, awman's keychain entry, or inline
//!   `auths` in Docker's `config.json`. Docker credential helpers are never
//!   executed — a host that only has a helper configured gets an error that
//!   says so. Credentials live in memory as [`SecretString`] and never reach
//!   logs, errors, the cache or the archive.
//! * **Transport.** HTTPS by default with the platform's roots plus an
//!   optional private CA bundle; plain HTTP only for hosts explicitly marked
//!   `insecure`. Proxies come from `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`/
//!   `NO_PROXY` as read through awman's environment layer, not implicitly.
//!
//! This module performs blocking I/O and must be called off any async
//! runtime (the acquirer runs it on a dedicated thread).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::data::config::image_source::{RegistryAuthSource, RegistryHostConfig};
use crate::data::oci_identity::{Digest, OciPlatform};
use crate::engine::auth::credential::SecretString;
use crate::engine::error::EngineError;
use crate::engine::oci::sources::{FetchContext, Fetched, Report};
use crate::engine::oci::verify::{
    digest_hex, ensure_space, is_limit_exceeded, sha256_digest, HashingReader, StagingSink,
};
use crate::engine::oci::AcquireProgress;

const DOCKER_HUB: &str = "docker.io";
const DOCKER_HUB_API: &str = "registry-1.docker.io";
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TOKEN_BYTES: u64 = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Metadata requests (manifests, tokens) — blobs have no total timeout.
const METADATA_TIMEOUT: Duration = Duration::from_secs(120);
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);

const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

// ── References ─────────────────────────────────────────────────────────────

/// A parsed `[host[:port]/]repository[:tag][@sha256:…]` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    /// Registry as named (`docker.io` for Docker Hub).
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<Digest>,
}

impl ImageReference {
    /// Parse `reference`. `registry_override` is the source's `registry`
    /// field; a reference that names a *different* registry is refused
    /// rather than silently redirected.
    pub fn parse(reference: &str, registry_override: Option<&str>) -> Result<Self, String> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err("empty image reference".into());
        }
        if reference.contains("://") || reference.contains(char::is_whitespace) {
            return Err(format!("`{reference}` is not an image reference"));
        }
        let (rest, digest) = match reference.split_once('@') {
            Some((r, d)) => (r, Some(Digest::parse(d)?)),
            None => (reference, None),
        };
        let (named_registry, path) = match rest.split_once('/') {
            Some((first, remainder))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (Some(first.to_string()), remainder.to_string())
            }
            _ => (None, rest.to_string()),
        };
        let (repository, tag) = match path.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => (repo.to_string(), Some(tag.to_string())),
            _ => (path, None),
        };
        let registry = match (named_registry, registry_override.map(str::trim)) {
            (Some(named), Some(forced)) if !same_registry(&named, forced) => {
                return Err(format!(
                    "reference `{reference}` names registry `{named}`, but the source is \
                     configured for `{forced}`"
                ))
            }
            (Some(named), _) => named,
            (None, Some(forced)) => forced.to_string(),
            (None, None) => DOCKER_HUB.to_string(),
        };
        if registry.contains('@') || registry.contains('/') {
            return Err(format!("registry `{registry}` must be host[:port]"));
        }
        let repository = if is_docker_hub(&registry) && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        validate_repository(&repository)?;
        if let Some(tag) = &tag {
            validate_tag(tag)?;
        }
        let tag = if tag.is_none() && digest.is_none() {
            Some("latest".to_string())
        } else {
            tag
        };
        Ok(Self {
            registry,
            repository,
            tag,
            digest,
        })
    }

    /// The API host (`registry-1.docker.io` for Docker Hub).
    pub fn api_host(&self) -> &str {
        if is_docker_hub(&self.registry) {
            DOCKER_HUB_API
        } else {
            &self.registry
        }
    }

    /// What to ask the manifests endpoint for: the digest when pinned.
    fn manifest_ref(&self) -> String {
        match (&self.digest, &self.tag) {
            (Some(d), _) => d.to_string(),
            (None, Some(t)) => t.clone(),
            (None, None) => "latest".into(),
        }
    }
}

impl std::fmt::Display for ImageReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

fn is_docker_hub(registry: &str) -> bool {
    matches!(
        registry,
        "docker.io" | "index.docker.io" | "registry-1.docker.io"
    )
}

/// Token services that well-known registries delegate to on a different
/// host: `(registry API host, token host)`.
const KNOWN_TOKEN_HOSTS: &[(&str, &str)] = &[(DOCKER_HUB_API, "auth.docker.io")];

/// Whether a Bearer `realm` may receive the registry's credentials: it must
/// share the registry's origin (host and effective port), or be the known
/// HTTPS token host of a well-known registry.
fn realm_trusted(realm: &reqwest::Url, api_host: &str, insecure: bool) -> bool {
    let Some(realm_host) = realm.host_str() else {
        return false;
    };
    let scheme = if insecure { "http" } else { "https" };
    let Ok(registry) = reqwest::Url::parse(&format!("{scheme}://{api_host}/")) else {
        return false;
    };
    let same_host = registry
        .host_str()
        .is_some_and(|h| h.eq_ignore_ascii_case(realm_host));
    if same_host && registry.port_or_known_default() == realm.port_or_known_default() {
        return true;
    }
    realm.scheme() == "https"
        && realm.port_or_known_default() == Some(443)
        && KNOWN_TOKEN_HOSTS.iter().any(|(reg, token)| {
            reg.eq_ignore_ascii_case(api_host) && token.eq_ignore_ascii_case(realm_host)
        })
}

fn same_registry(a: &str, b: &str) -> bool {
    a == b || (is_docker_hub(a) && is_docker_hub(b))
}

fn validate_repository(repo: &str) -> Result<(), String> {
    let ok = !repo.is_empty()
        && repo.split('/').all(|c| {
            !c.is_empty()
                && c.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
                })
                && c.as_bytes()[0].is_ascii_alphanumeric()
        });
    if ok {
        Ok(())
    } else {
        Err(format!("`{repo}` is not a valid repository name"))
    }
}

fn validate_tag(tag: &str) -> Result<(), String> {
    let ok = !tag.is_empty()
        && tag.len() <= 128
        && tag.as_bytes()[0] != b'.'
        && tag.as_bytes()[0] != b'-'
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(format!("`{tag}` is not a valid tag"))
    }
}

/// The registry-host settings for `reference`, trying `host[:port]` and,
/// for Docker Hub, its aliases.
pub(super) fn host_config<'a>(
    reference: &ImageReference,
    registries: &'a BTreeMap<String, RegistryHostConfig>,
) -> Option<&'a RegistryHostConfig> {
    registries.get(&reference.registry).or_else(|| {
        if is_docker_hub(&reference.registry) {
            ["docker.io", "index.docker.io", "registry-1.docker.io"]
                .iter()
                .find_map(|k| registries.get(*k))
        } else {
            None
        }
    })
}

// ── Credentials ────────────────────────────────────────────────────────────

/// Registry credentials. The password never implements `Display`.
#[derive(Clone)]
pub(super) struct Credentials {
    username: String,
    password: SecretString,
    /// Where they came from, for error messages (never the value).
    origin: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

/// Read the credentials `auth` names for `registry`. `Ok(None)` is
/// anonymous access.
pub(super) fn resolve_credentials(
    registry: &str,
    auth: Option<&RegistryAuthSource>,
    lookup_var: &dyn Fn(&str) -> Option<String>,
    docker_config: &dyn Fn() -> Option<(PathBuf, Vec<u8>)>,
) -> Result<Option<Credentials>, EngineError> {
    match auth {
        None | Some(RegistryAuthSource::Anonymous) => Ok(None),
        Some(RegistryAuthSource::Env {
            username_var,
            password_var,
        }) => {
            let get = |name: &str| lookup_var(name).filter(|v| !v.is_empty());
            match (get(username_var), get(password_var)) {
                (Some(username), Some(password)) => Ok(Some(Credentials {
                    username,
                    password: SecretString::new(password),
                    origin: format!("environment variables {username_var}/{password_var}"),
                })),
                _ => Err(EngineError::Auth(format!(
                    "registry {registry}: environment variables {username_var} and \
                     {password_var} must both be set and non-empty"
                ))),
            }
        }
        Some(RegistryAuthSource::Keychain { service }) => {
            let value =
                crate::engine::auth::keychain::keychain_lookup(service, registry, KEYCHAIN_TIMEOUT)
                    .map_err(|e| {
                        EngineError::Auth(format!(
                    "registry {registry}: keychain service `{service}` could not be read: {e}"
                ))
                    })?
                    .ok_or_else(|| {
                        EngineError::Auth(format!(
                    "registry {registry}: keychain service `{service}` has no item for account \
                     `{registry}`"
                ))
                    })?;
            let (username, password) = split_user_pass(&value).ok_or_else(|| {
                EngineError::Auth(format!(
                    "registry {registry}: keychain item `{service}`/`{registry}` must hold \
                     `username:password`"
                ))
            })?;
            Ok(Some(Credentials {
                username,
                password: SecretString::new(password),
                origin: format!("keychain service `{service}`"),
            }))
        }
        Some(RegistryAuthSource::DockerConfig) => {
            let (path, bytes) = docker_config().ok_or_else(|| {
                EngineError::Auth(format!(
                    "registry {registry}: Docker config.json was requested for credentials but \
                     could not be read"
                ))
            })?;
            docker_config_credentials(registry, &path, &bytes)
        }
    }
}

fn split_user_pass(value: &str) -> Option<(String, String)> {
    let (u, p) = value.trim_end_matches(['\r', '\n']).split_once(':')?;
    (!u.is_empty() && !p.is_empty()).then(|| (u.to_string(), p.to_string()))
}

/// Inline `auths` from Docker's `config.json`. Credential helpers
/// (`credsStore`/`credHelpers`) are reported, never executed.
fn docker_config_credentials(
    registry: &str,
    path: &Path,
    bytes: &[u8],
) -> Result<Option<Credentials>, EngineError> {
    #[derive(Deserialize, Default)]
    struct Config {
        #[serde(default)]
        auths: BTreeMap<String, AuthEntry>,
        #[serde(rename = "credsStore", default)]
        creds_store: Option<String>,
        #[serde(rename = "credHelpers", default)]
        cred_helpers: BTreeMap<String, String>,
    }
    #[derive(Deserialize, Default)]
    struct AuthEntry {
        #[serde(default)]
        auth: Option<String>,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
        #[serde(rename = "identitytoken", default)]
        identity_token: Option<String>,
    }
    let config: Config = serde_json::from_slice(bytes)
        .map_err(|e| EngineError::Auth(format!("{} is not valid JSON: {e}", path.display())))?;
    let keys: Vec<String> = if is_docker_hub(registry) {
        vec![
            "https://index.docker.io/v1/".into(),
            "index.docker.io".into(),
            "docker.io".into(),
            "registry-1.docker.io".into(),
        ]
    } else {
        vec![
            registry.to_string(),
            format!("https://{registry}"),
            format!("http://{registry}"),
        ]
    };
    let origin = format!("inline auths in {}", path.display());
    for key in &keys {
        let Some(entry) = config.auths.get(key) else {
            continue;
        };
        if let Some(encoded) = entry.auth.as_deref().filter(|a| !a.is_empty()) {
            let decoded =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded.trim())
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok())
                    .and_then(|s| split_user_pass(&s))
                    .ok_or_else(|| {
                        EngineError::Auth(format!(
                            "registry {registry}: the `auth` entry in {} is malformed",
                            path.display()
                        ))
                    })?;
            return Ok(Some(Credentials {
                username: decoded.0,
                password: SecretString::new(decoded.1),
                origin,
            }));
        }
        if let (Some(u), Some(p)) = (&entry.username, &entry.password) {
            return Ok(Some(Credentials {
                username: u.clone(),
                password: SecretString::new(p.clone()),
                origin,
            }));
        }
        if entry.identity_token.is_some() {
            return Err(EngineError::Auth(format!(
                "registry {registry}: {} holds an OAuth identity token, which awman does not \
                 use; configure `env` or `keychain` registry auth instead",
                path.display()
            )));
        }
    }
    let helper = keys
        .iter()
        .find_map(|k| config.cred_helpers.get(k))
        .or(config.creds_store.as_ref());
    match helper {
        Some(h) => Err(EngineError::Auth(format!(
            "registry {registry}: {} delegates to the credential helper \
             `docker-credential-{h}`; awman never executes credential helpers. Configure \
             `{{\"type\":\"env\",…}}` or `{{\"type\":\"keychain\",…}}` auth for this registry \
             instead",
            path.display()
        ))),
        None => Ok(None),
    }
}

// ── Client ─────────────────────────────────────────────────────────────────

/// Proxy settings, read through awman's environment layer by the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ProxySettings {
    pub https: Option<String>,
    pub http: Option<String>,
    pub no_proxy: Option<String>,
}

impl ProxySettings {
    pub(super) fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Self {
        let first = |names: &[&str]| {
            names
                .iter()
                .find_map(|n| lookup(n).filter(|v| !v.trim().is_empty()))
        };
        let all = first(&["ALL_PROXY", "all_proxy"]);
        Self {
            https: first(&["HTTPS_PROXY", "https_proxy"]).or_else(|| all.clone()),
            http: first(&["HTTP_PROXY", "http_proxy"]).or(all),
            no_proxy: first(&["NO_PROXY", "no_proxy"]),
        }
    }

    fn apply(
        &self,
        mut builder: reqwest::blocking::ClientBuilder,
    ) -> Result<reqwest::blocking::ClientBuilder, EngineError> {
        // Never let the HTTP library consult the process environment itself.
        builder = builder.no_proxy();
        let no_proxy = self
            .no_proxy
            .as_deref()
            .and_then(reqwest::NoProxy::from_string);
        let bad = |_| EngineError::Config("the configured HTTP(S) proxy URL is invalid".into());
        if let Some(url) = &self.https {
            builder = builder.proxy(
                reqwest::Proxy::https(url.as_str())
                    .map_err(bad)?
                    .no_proxy(no_proxy.clone()),
            );
        }
        if let Some(url) = &self.http {
            builder = builder.proxy(
                reqwest::Proxy::http(url.as_str())
                    .map_err(bad)?
                    .no_proxy(no_proxy),
            );
        }
        Ok(builder)
    }
}

struct Session<'a> {
    client: reqwest::blocking::Client,
    base: String,
    reference: &'a ImageReference,
    credentials: Option<Credentials>,
    insecure: bool,
    token: Option<SecretString>,
    basic: bool,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

impl<'a> Session<'a> {
    fn new(
        reference: &'a ImageReference,
        host: Option<&RegistryHostConfig>,
        credentials: Option<Credentials>,
        proxy: &ProxySettings,
    ) -> Result<Self, EngineError> {
        let insecure = host.is_some_and(|h| h.insecure);
        let mut builder = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(None::<Duration>)
            .user_agent(concat!("awman/", env!("CARGO_PKG_VERSION")));
        builder = proxy.apply(builder)?;
        if let Some(ca) = host.and_then(|h| h.ca_cert.as_ref()) {
            let pem = std::fs::read(ca).map_err(|e| EngineError::io(ca, e))?;
            let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| {
                EngineError::Config(format!("CA bundle {} is not valid PEM: {e}", ca.display()))
            })?;
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }
        let client = builder
            .build()
            .map_err(|e| EngineError::Network(format!("HTTP client setup failed: {e}")))?;
        let scheme = if insecure { "http" } else { "https" };
        Ok(Self {
            client,
            base: format!(
                "{scheme}://{}/v2/{}",
                reference.api_host(),
                reference.repository
            ),
            reference,
            credentials,
            insecure,
            token: None,
            basic: false,
        })
    }

    /// GET `url` with the current authorization, answering one auth
    /// challenge.
    fn get(
        &mut self,
        url: &str,
        accept: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<reqwest::blocking::Response, EngineError> {
        for attempt in 0..2 {
            let mut req = self.client.get(url);
            if let Some(a) = accept {
                req = req.header(reqwest::header::ACCEPT, a);
            }
            if let Some(t) = timeout {
                req = req.timeout(t);
            }
            if let Some(token) = &self.token {
                req = req.bearer_auth(token.expose());
            } else if let (true, Some(c)) = (self.basic, &self.credentials) {
                req = req.basic_auth(&c.username, Some(c.password.expose()));
            }
            let resp = req.send().map_err(|e| self.network_error(e))?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                if attempt == 0 {
                    let challenge = resp
                        .headers()
                        .get(reqwest::header::WWW_AUTHENTICATE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    self.authorize(&challenge)?;
                    continue;
                }
                return Err(self.rejected());
            }
            if resp.status() == reqwest::StatusCode::FORBIDDEN {
                return Err(self.rejected());
            }
            return Ok(resp);
        }
        Err(self.rejected())
    }

    fn rejected(&self) -> EngineError {
        EngineError::Auth(match &self.credentials {
            Some(c) => format!(
                "registry {} denied access to {} with the credentials from {}",
                self.reference.registry, self.reference.repository, c.origin
            ),
            None => format!(
                "registry {} requires authentication for {}; configure \
                 `builtin.registries.\"{}\".auth`",
                self.reference.registry, self.reference.repository, self.reference.registry
            ),
        })
    }

    fn network_error(&self, e: reqwest::Error) -> EngineError {
        // `reqwest::Error` renders the URL, never request headers.
        EngineError::Network(format!(
            "registry {}: {}",
            self.reference.registry,
            e.without_url()
        ))
    }

    fn authorize(&mut self, challenge: &str) -> Result<(), EngineError> {
        let (scheme, params) = parse_challenge(challenge);
        match scheme.to_ascii_lowercase().as_str() {
            "basic" => {
                if self.credentials.is_none() {
                    return Err(self.rejected());
                }
                self.basic = true;
                Ok(())
            }
            "bearer" => {
                let realm = params.get("realm").ok_or_else(|| {
                    EngineError::Auth(format!(
                        "registry {} sent a Bearer challenge without a realm",
                        self.reference.registry
                    ))
                })?;
                let mut url = reqwest::Url::parse(realm).map_err(|_| {
                    EngineError::Auth(format!(
                        "registry {} sent an invalid token realm",
                        self.reference.registry
                    ))
                })?;
                // Credentials only travel to a token service the registry
                // itself controls: the realm must be on the registry's own
                // origin (or a well-known pairing such as Docker Hub's
                // `auth.docker.io`). Any other realm is asked anonymously,
                // so a hostile challenge cannot harvest the configured
                // secret. Plain HTTP is only accepted for a registry the
                // user explicitly marked insecure.
                let trusted = realm_trusted(&url, self.reference.api_host(), self.insecure);
                if url.scheme() != "https" && !(self.insecure && url.scheme() == "http") {
                    return Err(EngineError::Auth(format!(
                        "registry {} asked for a token over plain HTTP from {}; refusing",
                        self.reference.registry,
                        url.host_str().unwrap_or("?")
                    )));
                }
                {
                    let mut q = url.query_pairs_mut();
                    if let Some(service) = params.get("service") {
                        q.append_pair("service", service);
                    }
                    let scope = params.get("scope").cloned().unwrap_or_else(|| {
                        format!("repository:{}:pull", self.reference.repository)
                    });
                    q.append_pair("scope", &scope);
                }
                let mut req = self.client.get(url).timeout(METADATA_TIMEOUT);
                if let (true, Some(c)) = (trusted, &self.credentials) {
                    req = req.basic_auth(&c.username, Some(c.password.expose()));
                }
                let resp = req.send().map_err(|e| self.network_error(e))?;
                if !resp.status().is_success() {
                    return Err(self.rejected());
                }
                let body = read_capped(resp, MAX_TOKEN_BYTES).map_err(|e| {
                    EngineError::Network(format!(
                        "registry {} token response: {e}",
                        self.reference.registry
                    ))
                })?;
                let parsed: TokenResponse = serde_json::from_slice(&body).map_err(|_| {
                    EngineError::Auth(format!(
                        "registry {} returned an unreadable token response",
                        self.reference.registry
                    ))
                })?;
                let token = parsed
                    .token
                    .or(parsed.access_token)
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| self.rejected())?;
                self.token = Some(SecretString::new(token));
                Ok(())
            }
            _ => Err(EngineError::Auth(format!(
                "registry {} uses an unsupported authentication scheme `{scheme}`",
                self.reference.registry
            ))),
        }
    }

    /// Fetch and verify a manifest (or index). Returns `(bytes, media type,
    /// digest)`.
    fn manifest(&mut self, reference: &str) -> Result<(Vec<u8>, String, Digest), EngineError> {
        let accept = [OCI_INDEX, OCI_MANIFEST, DOCKER_LIST, DOCKER_MANIFEST].join(", ");
        let url = format!("{}/manifests/{reference}", self.base);
        let resp = self.get(&url, Some(&accept), Some(METADATA_TIMEOUT))?;
        let shown = self.reference.to_string();
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(EngineError::Container(format!(
                "image {shown} was not found in registry {}",
                self.reference.registry
            )));
        }
        if !resp.status().is_success() {
            return Err(EngineError::Network(format!(
                "registry {} answered {} for the manifest of {shown}",
                self.reference.registry,
                resp.status()
            )));
        }
        let media_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or("").trim().to_string())
            .unwrap_or_default();
        let header_digest = resp
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = read_capped(resp, MAX_MANIFEST_BYTES)
            .map_err(|e| EngineError::Network(format!("manifest of {shown}: {e}")))?;
        let digest = sha256_digest(&body);
        if let Ok(expected) = Digest::parse(reference) {
            if expected != digest {
                return Err(EngineError::ImageDigestMismatch {
                    reference: shown,
                    expected: expected.to_string(),
                    actual: digest.to_string(),
                });
            }
        }
        if let Some(h) = header_digest {
            if h.starts_with("sha256:") && h != digest.as_str() {
                return Err(EngineError::ImageDigestMismatch {
                    reference: shown,
                    expected: h,
                    actual: digest.to_string(),
                });
            }
        }
        // Fall back to the body's own mediaType when the header is generic.
        let media_type = if [OCI_INDEX, OCI_MANIFEST, DOCKER_LIST, DOCKER_MANIFEST]
            .contains(&media_type.as_str())
        {
            media_type
        } else {
            #[derive(Deserialize)]
            struct Probe {
                #[serde(rename = "mediaType", default)]
                media_type: Option<String>,
                #[serde(default)]
                manifests: Option<serde_json::Value>,
            }
            let probe: Probe = serde_json::from_slice(&body).map_err(|e| {
                EngineError::Network(format!("manifest of {shown} is not JSON: {e}"))
            })?;
            match (probe.media_type, probe.manifests) {
                (Some(mt), _) => mt,
                (None, Some(_)) => OCI_INDEX.to_string(),
                (None, None) => OCI_MANIFEST.to_string(),
            }
        };
        Ok((body, media_type, digest))
    }

    /// Stream blob `digest` into `path`, verifying size and digest.
    fn blob_to_file(
        &mut self,
        digest: &Digest,
        size: u64,
        path: &Path,
        max_bytes: u64,
        report: Report<'_>,
    ) -> Result<(), EngineError> {
        if size > max_bytes {
            return Err(EngineError::ImageArchiveRejected {
                path: path.to_path_buf(),
                reason: format!("blob {digest} is {size} bytes; the limit is {max_bytes}"),
            });
        }
        let url = format!("{}/blobs/{digest}", self.base);
        let resp = self.get(&url, None, None)?;
        if !resp.status().is_success() {
            return Err(EngineError::Network(format!(
                "registry {} answered {} for blob {digest}",
                self.reference.registry,
                resp.status()
            )));
        }
        let mut file = crate::engine::oci::verify::create_private_file(path)?;
        // Allow one extra byte so an over-long body is detected, not cut.
        let mut reader = HashingReader::new(resp, size + 1);
        let mut buf = vec![0u8; crate::engine::oci::verify::CHUNK];
        let mut last = 0u64;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if is_limit_exceeded(&e) => {
                    return Err(EngineError::ImageDigestMismatch {
                        reference: self.reference.to_string(),
                        expected: format!("{digest} ({size} bytes)"),
                        actual: "a longer blob".into(),
                    })
                }
                Err(e) => {
                    return Err(EngineError::Network(format!(
                        "blob {digest} transfer failed after {} of {size} bytes: {e}",
                        reader.count()
                    )))
                }
            };
            std::io::Write::write_all(&mut file, &buf[..n])
                .map_err(|e| EngineError::io(path, e))?;
            if reader.count() - last >= 4 * 1024 * 1024 {
                last = reader.count();
                report(AcquireProgress::Layer {
                    digest: digest.clone(),
                    done: last,
                    total: Some(size),
                });
            }
        }
        file.sync_all().map_err(|e| EngineError::io(path, e))?;
        let count = reader.count();
        let (hex, _) = reader.finish().map_err(|e| EngineError::io(path, e))?;
        if count != size {
            return Err(EngineError::Network(format!(
                "blob {digest} was truncated: {count} of {size} bytes"
            )));
        }
        if hex != digest_hex(digest) {
            return Err(EngineError::ImageDigestMismatch {
                reference: self.reference.to_string(),
                expected: digest.to_string(),
                actual: format!("sha256:{hex}"),
            });
        }
        report(AcquireProgress::Layer {
            digest: digest.clone(),
            done: size,
            total: Some(size),
        });
        Ok(())
    }
}

fn read_capped(resp: reqwest::blocking::Response, cap: u64) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    resp.take(cap + 1)
        .read_to_end(&mut body)
        .map_err(|e| e.to_string())?;
    if body.len() as u64 > cap {
        return Err(format!("response exceeds {cap} bytes"));
    }
    Ok(body)
}

/// `Bearer realm="…",service="…"` → ("Bearer", {realm, service}).
fn parse_challenge(header: &str) -> (String, BTreeMap<String, String>) {
    let header = header.trim();
    let (scheme, rest) = header.split_once(' ').unwrap_or((header, ""));
    let mut params = BTreeMap::new();
    let mut chars = rest.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| *c == ',' || c.is_whitespace()) {
            chars.next();
        }
        let key: String = chars.by_ref().take_while(|c| *c != '=').collect();
        if key.is_empty() {
            break;
        }
        let value = if chars.peek() == Some(&'"') {
            chars.next();
            let mut v = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            v.push(n)
                        }
                    }
                    '"' => break,
                    c => v.push(c),
                }
            }
            v
        } else {
            chars.by_ref().take_while(|c| *c != ',').collect()
        };
        params.insert(key.trim().to_ascii_lowercase(), value);
    }
    (scheme.to_string(), params)
}

// ── Pull ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Desc {
    digest: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    platform: Option<DescPlatform>,
}

#[derive(Deserialize)]
struct DescPlatform {
    os: String,
    architecture: String,
    #[serde(default)]
    variant: Option<String>,
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<Desc>,
}

#[derive(Deserialize)]
struct Manifest {
    config: Desc,
    layers: Vec<Desc>,
}

#[derive(Deserialize)]
struct ConfigPlatform {
    os: String,
    architecture: String,
    #[serde(default)]
    variant: Option<String>,
}

/// Everything the pull needs besides the reference.
pub(super) struct PullInputs<'a> {
    pub host: Option<&'a RegistryHostConfig>,
    pub credentials: Option<Credentials>,
    pub proxy: ProxySettings,
}

/// Pull `reference` for `ctx.platform` and write an OCI layout archive into
/// the staging area.
pub(super) fn pull(
    reference: &ImageReference,
    awman_tag: &str,
    inputs: PullInputs<'_>,
    ctx: &FetchContext<'_>,
    report: Report<'_>,
) -> Result<Fetched, EngineError> {
    let mut session = Session::new(reference, inputs.host, inputs.credentials, &inputs.proxy)?;
    let shown = reference.to_string();

    let (mut bytes, mut media_type, mut digest) = session.manifest(&reference.manifest_ref())?;
    if media_type == OCI_INDEX || media_type == DOCKER_LIST {
        let index: Index = serde_json::from_slice(&bytes).map_err(|e| {
            EngineError::Network(format!("image index of {shown} is malformed: {e}"))
        })?;
        let mut found = Vec::new();
        let chosen = index.manifests.iter().find(|d| {
            let Some(p) = &d.platform else { return false };
            let platform = OciPlatform {
                os: p.os.clone(),
                architecture: p.architecture.clone(),
                variant: p.variant.clone(),
            };
            if p.os != "unknown" {
                found.push(platform.to_string());
            }
            ctx.platform.matches(&platform)
        });
        let Some(chosen) = chosen else {
            found.sort();
            found.dedup();
            return Err(EngineError::ImagePlatformMismatch {
                reference: shown,
                wanted: ctx.platform.to_string(),
                found: found.join(", "),
            });
        };
        let wanted = Digest::parse(&chosen.digest)
            .map_err(|e| EngineError::Network(format!("image index of {shown}: {e}")))?;
        (bytes, media_type, digest) = session.manifest(wanted.as_str())?;
        if digest != wanted {
            return Err(EngineError::ImageDigestMismatch {
                reference: shown,
                expected: wanted.to_string(),
                actual: digest.to_string(),
            });
        }
    }
    if media_type != OCI_MANIFEST && media_type != DOCKER_MANIFEST {
        return Err(EngineError::UnsupportedImageSource {
            source_kind: crate::data::config::image_source::ImageSourceKind::Registry,
            reason: format!("{shown} has unsupported manifest type `{media_type}`"),
        });
    }
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|e| EngineError::Network(format!("manifest of {shown} is malformed: {e}")))?;
    if manifest.layers.len() > ctx.limits.max_layers {
        return Err(EngineError::ImageArchiveRejected {
            path: PathBuf::from(&shown),
            reason: format!(
                "image has {} layers; the limit is {}",
                manifest.layers.len(),
                ctx.limits.max_layers
            ),
        });
    }

    let blobs_dir = ctx.staging_dir.join("blobs");
    crate::engine::oci::verify::create_private_dir(&blobs_dir)?;

    // Config first: it decides the platform before any layer is fetched.
    let config_digest = Digest::parse(&manifest.config.digest)
        .map_err(|e| EngineError::Network(format!("manifest of {shown}: {e}")))?;
    let config_path = blobs_dir.join(digest_hex(&config_digest));
    session.blob_to_file(
        &config_digest,
        manifest.config.size,
        &config_path,
        MAX_MANIFEST_BYTES,
        report,
    )?;
    let config: ConfigPlatform = serde_json::from_slice(
        &std::fs::read(&config_path).map_err(|e| EngineError::io(&config_path, e))?,
    )
    .map_err(|e| EngineError::Network(format!("image config of {shown} is malformed: {e}")))?;
    let config_platform = OciPlatform {
        os: config.os,
        architecture: config.architecture,
        variant: config.variant,
    };
    if !ctx.platform.matches(&config_platform) {
        return Err(EngineError::ImagePlatformMismatch {
            reference: shown,
            wanted: ctx.platform.to_string(),
            found: config_platform.to_string(),
        });
    }

    // Blobs are staged once and copied into the archive: budget twice.
    // Sizes are untrusted: cap each one before summing so the budget can
    // neither overflow nor be understated.
    let mut total: u64 = 0;
    for layer in &manifest.layers {
        let reject = |reason: String| EngineError::ImageArchiveRejected {
            path: PathBuf::from(&shown),
            reason,
        };
        if layer.size > ctx.limits.max_layer_bytes {
            return Err(reject(format!(
                "layer {} declares {} bytes; the limit is {}",
                layer.digest, layer.size, ctx.limits.max_layer_bytes
            )));
        }
        total = total
            .checked_add(layer.size)
            .filter(|t| *t <= ctx.limits.max_archive_bytes)
            .ok_or_else(|| {
                reject(format!(
                    "declared layer sizes exceed the archive limit of {} bytes",
                    ctx.limits.max_archive_bytes
                ))
            })?;
    }
    ensure_space(
        ctx.disk,
        ctx.staging_dir,
        total.saturating_mul(2),
        ctx.limits.min_free_bytes,
    )?;
    let mut layer_digests = Vec::new();
    for layer in &manifest.layers {
        let d = Digest::parse(&layer.digest)
            .map_err(|e| EngineError::Network(format!("manifest of {shown}: {e}")))?;
        let path = blobs_dir.join(digest_hex(&d));
        if !path.exists() {
            session.blob_to_file(&d, layer.size, &path, ctx.limits.max_layer_bytes, report)?;
        }
        layer_digests.push(d);
    }

    // Assemble the OCI image layout.
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": OCI_INDEX,
        "manifests": [{
            "mediaType": media_type,
            "digest": digest.as_str(),
            "size": bytes.len(),
            "platform": {
                "os": config_platform.os,
                "architecture": config_platform.architecture,
            },
            "annotations": {
                "io.containerd.image.name": shown,
                "org.opencontainers.image.ref.name": awman_tag,
            },
        }],
    }))
    .map_err(|e| EngineError::Other(e.to_string()))?;
    let archive_path = ctx.staging_dir.join("archive.tar");
    let mut sink = StagingSink::create(
        archive_path.clone(),
        ctx.limits.max_archive_bytes,
        ctx.limits.min_free_bytes,
        ctx.disk,
    )?;
    let written = (|| -> std::io::Result<()> {
        let mut builder = tar::Builder::new(&mut sink);
        append_bytes(
            &mut builder,
            "oci-layout",
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )?;
        append_bytes(
            &mut builder,
            &format!("blobs/sha256/{}", digest_hex(&digest)),
            &bytes,
        )?;
        let mut done = std::collections::HashSet::new();
        for d in std::iter::once(&config_digest).chain(layer_digests.iter()) {
            if !done.insert(d.clone()) {
                continue;
            }
            let path = blobs_dir.join(digest_hex(d));
            let mut f = BufReader::new(File::open(&path)?);
            let len = std::fs::metadata(&path)?.len();
            let mut h = tar::Header::new_ustar();
            h.set_size(len);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            builder.append_data(&mut h, format!("blobs/sha256/{}", digest_hex(d)), &mut f)?;
        }
        append_bytes(&mut builder, "index.json", &index)?;
        builder.finish()
    })();
    if let Err(e) = written {
        return Err(sink
            .take_failure()
            .unwrap_or_else(|| EngineError::io(&archive_path, e)));
    }
    let (path, _) = sink.finish()?;
    Ok(Fetched {
        path,
        wanted_refs: vec![awman_tag.to_string(), shown],
        expected_manifest: Some(digest),
        fingerprint: None,
    })
}

fn append_bytes<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    name: &str,
    data: &[u8],
) -> std::io::Result<()> {
    let mut h = tar::Header::new_ustar();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_entry_type(tar::EntryType::Regular);
    builder.append_data(&mut h, name, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_parse_with_hub_normalisation() {
        let r = ImageReference::parse("ubuntu", None).unwrap();
        assert_eq!(
            (r.registry.as_str(), r.repository.as_str(), r.tag.as_deref()),
            ("docker.io", "library/ubuntu", Some("latest"))
        );
        assert_eq!(r.api_host(), "registry-1.docker.io");

        let r = ImageReference::parse("localhost:5000/team/agent:v1", None).unwrap();
        assert_eq!(
            (r.registry.as_str(), r.repository.as_str(), r.tag.as_deref()),
            ("localhost:5000", "team/agent", Some("v1"))
        );

        let d = format!("sha256:{}", "a".repeat(64));
        let r = ImageReference::parse(&format!("ghcr.io/o/i@{d}"), None).unwrap();
        assert_eq!(r.digest.unwrap().as_str(), d);
        assert!(r.tag.is_none());

        let r = ImageReference::parse("team/agent:1", Some("registry.local")).unwrap();
        assert_eq!(r.registry, "registry.local");
        assert_eq!(r.repository, "team/agent");
    }

    #[test]
    fn a_reference_cannot_redirect_to_another_registry() {
        assert!(ImageReference::parse("evil.example/x:1", Some("registry.local")).is_err());
        assert!(ImageReference::parse("docker.io/library/x", Some("index.docker.io")).is_ok());
    }

    #[test]
    fn malformed_references_are_rejected() {
        for bad in [
            "",
            "https://x/y",
            "Upper/case",
            "a b",
            "x:-tag",
            "x@sha256:zz",
            "u@h/x",
        ] {
            assert!(ImageReference::parse(bad, None).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn token_realms_receive_credentials_only_on_the_registry_origin() {
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        assert!(realm_trusted(
            &url("https://reg.example/token"),
            "reg.example",
            false
        ));
        assert!(realm_trusted(
            &url("https://REG.example:443/t"),
            "reg.example",
            false
        ));
        assert!(realm_trusted(
            &url("http://127.0.0.1:5000/t"),
            "127.0.0.1:5000",
            true
        ));
        assert!(realm_trusted(
            &url("https://auth.docker.io/token"),
            "registry-1.docker.io",
            false
        ));
        assert!(!realm_trusted(
            &url("https://attacker.example/token"),
            "reg.example",
            false
        ));
        assert!(!realm_trusted(
            &url("https://reg.example:8443/t"),
            "reg.example",
            false
        ));
        assert!(!realm_trusted(
            &url("https://auth.docker.io/token"),
            "reg.example",
            false
        ));
        assert!(!realm_trusted(
            &url("http://127.0.0.1:6000/t"),
            "127.0.0.1:5000",
            true
        ));
    }

    #[test]
    fn challenges_parse() {
        let (scheme, p) = parse_challenge(
            r#"Bearer realm="https://auth.example/token",service="registry.example",scope="repository:a/b:pull""#,
        );
        assert_eq!(scheme, "Bearer");
        assert_eq!(p["realm"], "https://auth.example/token");
        assert_eq!(p["service"], "registry.example");
        assert_eq!(p["scope"], "repository:a/b:pull");
        let (scheme, p) = parse_challenge(r#"Basic realm="x""#);
        assert_eq!(scheme, "Basic");
        assert_eq!(p["realm"], "x");
    }

    #[test]
    fn env_credentials_are_read_by_name_and_never_rendered() {
        let lookup = |n: &str| match n {
            "U" => Some("user".to_string()),
            "P" => Some("hunter2".to_string()),
            _ => None,
        };
        let auth = RegistryAuthSource::Env {
            username_var: "U".into(),
            password_var: "P".into(),
        };
        let creds = resolve_credentials("r", Some(&auth), &lookup, &|| None)
            .unwrap()
            .unwrap();
        assert_eq!(creds.username, "user");
        assert_eq!(creds.password.expose(), "hunter2");
        assert!(!format!("{creds:?}").contains("hunter2"));

        let missing = RegistryAuthSource::Env {
            username_var: "U".into(),
            password_var: "NOPE".into(),
        };
        let err = resolve_credentials("r", Some(&missing), &lookup, &|| None).unwrap_err();
        assert!(matches!(err, EngineError::Auth(ref m) if m.contains("NOPE")));
    }

    #[test]
    fn inline_config_auth_is_used_and_helpers_are_never_run() {
        let auth = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "u:s3cret");
        let inline = format!(r#"{{"auths":{{"registry.local":{{"auth":"{auth}"}}}}}}"#);
        let creds =
            docker_config_credentials("registry.local", Path::new("/c.json"), inline.as_bytes())
                .unwrap()
                .unwrap();
        assert_eq!(creds.username, "u");
        assert_eq!(creds.password.expose(), "s3cret");

        let helper = br#"{"auths":{},"credsStore":"desktop"}"#;
        match docker_config_credentials("registry.local", Path::new("/c.json"), helper) {
            Err(EngineError::Auth(msg)) => {
                assert!(msg.contains("docker-credential-desktop"));
                assert!(msg.contains("never executes"));
            }
            other => panic!("expected Auth error, got {other:?}"),
        }
        let none = br#"{"auths":{}}"#;
        assert!(
            docker_config_credentials("registry.local", Path::new("/c.json"), none)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn proxies_come_only_from_the_supplied_lookup() {
        let lookup = |n: &str| match n {
            "https_proxy" => Some("http://proxy:3128".to_string()),
            "NO_PROXY" => Some("localhost,.internal".to_string()),
            _ => None,
        };
        let p = ProxySettings::from_lookup(&lookup);
        assert_eq!(p.https.as_deref(), Some("http://proxy:3128"));
        assert_eq!(p.http, None);
        assert_eq!(p.no_proxy.as_deref(), Some("localhost,.internal"));
        assert_eq!(
            ProxySettings::from_lookup(&|_| None),
            ProxySettings::default()
        );
    }

    // ── Hermetic pulls against a wiremock registry ─────────────────────────

    mod wire {
        use std::collections::BTreeMap;
        use std::sync::Arc;

        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use crate::data::config::env::EnvSnapshot;
        use crate::data::config::image_source::{
            ImageSourceKind, ImageSourceSpec, RegistryAuthSource, RegistryHostConfig,
        };
        use crate::data::oci_identity::OciPlatform;
        use crate::engine::error::EngineError;
        use crate::engine::oci::archive::tests::{arm64, config_json, gzip, layer_tar, limits, L};
        use crate::engine::oci::verify::sha256_hex;
        use crate::engine::oci::verify::tests::FixedDisk;
        use crate::engine::oci::{
            AcquirePolicy, AcquireRequest, ArchiveFormat, CachingAcquirer, ImageAcquirer,
        };

        struct Image {
            index: Vec<u8>,
            manifest: Vec<u8>,
            manifest_hex: String,
            config: Vec<u8>,
            config_hex: String,
            layer: Vec<u8>,
            layer_hex: String,
        }

        fn image(index_platform: &OciPlatform) -> Image {
            let raw = layer_tar(&[L::File("bin/agent", b"#!/bin/sh\n", 0o755)]);
            let layer = gzip(&raw);
            let layer_hex = sha256_hex(&layer);
            let config = config_json(&arm64(), &[format!("sha256:{}", sha256_hex(&raw))]);
            let config_hex = sha256_hex(&config);
            let manifest = serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2, "mediaType": super::OCI_MANIFEST,
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                           "digest": format!("sha256:{config_hex}"), "size": config.len()},
                "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                            "digest": format!("sha256:{layer_hex}"), "size": layer.len()}],
            }))
            .unwrap();
            let manifest_hex = sha256_hex(&manifest);
            let index = serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2, "mediaType": super::OCI_INDEX,
                "manifests": [
                    {"mediaType": super::OCI_MANIFEST, "digest": format!("sha256:{}", "f".repeat(64)),
                     "size": 1, "platform": {"os": "linux", "architecture": "s390x"}},
                    {"mediaType": super::OCI_MANIFEST, "digest": format!("sha256:{manifest_hex}"),
                     "size": manifest.len(),
                     "platform": {"os": index_platform.os, "architecture": index_platform.architecture}},
                ],
            }))
            .unwrap();
            Image {
                index,
                manifest,
                manifest_hex,
                config,
                config_hex,
                layer,
                layer_hex,
            }
        }

        async fn serve(server: &MockServer, img: &Image, layer_body: Vec<u8>) {
            serve_with_realm(server, img, layer_body, &format!("{}/token", server.uri())).await;
        }

        async fn serve_with_realm(
            server: &MockServer,
            img: &Image,
            layer_body: Vec<u8>,
            token_realm: &str,
        ) {
            let base = "/v2/team/agent";
            // Anything without the bearer token is challenged.
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(401).insert_header(
                    "www-authenticate",
                    format!(r#"Bearer realm="{token_realm}",service="test""#).as_str(),
                ))
                .with_priority(10)
                .mount(server)
                .await;
            let basic = format!(
                "Basic {}",
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "robot:pw")
            );
            Mock::given(method("GET"))
                .and(path("/token"))
                .and(header("authorization", basic.as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
                .with_priority(1)
                .mount(server)
                .await;
            let authed = |p: String| {
                Mock::given(method("GET"))
                    .and(path(p))
                    .and(header("authorization", "Bearer tok"))
            };
            authed(format!("{base}/manifests/1"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(img.index.clone(), super::OCI_INDEX),
                )
                .with_priority(1)
                .mount(server)
                .await;
            authed(format!("{base}/manifests/sha256:{}", img.manifest_hex))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(img.manifest.clone(), super::OCI_MANIFEST),
                )
                .with_priority(1)
                .mount(server)
                .await;
            authed(format!("{base}/blobs/sha256:{}", img.config_hex))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(img.config.clone()))
                .with_priority(1)
                .mount(server)
                .await;
            authed(format!("{base}/blobs/sha256:{}", img.layer_hex))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(layer_body))
                .with_priority(1)
                .mount(server)
                .await;
        }

        fn request(server: &MockServer, service: &str) -> AcquireRequest {
            let host = server.address().to_string();
            crate::engine::auth::keychain::keychain_store(
                service,
                &host,
                "robot:pw",
                std::time::Duration::from_secs(1),
            )
            .unwrap();
            AcquireRequest {
                tag: "awman-x-claude:latest".into(),
                source: ImageSourceSpec::Registry {
                    registry: Some(host.clone()),
                    reference: Some("team/agent:1".into()),
                },
                platform: arm64(),
                policy: AcquirePolicy::IfMissing,
                registries: BTreeMap::from([(
                    host,
                    RegistryHostConfig {
                        insecure: true,
                        ca_cert: None,
                        auth: Some(RegistryAuthSource::Keychain {
                            service: service.into(),
                        }),
                    },
                )]),
            }
        }

        fn acquirer(state: &std::path::Path) -> CachingAcquirer {
            CachingAcquirer::new(
                state,
                limits(),
                EnvSnapshot::empty(),
                Arc::new(FixedDisk(None)),
            )
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn pull_authenticates_selects_platform_verifies_and_caches() {
            let server = MockServer::start().await;
            let img = image(&arm64());
            serve(&server, &img, img.layer.clone()).await;
            let state = tempfile::tempdir().unwrap();
            let req = request(&server, "awman-oci-test-ok");
            let st = state.path().to_path_buf();
            let r = req.clone();
            let got = tokio::task::spawn_blocking(move || acquirer(&st).acquire(&r, &mut |_| {}))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(got.identity.source, ImageSourceKind::Registry);
            assert_eq!(
                got.identity.manifest_digest.as_str(),
                format!("sha256:{}", img.manifest_hex)
            );
            assert_eq!(got.archive_format, ArchiveFormat::OciLayout);

            // Offline reuse: the registry is gone, the cache still answers.
            drop(server);
            let st = state.path().to_path_buf();
            let mut cached = req.clone();
            cached.policy = AcquirePolicy::CachedOnly;
            let again =
                tokio::task::spawn_blocking(move || acquirer(&st).acquire(&cached, &mut |_| {}))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(again, got);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn tampered_layer_is_a_digest_mismatch_and_caches_nothing() {
            let server = MockServer::start().await;
            let img = image(&arm64());
            let mut bad = img.layer.clone();
            let last = bad.len() - 1;
            bad[last] ^= 0xff;
            serve(&server, &img, bad).await;
            let state = tempfile::tempdir().unwrap();
            let req = request(&server, "awman-oci-test-tamper");
            let st = state.path().to_path_buf();
            let err = tokio::task::spawn_blocking(move || acquirer(&st).acquire(&req, &mut |_| {}))
                .await
                .unwrap()
                .unwrap_err();
            assert!(
                matches!(err, EngineError::ImageDigestMismatch { .. }),
                "{err:?}"
            );
            let images = state.path().join("oci-cache/images");
            assert_eq!(std::fs::read_dir(images).unwrap().count(), 0);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn index_without_the_native_platform_is_a_platform_mismatch() {
            let server = MockServer::start().await;
            let img = image(&crate::engine::oci::archive::tests::amd64());
            serve(&server, &img, img.layer.clone()).await;
            let state = tempfile::tempdir().unwrap();
            let req = request(&server, "awman-oci-test-platform");
            let st = state.path().to_path_buf();
            let err = tokio::task::spawn_blocking(move || acquirer(&st).acquire(&req, &mut |_| {}))
                .await
                .unwrap()
                .unwrap_err();
            match err {
                EngineError::ImagePlatformMismatch { wanted, found, .. } => {
                    assert_eq!(wanted, "linux/arm64");
                    assert!(found.contains("linux/amd64"), "{found}");
                }
                other => panic!("expected platform mismatch, got {other:?}"),
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn wrong_credentials_are_an_auth_error_without_the_secret() {
            let server = MockServer::start().await;
            let img = image(&arm64());
            serve(&server, &img, img.layer.clone()).await;
            let state = tempfile::tempdir().unwrap();
            let mut req = request(&server, "awman-oci-test-badcreds");
            crate::engine::auth::keychain::keychain_store(
                "awman-oci-test-badcreds",
                &server.address().to_string(),
                "robot:wrong-secret",
                std::time::Duration::from_secs(1),
            )
            .unwrap();
            req.policy = AcquirePolicy::Refresh;
            let st = state.path().to_path_buf();
            let err = tokio::task::spawn_blocking(move || acquirer(&st).acquire(&req, &mut |_| {}))
                .await
                .unwrap()
                .unwrap_err();
            let text = err.to_string();
            assert!(matches!(err, EngineError::Auth(_)), "{err:?}");
            assert!(!text.contains("wrong-secret"), "{text}");
            assert!(text.contains("keychain service"), "{text}");
        }

        /// Regression (review finding: credential leak via token realm): a
        /// registry that points its Bearer realm at a foreign host must not
        /// receive the configured credentials there; the token is requested
        /// anonymously.
        #[tokio::test(flavor = "multi_thread")]
        async fn foreign_token_realm_never_receives_credentials() {
            let server = MockServer::start().await;
            let foreign = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/token"))
                .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
                .mount(&foreign)
                .await;
            let img = image(&arm64());
            let realm = format!("{}/token", foreign.uri());
            serve_with_realm(&server, &img, img.layer.clone(), &realm).await;
            let state = tempfile::tempdir().unwrap();
            let req = request(&server, "awman-oci-test-foreign-realm");
            let st = state.path().to_path_buf();
            let got = tokio::task::spawn_blocking(move || acquirer(&st).acquire(&req, &mut |_| {}))
                .await
                .unwrap();
            // The foreign realm was reached for a token but saw no secret.
            let seen = foreign.received_requests().await.unwrap();
            assert!(!seen.is_empty());
            for r in &seen {
                assert!(
                    !r.headers.contains_key("authorization"),
                    "credentials leaked to a foreign token realm"
                );
            }
            got.unwrap();
        }
    }
}
