//! Docker Engine source: export one image through the Engine API's
//! supported image-export endpoint (`GET /images/{name}/get`).
//!
//! The daemon is reached over its Unix socket, over `tcp://` with TLS
//! (optionally mutual TLS), or over plain `tcp://` to a loopback address
//! only. `ssh://` and Windows named pipes are refused with a precise reason.
//! Docker's private storage directories are never read.
//!
//! The daemon is needed only while acquiring. A cached image is never
//! re-validated against it, and a missing daemon never blocks running one.
//!
//! This module performs blocking I/O and must be called off any async
//! runtime (the acquirer runs it on a dedicated thread).

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use super::retry::OperationControl;
use super::transport::{Response, Transport};
use crate::data::config::env::EnvSnapshot;
use crate::data::config::image_source::{DockerTlsConfig, ImageSourceKind};
use crate::data::oci_identity::OciPlatform;
use crate::engine::error::{EngineError, ImageRetryClass};
use crate::engine::oci::sources::{FetchContext, Fetched, Report};
use crate::engine::oci::verify::{ensure_space, StagingSink};

/// Oldest Engine API that has everything used here.
const MIN_API: (u32, u32) = (1, 41);
/// First Engine API whose export endpoint accepts `platform`.
const PLATFORM_API: (u32, u32) = (1, 48);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const METADATA_TIMEOUT: Duration = Duration::from_secs(60);
/// A running export that delivers no bytes for this long is treated as a
/// disconnect (the caller may retry it; the acquisition deadline still bounds
/// the whole transfer).
const EXPORT_STALL_TIMEOUT: Duration = Duration::from_secs(300);

#[cfg(test)]
#[path = "docker_engine_tests.rs"]
mod tests;

/// Where the Engine listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Unix(PathBuf),
    /// Keep the origin for diagnosis; the path still keys the same store.
    DefaultUnix(PathBuf),
    Tcp {
        host: String,
        port: u16,
        tls: Option<DockerTlsConfig>,
    },
}

impl Endpoint {
    /// Non-secret description, used in messages and as the cache locator.
    pub fn describe(&self) -> String {
        match self {
            Self::Unix(p) | Self::DefaultUnix(p) => format!("unix://{}", p.display()),
            Self::Tcp { host, port, tls } => {
                let scheme = if tls.is_some() { "tcp+tls" } else { "tcp" };
                format!("{scheme}://{host}:{port}")
            }
        }
    }

    pub(crate) fn unix_path(&self) -> Option<&Path> {
        match self {
            Self::Unix(path) | Self::DefaultUnix(path) => Some(path),
            Self::Tcp { .. } => None,
        }
    }
}

/// A final local source condition, retained inside `EngineError::Io` so
/// callers can distinguish it without interpreting a diagnostic string.
#[derive(Debug, thiserror::Error)]
pub enum LocalDockerUnavailable {
    #[error("{}", stopped_diagnostic(endpoint, *default_socket))]
    Stopped {
        endpoint: String,
        default_socket: bool,
    },
    #[error("Cannot access the Docker socket at {endpoint}. Check socket permissions and your Docker connection context (`host` / DOCKER_HOST), then run `awman ready` again.")]
    PermissionDenied { endpoint: String },
    #[error("The Docker endpoint {endpoint} is not a usable Docker socket. Check the source's `host` / DOCKER_HOST, then run `awman ready` again.")]
    InvalidSocket { endpoint: String },
}

fn stopped_diagnostic(endpoint: &str, default_socket: bool) -> String {
    if default_socket {
        format!("No Docker daemon at the default socket {endpoint}. Start Docker, or set the source's `host` (or DOCKER_HOST) if Docker uses another socket/context, then run `awman ready` again.")
    } else {
        format!("Docker is not running at {endpoint}. Start Docker, then run `awman ready` again.")
    }
}

/// Format the typed source diagnosis once, without the generic I/O wrapper.
/// This is presentation only; policy must use `local_unavailability` first.
pub fn failure_message(error: &EngineError) -> String {
    match local_unavailability(error) {
        Some(failure) => failure.to_string(),
        None => error.to_string(),
    }
}

/// Imports and builder CLIs use a frozen explicit host. Restore its original
/// default-socket diagnosis when preparation still has the resolved source.
pub(crate) fn failure_message_at(error: &EngineError, endpoint: Option<&Endpoint>) -> String {
    match (local_unavailability(error), endpoint) {
        (
            Some(LocalDockerUnavailable::Stopped { endpoint, .. }),
            Some(Endpoint::DefaultUnix(_)),
        ) => stopped_diagnostic(endpoint, true),
        _ => failure_message(error),
    }
}

/// Inspect the typed local failure after acquisition or a source-matched
/// builder probe. All local socket conditions are final and never authorize fallback.
pub fn local_unavailability(err: &EngineError) -> Option<&LocalDockerUnavailable> {
    match err {
        EngineError::Io { source, .. } => source.get_ref()?.downcast_ref(),
        _ => None,
    }
}

/// Reqwest/hyper wrap the socket's `io::Error`. Walk the source chain
/// (including an io wrapper's payload), never their display text. A refused
/// TCP connection is deliberately excluded, even when its host is loopback.
fn local_socket_failure(
    endpoint: &Endpoint,
    error: &(dyn std::error::Error + 'static),
) -> Option<EngineError> {
    let path = endpoint.unix_path()?;
    let mut cause = Some(error);
    while let Some(error) = cause {
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            let failure = match io.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                    #[cfg(unix)]
                    let not_socket = {
                        use std::os::unix::fs::FileTypeExt;
                        std::fs::metadata(path).is_ok_and(|m| !m.file_type().is_socket())
                    };
                    #[cfg(not(unix))]
                    let not_socket = false;
                    Some(if not_socket {
                        LocalDockerUnavailable::InvalidSocket {
                            endpoint: endpoint.describe(),
                        }
                    } else {
                        LocalDockerUnavailable::Stopped {
                            endpoint: endpoint.describe(),
                            default_socket: matches!(endpoint, Endpoint::DefaultUnix(_)),
                        }
                    })
                }
                std::io::ErrorKind::NotADirectory | std::io::ErrorKind::InvalidInput => {
                    Some(LocalDockerUnavailable::InvalidSocket {
                        endpoint: endpoint.describe(),
                    })
                }
                std::io::ErrorKind::PermissionDenied => {
                    Some(LocalDockerUnavailable::PermissionDenied {
                        endpoint: endpoint.describe(),
                    })
                }
                // A timeout is not evidence of a stopped daemon.
                std::io::ErrorKind::TimedOut => return None,
                _ => None,
            };
            if let Some(failure) = failure {
                return Some(EngineError::io(
                    path,
                    std::io::Error::new(io.kind(), failure),
                ));
            }
            if let Some(inner) = io.get_ref() {
                cause = Some(inner);
                continue;
            }
        }
        cause = error.source();
    }
    None
}

/// After a local builder CLI fails, use the socket itself for an authoritative
/// diagnosis. The CLI's stderr cannot supply a typed connection error. Other
/// failures retain the original CLI cause. This check starts no service and
/// respects the builder's cancellation and deadline, including a full backlog.
pub(crate) fn check_local_connection(
    endpoint: &str,
    control: &crate::engine::container::BuildControl,
) -> Result<(), EngineError> {
    control.check()?;
    #[cfg(not(unix))]
    let _ = endpoint;
    #[cfg(unix)]
    if let Some(path) = endpoint.strip_prefix("unix://") {
        let endpoint = Endpoint::Unix(PathBuf::from(path));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| EngineError::io(path, e))?;
        return runtime.block_on(async {
            let connect = tokio::net::UnixStream::connect(path);
            tokio::pin!(connect);
            loop {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep(Duration::from_millis(20)) => control.check()?,
                    result = &mut connect => {
                        control.check()?;
                        return match result {
                            Err(error) => match local_socket_failure(&endpoint, &error) {
                                Some(error) => Err(error),
                                None => Ok(()),
                            },
                            Ok(_) => Ok(()),
                        };
                    }
                }
            }
        });
    }
    Ok(())
}

fn unsupported(reason: String) -> EngineError {
    EngineError::UnsupportedImageSource {
        source_kind: ImageSourceKind::DockerStore,
        reason,
    }
}

/// Resolve the Engine endpoint: the source's `host`, else `DOCKER_HOST`,
/// else the platform's default socket. `tls` comes from the source, else
/// from `DOCKER_TLS_VERIFY`/`DOCKER_CERT_PATH` when `DOCKER_HOST` is used.
pub fn resolve_endpoint(
    host: Option<&str>,
    tls: Option<&DockerTlsConfig>,
    env: &EnvSnapshot,
) -> Result<Endpoint, EngineError> {
    let (raw, from_env) = match host {
        Some(h) => (h.trim().to_string(), false),
        None => match env.docker_host().filter(|h| !h.trim().is_empty()) {
            Some(h) => (h.trim().to_string(), true),
            None => return default_endpoint(env),
        },
    };
    let tls = match tls {
        Some(t) => Some(t.clone()),
        None if from_env && env.docker_tls_verify() => {
            let dir = env
                .docker_cert_path()
                .or_else(|| env.get("HOME").map(|h| PathBuf::from(h).join(".docker")))
                .ok_or_else(|| {
                    EngineError::Config(
                        "DOCKER_TLS_VERIFY is set but DOCKER_CERT_PATH is not, and there is no \
                         home directory"
                            .into(),
                    )
                })?;
            Some(DockerTlsConfig {
                ca: dir.join("ca.pem"),
                cert: Some(dir.join("cert.pem")),
                key: Some(dir.join("key.pem")),
                verify: true,
            })
        }
        None => None,
    };
    parse_host(&raw, tls)
}

fn parse_host(raw: &str, tls: Option<DockerTlsConfig>) -> Result<Endpoint, EngineError> {
    if let Some(path) = raw.strip_prefix("unix://") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(EngineError::Config(format!(
                "docker host `{raw}` must name an absolute socket path"
            )));
        }
        if tls.is_some() {
            return Err(EngineError::Config(
                "TLS settings apply only to tcp:// docker hosts".into(),
            ));
        }
        return Ok(Endpoint::Unix(path));
    }
    if let Some(rest) = raw.strip_prefix("tcp://") {
        let rest = rest.trim_end_matches('/');
        if rest.contains('@') {
            return Err(EngineError::Config(
                "docker host URLs must not embed credentials".into(),
            ));
        }
        let (host, port) = split_host_port(rest).ok_or_else(|| {
            EngineError::Config(format!("docker host `{raw}` must be tcp://host[:port]"))
        })?;
        let port = port.unwrap_or(if tls.is_some() { 2376 } else { 2375 });
        if tls.is_none() && !is_loopback(&host) {
            return Err(unsupported(format!(
                "plain tcp:// to the remote Docker Engine at {host}:{port} is refused; \
                 configure `tls` (the supported authenticated remote transport)"
            )));
        }
        return Ok(Endpoint::Tcp { host, port, tls });
    }
    if raw.starts_with("ssh://") {
        return Err(unsupported(
            "ssh:// Docker hosts are not supported (awman does not run an ssh client); \
             expose the Engine over tcp:// with TLS instead"
                .into(),
        ));
    }
    if raw.starts_with("npipe://") {
        return Err(unsupported(
            "Windows named-pipe Docker hosts are not supported by the builtin runtime".into(),
        ));
    }
    Err(EngineError::Config(format!(
        "docker host `{raw}` must start with unix:// or tcp://"
    )))
}

fn split_host_port(s: &str) -> Option<(String, Option<u16>)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p.parse().ok()?),
            None if after.is_empty() => None,
            None => return None,
        };
        return Some((h.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_string(), Some(p.parse().ok()?))),
        _ if !s.is_empty() && !s.contains('/') => Some((s.to_string(), None)),
        _ => None,
    }
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn default_endpoint(env: &EnvSnapshot) -> Result<Endpoint, EngineError> {
    // Under test isolation the machine's real Engine is off-limits unless
    // Docker tests are explicitly enabled.
    if crate::data::config::env::test_isolation_active() && !env.test_docker() {
        return Err(unsupported(
            "the default Docker Engine socket is disabled under test isolation; set an \
             explicit `host`"
                .into(),
        ));
    }
    let system = PathBuf::from("/var/run/docker.sock");
    if cfg!(target_os = "macos") {
        if let Some(user) = env
            .get("HOME")
            .map(|h| PathBuf::from(h).join(".docker/run/docker.sock"))
        {
            if user.exists() || !system.exists() {
                return Ok(Endpoint::DefaultUnix(user));
            }
        }
    }
    if cfg!(windows) {
        return Err(unsupported(
            "Windows named-pipe Docker hosts are not supported by the builtin runtime".into(),
        ));
    }
    Ok(Endpoint::DefaultUnix(system))
}

/// Refuse references that could be mistaken for API paths or queries.
pub(crate) fn validate_reference(reference: &str) -> Result<(), EngineError> {
    let ok = !reference.is_empty()
        && reference.len() <= 512
        && !reference.starts_with(['-', '/', '.'])
        && !reference.contains("..")
        && reference.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b':' | b'@')
        });
    if ok {
        Ok(())
    } else {
        Err(EngineError::Config(format!(
            "`{reference}` is not a valid Docker image reference"
        )))
    }
}

struct Client {
    http: reqwest::Client,
    transport: Transport,
    base: String,
    endpoint: String,
    selected_endpoint: Endpoint,
}

impl Client {
    fn new(endpoint: &Endpoint, control: OperationControl) -> Result<Self, EngineError> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(EXPORT_STALL_TIMEOUT)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("awman/", env!("CARGO_PKG_VERSION")));
        let base = match endpoint {
            #[cfg(unix)]
            Endpoint::Unix(path) | Endpoint::DefaultUnix(path) => {
                builder = builder.unix_socket(path.clone());
                "http://docker".to_string()
            }
            #[cfg(not(unix))]
            Endpoint::Unix(_) | Endpoint::DefaultUnix(_) => {
                return Err(unsupported(
                    "Unix-socket Docker hosts need a Unix platform".into(),
                ))
            }
            Endpoint::Tcp { host, port, tls } => {
                let host_part = if host.contains(':') {
                    format!("[{host}]")
                } else {
                    host.clone()
                };
                match tls {
                    None => format!("http://{host_part}:{port}"),
                    Some(tls) => {
                        builder = apply_tls(builder, tls)?;
                        format!("https://{host_part}:{port}")
                    }
                }
            }
        };
        let http = builder.build().map_err(|e| {
            ImageRetryClass::Permanent.failure(format!("HTTP client setup failed: {e}"))
        })?;
        Ok(Self {
            http,
            transport: Transport::new(control)?,
            base,
            endpoint: endpoint.describe(),
            selected_endpoint: endpoint.clone(),
        })
    }

    fn get(&self, path: &str, timeout: Duration) -> Result<Response, EngineError> {
        let req = self
            .http
            .get(format!("{}{path}", self.base))
            .timeout(timeout);
        self.transport.send(req).map_err(|e| {
            e.map_http(|e| {
                if !e.is_timeout() {
                    if let Some(error) = local_socket_failure(&self.selected_endpoint, &e) {
                        return error;
                    }
                }
                ImageRetryClass::http(&e).failure(format!(
                    "Docker Engine at {} is not reachable: {}",
                    self.endpoint,
                    crate::engine::oci::retry::error_chain(&e.without_url())
                ))
            })
        })
    }

    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        timeout: Duration,
    ) -> Result<(reqwest::StatusCode, Option<T>), EngineError> {
        let resp = self.get(path, timeout)?;
        let status = resp.status();
        if !resp.status().is_success() {
            return Ok((status, None));
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut std::io::Read::take(resp, 1024 * 1024 + 1), &mut bytes)
            .map_err(|e| {
                super::retry::map_diagnostic(super::retry::read_error(e), |diagnostic| {
                    format!("Docker metadata transfer failed: {diagnostic}")
                })
            })?;
        if bytes.len() > 1024 * 1024 {
            return Err(ImageRetryClass::Permanent.failure("Docker metadata exceeds 1 MiB".into()));
        }
        let parsed = serde_json::from_slice::<T>(&bytes).map_err(|e| {
            ImageRetryClass::Permanent.failure(format!(
                "Docker Engine at {} returned an unreadable response for {path}: {}",
                self.endpoint, e
            ))
        })?;
        Ok((status, Some(parsed)))
    }
}

fn apply_tls(
    mut builder: reqwest::ClientBuilder,
    tls: &DockerTlsConfig,
) -> Result<reqwest::ClientBuilder, EngineError> {
    let read = |p: &Path| std::fs::read(p).map_err(|e| EngineError::io(p, e));
    let ca = reqwest::Certificate::from_pem_bundle(&read(&tls.ca)?).map_err(|e| {
        EngineError::Config(format!(
            "Docker CA {} is not valid PEM: {e}",
            tls.ca.display()
        ))
    })?;
    builder = builder.tls_certs_only(ca);
    match (&tls.cert, &tls.key) {
        (Some(cert), Some(key)) => {
            let mut pem = read(cert)?;
            pem.push(b'\n');
            pem.extend(read(key)?);
            let identity = reqwest::Identity::from_pem(&pem).map_err(|e| {
                EngineError::Config(format!(
                    "Docker client certificate {} / key {} are not valid PEM: {e}",
                    cert.display(),
                    key.display()
                ))
            })?;
            builder = builder.identity(identity);
        }
        (None, None) => {}
        _ => {
            return Err(EngineError::Config(
                "Docker TLS needs both `cert` and `key` for a client certificate".into(),
            ))
        }
    }
    if !tls.verify {
        builder = builder.tls_danger_accept_invalid_certs(true);
    }
    Ok(builder)
}

#[derive(Deserialize)]
struct VersionInfo {
    #[serde(rename = "ApiVersion")]
    api_version: String,
}

#[derive(Deserialize)]
struct Inspect {
    #[serde(rename = "Os", default)]
    os: String,
    #[serde(rename = "Architecture", default)]
    architecture: String,
    #[serde(rename = "Variant", default)]
    variant: Option<String>,
    #[serde(rename = "Size", default)]
    size: u64,
}

fn parse_api(v: &str) -> Option<(u32, u32)> {
    let (a, b) = v.trim().split_once('.')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Export `reference` from the Engine at `endpoint` into the staging area.
pub(super) fn export(
    endpoint: &Endpoint,
    reference: &str,
    selector: &str,
    ctx: &FetchContext<'_>,
    _report: Report<'_>,
) -> Result<Fetched, EngineError> {
    validate_reference(reference)?;
    validate_reference(selector)?;
    ctx.cancel.check()?;
    let client = Client::new(
        endpoint,
        OperationControl::new(ctx.cancel.clone(), ctx.deadline),
    )?;
    let metadata = || ctx.deadline.request_timeout(Some(METADATA_TIMEOUT));

    let ping = client.get("/_ping", metadata())?;
    if !ping.status().is_success() {
        return Err(ImageRetryClass::status(ping.status()).failure(format!(
            "Docker Engine at {} answered {} to /_ping",
            client.endpoint,
            ping.status()
        )));
    }
    let (status, version) = client.get_json::<VersionInfo>("/version", metadata())?;
    if !status.is_success() {
        return Err(ImageRetryClass::status(status).failure(format!(
            "Docker Engine at {} answered {status} to /version",
            client.endpoint
        )));
    }
    let api = version
        .as_ref()
        .and_then(|v| parse_api(&v.api_version))
        .ok_or_else(|| {
            ImageRetryClass::Permanent.failure(format!(
                "Docker Engine at {} did not report its API version",
                client.endpoint
            ))
        })?;
    if api < MIN_API {
        return Err(unsupported(format!(
            "Docker Engine at {} speaks API {}.{}; {}.{} or newer is required",
            client.endpoint, api.0, api.1, MIN_API.0, MIN_API.1
        )));
    }
    let use_api = if api >= PLATFORM_API {
        PLATFORM_API
    } else {
        MIN_API
    };
    let prefix = format!("/v{}.{}", use_api.0, use_api.1);

    ctx.cancel.check()?;
    let (status, inspect) =
        client.get_json::<Inspect>(&format!("{prefix}/images/{selector}/json"), metadata())?;
    let Some(inspect) = inspect else {
        return Err(if status == reqwest::StatusCode::NOT_FOUND {
            EngineError::SourceImageNotFound {
                reference: reference.to_string(),
                location: format!("the Docker Engine at {}", client.endpoint),
            }
        } else {
            ImageRetryClass::status(status).failure(format!(
                "Docker Engine at {} answered {status} when inspecting '{reference}'",
                client.endpoint
            ))
        });
    };
    let found = OciPlatform {
        os: inspect.os,
        architecture: inspect.architecture,
        variant: inspect.variant.filter(|v| !v.is_empty()),
    };
    // The unqualified inspect describes the store's default variant. With a
    // platform-qualified export (API 1.48+) the host variant may still be
    // present, so leave the decision to the archive validator, which checks
    // the exported config's platform. Older engines export only the default
    // variant, so a mismatch here is final.
    if !ctx.platform.matches(&found) && use_api < PLATFORM_API {
        return Err(EngineError::ImagePlatformMismatch {
            reference: reference.to_string(),
            wanted: ctx.platform.to_string(),
            found: found.to_string(),
        });
    }
    // The export is roughly the image's uncompressed size.
    ensure_space(
        ctx.disk,
        ctx.staging_dir,
        inspect.size,
        ctx.limits.min_free_bytes,
    )?;

    let mut path = format!("{prefix}/images/{selector}/get");
    if use_api >= PLATFORM_API {
        let platform = serde_json::json!({
            "os": ctx.platform.os,
            "architecture": ctx.platform.architecture,
        })
        .to_string();
        let encoded: String =
            reqwest::Url::parse_with_params("http://x/", [("platform", platform)])
                .map(|u| u.query().unwrap_or("").to_string())
                .unwrap_or_default();
        path = format!("{path}?{encoded}");
    }
    ctx.cancel.check()?;
    // The export body is bounded by the acquisition deadline; a stall longer
    // than EXPORT_STALL_TIMEOUT within it is reported as a disconnect.
    let export_timeout = ctx
        .deadline
        .request_timeout(None)
        .max(Duration::from_millis(1));
    let resp = client.get(&path, export_timeout)?;
    if !resp.status().is_success() {
        return Err(ImageRetryClass::status(resp.status()).failure(format!(
            "Docker Engine at {} answered {} when exporting '{reference}'",
            client.endpoint,
            resp.status()
        )));
    }
    let declared = resp.content_length();
    let archive_path = ctx.staging_dir.join("archive.tar");
    let mut sink = StagingSink::create(
        archive_path,
        ctx.limits.max_archive_bytes,
        ctx.limits.min_free_bytes,
        ctx.disk,
    )?
    .cancellable(ctx.cancel)
    .with_deadline(ctx.deadline);
    let mut body = resp;
    sink.copy_from(&mut body).map_err(|e| {
        super::retry::map_diagnostic(e, |diagnostic| {
            format!(
                "Docker Engine at {} disconnected while exporting '{reference}': {diagnostic}",
                client.endpoint
            )
        })
    })?;
    if let Some(n) = declared {
        if n != sink.written() {
            return Err(ImageRetryClass::Transient.failure(format!(
                "export of '{reference}' from {} was truncated: {} of {n} bytes",
                client.endpoint,
                sink.written()
            )));
        }
    }
    let (path, _) = sink.finish()?;
    Ok(Fetched {
        path,
        wanted_refs: vec![reference.to_string()],
        expected_manifest: None,
        fingerprint: None,
    })
}
