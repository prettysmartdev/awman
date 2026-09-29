//! Apple Containers image store — the version-gated, in-process bridge.
//!
//! # The contract (evidence dated 2026-09-29)
//!
//! `apple/container` keeps its images in a content store owned by the launch
//! agent helper `container-core-images`. The CLI's `container image save`
//! does not read that store itself: it sends an XPC request to the helper,
//! which writes an OCI image-layout tar to a caller-supplied path. awman
//! sends the same requests itself, in-process, through libxpc:
//!
//! | step | service | route | request keys | reply keys |
//! |---|---|---|---|---|
//! | release check | `com.apple.container.apiserver` | `ping` | — | `apiServerVersion`, `apiServerCommit`, `apiServerBuild` |
//! | selection | `com.apple.container.core.container-core-images` | `imageList` | — | `imageDescriptions` (JSON `[ImageDescription]`) |
//! | export | `com.apple.container.core.container-core-images` | `imageSave` | `imageDescriptions`, `filePath`, `ociPlatform` (JSON) | — |
//!
//! The route travels under `com.apple.container.xpc.route`; a failed request
//! replies with JSON `{code, message}` under `com.apple.container.xpc.error`.
//! The helper only checks that the caller has its effective uid.
//!
//! The schema carries **no protocol version**. The bridge therefore pins
//! whole releases: before any image route it pings the API server and
//! requires the exact release *and* full commit to be one of
//! [`SUPPORTED_RELEASES`]. Each row was established from the tagged sources
//! (the ping, list and save handlers, `ImageDescription`, the XPC key enums
//! and `XPCMessage` framing are identical in the parts awman uses) and by
//! native round trips against that release on Apple Silicon. Any other
//! release — older, newer or a local build — is refused before an image
//! route is sent (1.3.1 was confirmed refused natively).
//!
//! # What awman does and never does
//!
//! The helper, not awman, reads the store; awman never opens anything under
//! `~/Library/Application Support/com.apple.container`. The export lands in
//! a fresh private directory inside the leased staging area of awman's own
//! cache. awman then checks it is a single-link regular file it owns, moves
//! it (same filesystem) into place, and hands it to the same limits,
//! archive validator and atomic cache publication every other source uses.
//! awman does not run the `container` CLI, ship or extract a helper
//! executable or library, or fall back to another source.
//!
//! The unsafe libxpc calls live outside this library, in the binary's
//! `apple_xpc` module, behind the safe [`XpcTransport`] trait; the library
//! keeps `#![forbid(unsafe_code)]`. A build without that transport (any
//! non-macOS target, or library tests) refuses the source with
//! `ImageSourceBlocked`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::data::config::image_source::ImageSourceKind;
use crate::data::oci_identity::{Digest, OciPlatform};
use crate::engine::error::EngineError;
use crate::engine::oci::sources::{FetchContext, Fetched, Report};

/// When the sources and native service below were read.
pub const EVIDENCE_DATE: &str = "2026-09-29";

/// How a supported release was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseEvidence {
    /// Native round trips against the installed service on Apple Silicon.
    Native,
    /// The tagged sources were compared with a natively validated release,
    /// without a native run.
    SourceVerified,
}

/// One `apple/container` release the bridge may speak to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedRelease {
    pub version: &'static str,
    /// Full commit the release tag resolves to, as the API server reports it.
    pub commit: &'static str,
    pub evidence: ReleaseEvidence,
}

/// The only releases the bridge sends image routes to.
pub const SUPPORTED_RELEASES: &[SupportedRelease] = &[
    SupportedRelease {
        version: "0.12.0",
        commit: "651811cc090937457956643dd2c454df77eb141b",
        evidence: ReleaseEvidence::Native,
    },
    SupportedRelease {
        version: "1.4.1",
        commit: "9a8917ca2da5cd6ba059b9ba5ca5a74892e9bb7d",
        evidence: ReleaseEvidence::Native,
    },
    SupportedRelease {
        version: "1.5.0",
        commit: "d265d669ecae041bf338cb3b39c4118316d138f0",
        evidence: ReleaseEvidence::Native,
    },
];

/// The XPC contract shared by every supported release. Data only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeContract {
    pub api_service: &'static str,
    pub images_service: &'static str,
    pub route_key: &'static str,
    pub error_key: &'static str,
    pub ping_route: &'static str,
    pub list_route: &'static str,
    pub save_route: &'static str,
    pub version_key: &'static str,
    pub commit_key: &'static str,
    pub build_key: &'static str,
    pub descriptions_key: &'static str,
    pub file_path_key: &'static str,
    pub platform_key: &'static str,
    /// Whether the protocol carries its own version (it does not).
    pub protocol_versioned: bool,
}

pub const CONTRACT: BridgeContract = BridgeContract {
    api_service: "com.apple.container.apiserver",
    images_service: "com.apple.container.core.container-core-images",
    route_key: "com.apple.container.xpc.route",
    error_key: "com.apple.container.xpc.error",
    ping_route: "ping",
    list_route: "imageList",
    save_route: "imageSave",
    version_key: "apiServerVersion",
    commit_key: "apiServerCommit",
    build_key: "apiServerBuild",
    descriptions_key: "imageDescriptions",
    file_path_key: "filePath",
    platform_key: "ociPlatform",
    protocol_versioned: false,
};

// ── Transport ──────────────────────────────────────────────────────────────

/// A value in an XPC dictionary, as far as this contract uses them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XpcValue {
    String(String),
    Data(Vec<u8>),
}

/// The type a reply key is read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XpcKind {
    String,
    Data,
}

/// The reply keys that were present with the requested type.
pub type XpcReply = BTreeMap<String, XpcValue>;

/// When a request must stop waiting.
pub struct WaitControl<'a> {
    pub deadline: Instant,
    /// Polled while the request is outstanding; `true` abandons it.
    pub stop: &'a (dyn Fn() -> bool + Sync),
}

/// Why a request produced no reply dictionary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XpcFailure {
    /// The mach service is not registered (the service is not running).
    Unavailable,
    /// The peer went away mid-request (crash or restart).
    Interrupted,
    /// `stop` returned true; the connection was cancelled.
    Stopped,
    /// The deadline passed; the connection was cancelled.
    TimedOut,
    /// The reply was neither a dictionary nor a connection error.
    UnexpectedReply,
}

/// Sends one request dictionary to a mach service and returns the reply.
/// Implemented natively by the binary on macOS; faked in tests.
pub trait XpcTransport: Send + Sync {
    fn send(
        &self,
        service: &str,
        request: &[(&str, XpcValue)],
        reply_keys: &[(&str, XpcKind)],
        wait: &WaitControl<'_>,
    ) -> Result<XpcReply, XpcFailure>;
}

static NATIVE: OnceLock<Arc<dyn XpcTransport>> = OnceLock::new();

/// Register the process's native transport. Called once by the binary at
/// startup on macOS; later calls are ignored.
pub fn install_transport(transport: Arc<dyn XpcTransport>) {
    let _ = NATIVE.set(transport);
}

/// The registered native transport, if this process has one.
pub fn installed_transport() -> Option<Arc<dyn XpcTransport>> {
    NATIVE.get().cloned()
}

// ── Errors ─────────────────────────────────────────────────────────────────

fn blocked(reason: String) -> EngineError {
    EngineError::ImageSourceBlocked {
        source_kind: ImageSourceKind::AppleStore,
        reason,
    }
}

fn unsupported(reason: String) -> EngineError {
    EngineError::UnsupportedImageSource {
        source_kind: ImageSourceKind::AppleStore,
        reason,
    }
}

/// Why this process has no bridge at all.
pub(super) fn no_transport() -> EngineError {
    let why = if cfg!(target_os = "macos") {
        "this awman process has no Apple Containers bridge registered"
    } else {
        "Apple Containers exists only on macOS"
    };
    blocked(format!(
        "{why}. awman does not run the `container` CLI or read the store's private files; use a \
         registry, Docker Engine or archive image source instead."
    ))
}

/// Map a transport failure. Only an interrupted peer may be retried.
fn transport_error(service: &str, route: &str, failure: XpcFailure) -> EngineError {
    match failure {
        XpcFailure::Unavailable => blocked(format!(
            "the Apple Containers service is not running ({service} is not registered). Start it \
             with `container system start` and run `awman ready` again."
        )),
        XpcFailure::Interrupted => EngineError::Network(format!(
            "the Apple Containers service {service} went away during `{route}`"
        )),
        XpcFailure::Stopped => crate::engine::oci::retry::cancelled(),
        XpcFailure::TimedOut => {
            EngineError::Network("image acquisition exceeded its deadline".into())
        }
        XpcFailure::UnexpectedReply => unsupported(format!(
            "{service} answered `{route}` with an object that is not a reply dictionary"
        )),
    }
}

/// Printable, bounded text from the peer: control characters dropped,
/// private paths replaced, at most 300 characters.
fn sanitize(text: &str, private: &[&Path]) -> String {
    let mut out = text.to_string();
    for p in private {
        let s = p.display().to_string();
        if !s.is_empty() {
            out = out.replace(&s, "<staging>");
        }
    }
    let cleaned: String = out
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut short: String = cleaned.chars().take(300).collect();
    if cleaned.chars().count() > 300 {
        short.push('…');
    }
    short
}

#[derive(serde::Deserialize)]
struct PeerError {
    code: String,
    message: String,
}

/// Send one route and check the protocol-level error key.
fn call(
    transport: &dyn XpcTransport,
    service: &str,
    route: &str,
    mut fields: Vec<(&str, XpcValue)>,
    reply_keys: &[(&str, XpcKind)],
    wait: &WaitControl<'_>,
    private: &[&Path],
) -> Result<XpcReply, EngineError> {
    fields.insert(0, (CONTRACT.route_key, XpcValue::String(route.into())));
    let mut keys = reply_keys.to_vec();
    keys.push((CONTRACT.error_key, XpcKind::Data));
    let reply = transport
        .send(service, &fields, &keys, wait)
        .map_err(|f| transport_error(service, route, f))?;
    if let Some(XpcValue::Data(raw)) = reply.get(CONTRACT.error_key) {
        let (code, message) = match serde_json::from_slice::<PeerError>(raw) {
            Ok(e) => (e.code, e.message),
            Err(_) => ("malformed".into(), "unreadable error payload".into()),
        };
        let code = sanitize(&code, &[]);
        let message = sanitize(&message, private);
        return Err(match code.as_str() {
            "notFound" => {
                EngineError::Config(format!("Apple Containers `{route}` failed: {message}"))
            }
            "interrupted" => EngineError::Network(format!(
                "Apple Containers `{route}` was interrupted: {message}"
            )),
            _ => EngineError::Container(format!(
                "Apple Containers `{route}` failed ({code}): {message}"
            )),
        });
    }
    Ok(reply)
}

// ── Release check ──────────────────────────────────────────────────────────

/// The release the API server reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRelease {
    pub version: String,
    pub commit: String,
    pub build: String,
}

/// `apiServerVersion` is `X.Y.Z` in 1.x and
/// `container-apiserver version X.Y.Z (build: …, commit: …)` in 0.x.
fn parse_version(raw: &str) -> Option<String> {
    let token = match raw.split_once(" version ") {
        Some((_, rest)) => rest.split_whitespace().next()?,
        None => raw.trim(),
    };
    let ok = !token.is_empty()
        && token.split('.').count() == 3
        && token
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    ok.then(|| token.to_string())
}

/// Check the reported release against [`SUPPORTED_RELEASES`].
pub fn check_release(reply: &XpcReply) -> Result<ServiceRelease, EngineError> {
    let get = |key: &str| match reply.get(key) {
        Some(XpcValue::String(s)) => Some(s.clone()),
        _ => None,
    };
    let (Some(raw), Some(commit), Some(build)) = (
        get(CONTRACT.version_key),
        get(CONTRACT.commit_key),
        get(CONTRACT.build_key),
    ) else {
        return Err(unsupported(
            "the Apple Containers API server did not report its release, commit and build; \
             awman only talks to releases it has verified"
                .into(),
        ));
    };
    let version = parse_version(&raw).unwrap_or_else(|| sanitize(&raw, &[]));
    let release = ServiceRelease {
        version,
        commit: sanitize(&commit, &[]),
        build: sanitize(&build, &[]),
    };
    let known = SUPPORTED_RELEASES
        .iter()
        .any(|r| r.version == release.version && r.commit == release.commit);
    if !known || release.build != "release" {
        let supported = SUPPORTED_RELEASES
            .iter()
            .map(|r| r.version)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(unsupported(format!(
            "Apple Containers {} ({} build, commit {}) is not a release awman has verified; \
             supported releases: {supported}. The image-store protocol is unversioned, so awman \
             refuses unknown releases instead of guessing.",
            release.version, release.build, release.commit
        )));
    }
    Ok(release)
}

// ── Selection ──────────────────────────────────────────────────────────────

/// Fully qualify a reference the way the store does for Docker Hub names:
/// `name` → `docker.io/library/name:latest`.
pub fn normalize_reference(reference: &str) -> String {
    let (domain, rest) = match reference.split_once('/') {
        Some((first, rest))
            if first.contains('.') || first.contains(':') || first == "localhost" =>
        {
            (first.to_string(), rest.to_string())
        }
        _ => ("docker.io".to_string(), reference.to_string()),
    };
    let rest = if domain == "docker.io" && !rest.contains('/') {
        format!("library/{rest}")
    } else {
        rest
    };
    let last = rest.rsplit('/').next().unwrap_or("");
    let rest = if last.contains(':') || last.contains('@') {
        rest
    } else {
        format!("{rest}:latest")
    };
    format!("{domain}/{rest}")
}

/// The one store entry named `reference`.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectedImage {
    /// The store's own reference string.
    pub reference: String,
    pub digest: String,
    pub media_type: String,
    /// The entry exactly as listed, re-sent to `imageSave`.
    pub description: serde_json::Value,
}

/// Choose the single store image named `reference` from an `imageList`
/// reply. Missing, malformed or ambiguous listings are refused.
pub fn select_image(listing: &[u8], reference: &str) -> Result<SelectedImage, EngineError> {
    let entries: Vec<serde_json::Value> = serde_json::from_slice(listing).map_err(|_| {
        unsupported("the Apple Containers image list is not a JSON array of images".into())
    })?;
    let wanted = normalize_reference(reference);
    let mut found: Vec<SelectedImage> = Vec::new();
    for entry in entries {
        let Some(name) = entry.get("reference").and_then(|v| v.as_str()) else {
            continue;
        };
        if normalize_reference(name) != wanted {
            continue;
        }
        let descriptor = entry.get("descriptor");
        let field = |k: &str| {
            descriptor
                .and_then(|d| d.get(k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let (Some(digest), Some(media_type)) = (field("digest"), field("mediaType")) else {
            return Err(unsupported(format!(
                "the Apple Containers entry for {} has no descriptor digest or media type",
                sanitize(name, &[])
            )));
        };
        if Digest::parse(&digest).is_err() {
            return Err(unsupported(format!(
                "the Apple Containers entry for {} has an invalid digest",
                sanitize(name, &[])
            )));
        }
        if !found.iter().any(|f| f.digest == digest) {
            found.push(SelectedImage {
                reference: name.to_string(),
                digest,
                media_type,
                description: entry,
            });
        }
    }
    match found.len() {
        0 => Err(EngineError::Config(format!(
            "image '{}' is not in the Apple Containers image store; build or pull it with \
             Apple Containers first",
            sanitize(reference, &[])
        ))),
        1 => Ok(found.pop().expect("one entry")),
        n => Err(EngineError::Config(format!(
            "{n} different Apple Containers images are named '{}'; remove the stale ones so the \
             name is unambiguous",
            sanitize(reference, &[])
        ))),
    }
}

/// A single-platform manifest descriptor vouches for the manifest digest
/// the validator must find; an index does not.
fn expected_manifest(selected: &SelectedImage) -> Option<Digest> {
    const MANIFESTS: &[&str] = &[
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.docker.distribution.manifest.v2+json",
    ];
    MANIFESTS
        .contains(&selected.media_type.as_str())
        .then(|| Digest::parse(&selected.digest).ok())
        .flatten()
}

/// The `ociPlatform` value: containerization's `Platform` uses the OCI key
/// names.
fn platform_json(platform: &OciPlatform) -> Vec<u8> {
    let mut map = serde_json::Map::new();
    map.insert("os".into(), platform.os.clone().into());
    map.insert("architecture".into(), platform.architecture.clone().into());
    if let Some(v) = &platform.variant {
        map.insert("variant".into(), v.clone().into());
    }
    serde_json::Value::Object(map).to_string().into_bytes()
}

// ── Export ─────────────────────────────────────────────────────────────────

/// Pings may wait for launchd to start the service, but not for long.
const PING_TIMEOUT: Duration = Duration::from_secs(60);

/// Export `reference` from the Apple store into `ctx.staging_dir`.
pub(super) fn export(
    transport: &dyn XpcTransport,
    awman_tag: &str,
    reference: &str,
    ctx: &FetchContext<'_>,
    _report: Report<'_>,
) -> Result<Fetched, EngineError> {
    let cancel = ctx.cancel.clone();
    let stop = move || cancel.is_cancelled();
    let deadline_in = |preferred: Option<Duration>| WaitControl {
        deadline: Instant::now() + ctx.deadline.request_timeout(preferred),
        stop: &stop,
    };
    ctx.deadline.check()?;
    ctx.cancel.check()?;

    // 1. Release check, before any image route.
    let ping = call(
        transport,
        CONTRACT.api_service,
        CONTRACT.ping_route,
        Vec::new(),
        &[
            (CONTRACT.version_key, XpcKind::String),
            (CONTRACT.commit_key, XpcKind::String),
            (CONTRACT.build_key, XpcKind::String),
        ],
        &deadline_in(Some(PING_TIMEOUT)),
        &[],
    )?;
    let release = check_release(&ping)?;
    tracing::debug!(version = %release.version, "Apple Containers release verified");

    // 2. Select exactly one image through the service.
    let listed = call(
        transport,
        CONTRACT.images_service,
        CONTRACT.list_route,
        Vec::new(),
        &[(CONTRACT.descriptions_key, XpcKind::Data)],
        &deadline_in(Some(PING_TIMEOUT)),
        &[],
    )?;
    let Some(XpcValue::Data(listing)) = listed.get(CONTRACT.descriptions_key) else {
        return Err(unsupported(
            "the Apple Containers image list reply has no image descriptions".into(),
        ));
    };
    let selected = select_image(listing, reference)?;
    ctx.cancel.check()?;

    // 3. Export into a fresh private directory of the leased staging area.
    crate::engine::oci::verify::ensure_space(
        ctx.disk,
        ctx.staging_dir,
        0,
        ctx.limits.min_free_bytes,
    )?;
    let export_dir = create_private_dir(&ctx.staging_dir.join("apple-export"))?;
    let out = export_dir.join("image.tar");
    let descriptions = serde_json::to_vec(&[&selected.description])
        .map_err(|e| EngineError::Other(format!("encoding image description: {e}")))?;
    call(
        transport,
        CONTRACT.images_service,
        CONTRACT.save_route,
        vec![
            (CONTRACT.descriptions_key, XpcValue::Data(descriptions)),
            (
                CONTRACT.file_path_key,
                XpcValue::String(out.display().to_string()),
            ),
            (
                CONTRACT.platform_key,
                XpcValue::Data(platform_json(ctx.platform)),
            ),
        ],
        &[],
        &deadline_in(None),
        &[ctx.staging_dir],
    )?;
    ctx.cancel.check()?;
    ctx.deadline.check()?;

    // 4. Accept only what the helper was asked to produce.
    let staged = ctx.staging_dir.join("archive.tar");
    adopt_export(&export_dir, &out, &staged, ctx.limits.max_archive_bytes)?;
    crate::engine::oci::verify::ensure_space(
        ctx.disk,
        ctx.staging_dir,
        0,
        ctx.limits.min_free_bytes,
    )?;
    Ok(Fetched {
        path: staged,
        wanted_refs: vec![selected.reference.clone(), awman_tag.to_string()],
        expected_manifest: expected_manifest(&selected),
        fingerprint: None,
    })
}

/// Create `dir` (which must not exist) readable only by this user.
fn create_private_dir(dir: &Path) -> Result<PathBuf, EngineError> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).map_err(|e| EngineError::io(dir, e))?;
    Ok(dir.to_path_buf())
}

fn rejected(reason: &str) -> EngineError {
    EngineError::ImageArchiveRejected {
        path: PathBuf::from("<apple-containers export>"),
        reason: reason.into(),
    }
}

/// Move the helper's output from `out` to `staged` after checking that the
/// export directory is still the private directory awman made, and that the
/// output is one regular, single-link, non-empty file owned by the same user
/// within the byte cap. The file is re-checked by identity after the move,
/// so a swapped path cannot be validated in its place.
fn adopt_export(
    export_dir: &Path,
    out: &Path,
    staged: &Path,
    max_bytes: u64,
) -> Result<(), EngineError> {
    let dir_meta =
        std::fs::symlink_metadata(export_dir).map_err(|e| EngineError::io(export_dir, e))?;
    if !dir_meta.is_dir() {
        return Err(rejected("the export directory was replaced"));
    }
    let meta = match std::fs::symlink_metadata(out) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(rejected(
                "the Apple Containers service reported success but wrote no file",
            ))
        }
        Err(e) => return Err(EngineError::io(out, e)),
    };
    if !meta.file_type().is_file() {
        return Err(rejected("the export is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != dir_meta.uid() {
            return Err(rejected("the export is owned by another user"));
        }
        if meta.nlink() != 1 {
            return Err(rejected("the export has more than one link"));
        }
        if dir_meta.mode() & 0o077 != 0 {
            return Err(rejected("the export directory is no longer private"));
        }
    }
    if meta.len() == 0 {
        return Err(rejected("the export is empty"));
    }
    if meta.len() > max_bytes {
        return Err(rejected(&format!(
            "the export is {} bytes, over the {max_bytes}-byte limit",
            meta.len()
        )));
    }
    std::fs::rename(out, staged).map_err(|e| EngineError::io(staged, e))?;
    let moved = std::fs::symlink_metadata(staged).map_err(|e| EngineError::io(staged, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (moved.dev(), moved.ino()) != (meta.dev(), meta.ino()) || moved.len() != meta.len() {
            return Err(rejected("the export changed while it was being adopted"));
        }
    }
    #[cfg(not(unix))]
    let _ = moved;
    let _ = std::fs::remove_dir(export_dir);
    Ok(())
}

#[cfg(test)]
mod tests;
