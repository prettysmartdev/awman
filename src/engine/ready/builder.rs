//! Source-matched local image builders. Selection is pure; only an opted-in
//! ready build invokes a host CLI. Runtimes still execute in builtin.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
use crate::data::oci_identity::{BuiltImageIdentity, Digest, OciPlatform};
use crate::engine::container::host_cli_backend::{
    bound_docker_command, build_local_docker, tail, BuildControl, HostBuildRequest,
    HostCommandRunner, ProcessHostCommandRunner, PROBE_TIMEOUT, SOURCE_BUILD_TIMEOUT,
};
use crate::engine::error::EngineError;
use crate::engine::oci::docker_engine::Endpoint;
use crate::engine::oci::ResolvedImageSource;
pub use crate::engine::oci::SourceImage;

/// How long one source-store inspection may take. Longer than a probe: the
/// Apple bridge may wait for launchd to start its service.
pub const INSPECT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalBuilderTarget {
    Docker { endpoint: String },
    Apple,
}

impl LocalBuilderTarget {
    /// The builder as named in messages: the container system and, for
    /// Docker, the endpoint it is bound to.
    pub fn describe(&self) -> String {
        match self {
            Self::Docker { endpoint } => format!("Docker at {endpoint}"),
            Self::Apple => "Apple Containers".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuilderSelection {
    Local(LocalBuilderTarget),
    Incompatible(ImageSourceKind),
    Unconfigured,
}

impl BuilderSelection {
    pub fn from_source(source: Option<&ResolvedImageSource>) -> Self {
        let Some(source) = source else {
            return Self::Unconfigured;
        };
        match (&source.source, &source.docker_endpoint) {
            // Acquisition reads Unix sockets only on Unix hosts, so only
            // there can a build and its import share one endpoint.
            (ImageSourceSpec::DockerStore { .. }, Some(Endpoint::Unix(_))) if cfg!(unix) => {
                Self::Local(LocalBuilderTarget::Docker {
                    endpoint: source.docker_endpoint.as_ref().unwrap().describe(),
                })
            }
            (ImageSourceSpec::AppleStore { .. }, _) => Self::Local(LocalBuilderTarget::Apple),
            _ => Self::Incompatible(source.source.kind()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LocalImageBuildRequest {
    pub target: LocalBuilderTarget,
    pub reference: String,
    pub dockerfile: PathBuf,
    pub context: PathBuf,
    pub platform: OciPlatform,
    /// `--build-arg` values; empty for every current ready build, as for the
    /// Docker runtime's builds.
    pub build_args: Vec<(String, String)>,
    pub no_cache: bool,
}

/// Fakeable seam for the whole ready flow. Implementations may hold a fake
/// store and script probe, inspect, build, failure and cancellation outcomes.
pub trait LocalImageBuilder: Send + Sync {
    fn probe(&self, target: &LocalBuilderTarget, control: &BuildControl)
        -> Result<(), EngineError>;
    fn inspect(
        &self,
        target: &LocalBuilderTarget,
        reference: &str,
        platform: &OciPlatform,
        control: &BuildControl,
    ) -> Result<SourceImage, EngineError>;
    fn build(
        &self,
        request: &LocalImageBuildRequest,
        control: &BuildControl,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<BuiltImageIdentity, EngineError>;
}

pub struct HostLocalImageBuilder {
    runner: Arc<dyn HostCommandRunner>,
}

impl Default for HostLocalImageBuilder {
    fn default() -> Self {
        Self {
            runner: Arc::new(ProcessHostCommandRunner),
        }
    }
}

impl HostLocalImageBuilder {
    pub fn with_runner(runner: Arc<dyn HostCommandRunner>) -> Self {
        Self { runner }
    }
}

fn bound_command(target: &LocalBuilderTarget) -> Command {
    match target {
        LocalBuilderTarget::Docker { endpoint } => bound_docker_command(endpoint),
        LocalBuilderTarget::Apple => Command::new(crate::engine::host_cli::program("container")),
    }
}

/// The fields of `docker image inspect --format '{{json .}}'` the builder
/// reads. Older Engines omit `Descriptor` even with the containerd image
/// store, so the store kind is checked separately in that case.
#[derive(Deserialize)]
struct DockerInspect {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Os", default)]
    os: String,
    #[serde(rename = "Architecture", default)]
    architecture: String,
    #[serde(rename = "Variant", default)]
    variant: Option<String>,
    #[serde(rename = "Descriptor", default)]
    descriptor: Option<DockerDescriptor>,
}

#[derive(Deserialize)]
struct DockerDescriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
}

const MANIFEST_MEDIA_TYPES: &[&str] = &[
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

/// Classify one Docker inspect document. The image ID's digest kind depends
/// on the Engine's image store: the classic store reports the config digest
/// and no descriptor; the containerd store reports the digest of the stored
/// manifest (or index). Newer Engines also report a descriptor. The identity holds the digest
/// in the matching field, so it is compared with the same kind of digest
/// after export.
pub fn classify_docker_inspect(
    json: &str,
    platform: &OciPlatform,
) -> Result<SourceImage, EngineError> {
    classify_docker_inspect_for_store(json, platform, false)
}

fn classify_docker_inspect_for_store(
    json: &str,
    platform: &OciPlatform,
    containerd_store: bool,
) -> Result<SourceImage, EngineError> {
    let inspect: DockerInspect = serde_json::from_str(json).map_err(|e| {
        EngineError::Container(format!(
            "Docker returned an unreadable image inspection: {e}"
        ))
    })?;
    let found = OciPlatform {
        os: inspect.os,
        architecture: inspect.architecture,
        variant: inspect.variant.filter(|v| !v.is_empty()),
    };
    if !platform.matches(&found) {
        return Ok(SourceImage::Absent);
    }
    let id = Digest::parse(&inspect.id).map_err(EngineError::Config)?;
    let (config, manifest) = match inspect.descriptor {
        None if containerd_store => (None, Some(id)),
        None => (Some(id), None),
        Some(descriptor) if MANIFEST_MEDIA_TYPES.contains(&descriptor.media_type.as_str()) => {
            let digest = Digest::parse(&descriptor.digest).map_err(EngineError::Config)?;
            (None, Some(digest))
        }
        Some(_) => return Ok(SourceImage::Unbound),
    };
    BuiltImageIdentity::new(config, manifest, platform.clone())
        .map(SourceImage::Present)
        .map_err(EngineError::Config)
}

impl HostLocalImageBuilder {
    fn docker_uses_containerd_store(
        &self,
        target: &LocalBuilderTarget,
        control: &BuildControl,
    ) -> Result<bool, EngineError> {
        let mut command = bound_command(target);
        command.args(["info", "--format", "{{json .DriverStatus}}"]);
        let mut lines = Vec::new();
        let status = self
            .runner
            .run(command, &control.bounded(PROBE_TIMEOUT), &mut |line| {
                lines.push(line.to_string());
            })?;
        if !status.success() {
            return Err(EngineError::Container(format!(
                "{} could not identify its image store (exit {})",
                target.describe(),
                status.code().unwrap_or(-1)
            )));
        }
        let raw = lines
            .iter()
            .find(|line| line.trim_start().starts_with('['))
            .ok_or_else(|| {
                EngineError::Container(format!(
                    "{} did not report its image store; cannot verify the image ID",
                    target.describe()
                ))
            })?;
        let status: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
            EngineError::Container(format!(
                "{} reported an unreadable image store: {e}",
                target.describe()
            ))
        })?;
        Ok(status.to_string().contains("io.containerd.snapshotter.v1"))
    }

    fn inspect_docker(
        &self,
        target: &LocalBuilderTarget,
        reference: &str,
        platform: &OciPlatform,
        control: &BuildControl,
    ) -> Result<SourceImage, EngineError> {
        let mut command = bound_command(target);
        command.args(["image", "inspect", "--format", "{{json .}}", reference]);
        let mut lines = Vec::new();
        let status = self
            .runner
            .run(command, &control.bounded(INSPECT_TIMEOUT), &mut |line| {
                lines.push(line.to_string())
            })?;
        if !status.success() {
            let missing = lines
                .iter()
                .any(|l| l.contains("No such image") || l.contains("No such object"));
            return if missing {
                Ok(SourceImage::Absent)
            } else {
                Err(EngineError::Container(format!(
                    "{} could not inspect {reference} (exit {}): {}",
                    target.describe(),
                    status.code().unwrap_or(-1),
                    tail(&lines)
                )))
            };
        }
        let json = lines
            .iter()
            .find(|line| line.trim_start().starts_with('{'))
            .ok_or_else(|| {
                EngineError::Container(format!(
                    "{} returned no image inspection for {reference}",
                    target.describe()
                ))
            })?;
        let containerd_store = if json.contains("\"Descriptor\"") {
            false
        } else {
            self.docker_uses_containerd_store(target, control)?
        };
        classify_docker_inspect_for_store(json, platform, containerd_store)
    }
}

impl LocalImageBuilder for HostLocalImageBuilder {
    fn probe(
        &self,
        target: &LocalBuilderTarget,
        control: &BuildControl,
    ) -> Result<(), EngineError> {
        let mut command = bound_command(target);
        let what = match target {
            LocalBuilderTarget::Docker { .. } => {
                command.arg("info");
                "docker info"
            }
            LocalBuilderTarget::Apple => {
                command.args(["system", "status"]);
                "container system status"
            }
        };
        let mut lines = Vec::new();
        let status = self
            .runner
            .run(command, &control.bounded(PROBE_TIMEOUT), &mut |line| {
                lines.push(line.to_string());
                if lines.len() > 50 {
                    lines.remove(0);
                }
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(EngineError::Container(format!(
                "the {} image builder is unavailable (`{what}` exited {}): {}",
                target.describe(),
                status.code().unwrap_or(-1),
                tail(&lines)
            )))
        }
    }

    fn inspect(
        &self,
        target: &LocalBuilderTarget,
        reference: &str,
        platform: &OciPlatform,
        control: &BuildControl,
    ) -> Result<SourceImage, EngineError> {
        self.probe(target, control)?;
        match target {
            LocalBuilderTarget::Apple => {
                let transport =
                    crate::engine::oci::apple_store::installed_transport().ok_or_else(|| {
                        EngineError::Container(
                            "Apple Containers image transport is unavailable".into(),
                        )
                    })?;
                crate::engine::oci::apple_store::inspect_local(
                    transport.as_ref(),
                    reference,
                    platform,
                    &control.bounded(INSPECT_TIMEOUT),
                )
            }
            LocalBuilderTarget::Docker { .. } => {
                self.inspect_docker(target, reference, platform, control)
            }
        }
    }

    fn build(
        &self,
        request: &LocalImageBuildRequest,
        control: &BuildControl,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<BuiltImageIdentity, EngineError> {
        self.probe(&request.target, control)?;
        match &request.target {
            LocalBuilderTarget::Docker { endpoint } => {
                let image_id = build_local_docker(
                    self.runner.as_ref(),
                    &HostBuildRequest {
                        endpoint,
                        tag: &request.reference,
                        dockerfile: &request.dockerfile,
                        context: &request.context,
                        platform: &request.platform,
                        build_args: &request.build_args,
                        no_cache: request.no_cache,
                    },
                    control,
                    on_line,
                )?;
                // Classify the ID the build itself reported, never the tag,
                // which a concurrent retag could already have moved.
                let built = self.inspect_docker(
                    &request.target,
                    &image_id.to_string(),
                    &request.platform,
                    control,
                )?;
                match built {
                    SourceImage::Present(identity)
                        if identity.config_digest() == Some(&image_id)
                            || identity.manifest_digest() == Some(&image_id) =>
                    {
                        Ok(identity)
                    }
                    SourceImage::Present(_) => Err(EngineError::Container(format!(
                        "{} reported image {image_id} for {}, but inspecting it returned a \
                         different image",
                        request.target.describe(),
                        request.reference
                    ))),
                    SourceImage::Unbound => Err(EngineError::Container(format!(
                        "{} stored the build of {} as a multi-platform index; awman needs a \
                         single {} image to verify the import",
                        request.target.describe(),
                        request.reference,
                        request.platform
                    ))),
                    SourceImage::Absent => Err(EngineError::Container(format!(
                        "{} reported image {image_id} for {}, but it is not a {} image in that \
                         store",
                        request.target.describe(),
                        request.reference,
                        request.platform
                    ))),
                }
            }
            LocalBuilderTarget::Apple => {
                let mut command = bound_command(&request.target);
                let platform = request.platform.to_string();
                let argv = crate::engine::container::host_cli_backend::BuildArgv {
                    platform: Some(&platform),
                    build_args: &request.build_args,
                    ..crate::engine::container::host_cli_backend::BuildArgv::new(
                        &request.reference,
                        &request.dockerfile,
                        &request.context,
                        request.no_cache,
                    )
                };
                command.args(argv.to_args());
                let mut produced_manifest = None;
                let status = self.runner.run(command, control, &mut |line| {
                    if let Some(marker) = line.find("exporting manifest sha256:") {
                        let digest = line[marker + "exporting manifest ".len()..]
                            .split_whitespace()
                            .next()
                            .unwrap_or("");
                        produced_manifest = Digest::parse(digest).ok();
                    }
                    on_line(line);
                })?;
                if !status.success() {
                    return Err(EngineError::ImageBuildExitNonzero {
                        tag: request.reference.clone(),
                        exit_code: status.code().unwrap_or(-1),
                    });
                }
                let produced_manifest = produced_manifest.ok_or_else(|| {
                    EngineError::Container(
                        "Apple builder did not report a verifiable output manifest digest".into(),
                    )
                })?;
                let installed = match self.inspect(
                    &request.target,
                    &request.reference,
                    &request.platform,
                    control,
                )? {
                    SourceImage::Present(identity) => identity,
                    SourceImage::Unbound => {
                        return BuiltImageIdentity::new(
                            None,
                            Some(produced_manifest),
                            request.platform.clone(),
                        )
                        .map_err(EngineError::Config);
                    }
                    SourceImage::Absent => {
                        return Err(EngineError::Container(
                            "Apple builder produced no inspectable image".into(),
                        ))
                    }
                };
                if installed.manifest_digest() != Some(&produced_manifest) {
                    return Err(EngineError::Container(format!(
                        "Apple image {} changed between build and source-store inspection",
                        request.reference
                    )));
                }
                Ok(installed)
            }
        }
    }
}

pub fn build_control(cancel: crate::engine::oci::CancelToken) -> BuildControl {
    BuildControl::new(cancel, SOURCE_BUILD_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::image_source::DockerTlsConfig;
    use crate::engine::oci::docker_engine::Endpoint;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingRunner {
        argv: Mutex<Vec<Vec<String>>>,
    }

    impl HostCommandRunner for RecordingRunner {
        fn run(
            &self,
            command: Command,
            _control: &BuildControl,
            on_line: &mut dyn FnMut(&str),
        ) -> Result<std::process::ExitStatus, EngineError> {
            use std::os::unix::process::ExitStatusExt;
            let args: Vec<String> = command
                .get_args()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect();
            if args.iter().any(|arg| arg == "inspect") {
                on_line(&format!(
                    r#"{{"Id":"sha256:{}","Os":"linux","Architecture":"{}"}}"#,
                    "a".repeat(64),
                    OciPlatform::host_linux().architecture
                ));
            } else if args.iter().any(|arg| arg == "--format") {
                on_line(r#"[["driver-type","io.containerd.snapshotter.v1"]]"#);
            }
            self.argv.lock().unwrap().push(args);
            Ok(std::process::ExitStatus::from_raw(0))
        }
    }

    #[test]
    fn host_builder_binds_inspection_and_classifies_old_containerd_ids() {
        let runner = Arc::new(RecordingRunner::default());
        let builder = HostLocalImageBuilder::with_runner(runner.clone());
        let target = LocalBuilderTarget::Docker {
            endpoint: "unix:///run/explicit.sock".into(),
        };
        let image = builder
            .inspect(
                &target,
                "agent:latest",
                &OciPlatform::host_linux(),
                &build_control(crate::engine::oci::CancelToken::new()),
            )
            .unwrap();
        assert!(
            matches!(image, SourceImage::Present(id) if id.manifest_digest().is_some() && id.config_digest().is_none())
        );
        let calls = runner.argv.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls
            .iter()
            .all(|args| args[0..2] == ["--host", "unix:///run/explicit.sock"]));
    }

    fn host_platform() -> OciPlatform {
        OciPlatform::host_linux()
    }

    fn other_platform(platform: &OciPlatform) -> OciPlatform {
        OciPlatform {
            os: platform.os.clone(),
            architecture: if platform.architecture == "amd64" {
                "arm64".to_string()
            } else {
                "amd64".to_string()
            },
            variant: None,
        }
    }

    fn digest(c: char) -> String {
        format!("sha256:{}", c.to_string().repeat(64))
    }

    // ── classify_docker_inspect ──────────────────────────────────────────

    /// The classic image store reports no `Descriptor`: the ID is the image's
    /// config digest, the only digest kind that store can give.
    #[test]
    fn classify_docker_inspect_classic_store_is_a_config_digest() {
        let platform = host_platform();
        let json = format!(
            r#"{{"Id":"{}","Os":"{}","Architecture":"{}"}}"#,
            digest('a'),
            platform.os,
            platform.architecture
        );
        match classify_docker_inspect(&json, &platform).unwrap() {
            SourceImage::Present(identity) => {
                assert_eq!(identity.config_digest().unwrap().as_str(), digest('a'));
                assert!(identity.manifest_digest().is_none());
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    #[test]
    fn classify_pre_28_containerd_store_id_as_manifest_digest() {
        let platform = host_platform();
        let json = format!(
            r#"{{"Id":"{}","Os":"{}","Architecture":"{}"}}"#,
            digest('a'),
            platform.os,
            platform.architecture
        );
        match classify_docker_inspect_for_store(&json, &platform, true).unwrap() {
            SourceImage::Present(identity) => {
                assert_eq!(identity.manifest_digest().unwrap().as_str(), digest('a'));
                assert!(identity.config_digest().is_none());
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    /// The containerd store reports a manifest `Descriptor`: the stored
    /// digest is the manifest digest, never compared as a config digest.
    #[test]
    fn classify_docker_inspect_containerd_store_manifest_is_a_manifest_digest() {
        let platform = host_platform();
        let json = format!(
            r#"{{"Id":"{}","Os":"{}","Architecture":"{}",
                 "Descriptor":{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}"}}}}"#,
            digest('a'),
            platform.os,
            platform.architecture,
            digest('b'),
        );
        match classify_docker_inspect(&json, &platform).unwrap() {
            SourceImage::Present(identity) => {
                assert_eq!(identity.manifest_digest().unwrap().as_str(), digest('b'));
                assert!(identity.config_digest().is_none());
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    /// The older Docker distribution manifest media type is accepted the
    /// same way as the OCI one.
    #[test]
    fn classify_docker_inspect_accepts_the_docker_distribution_manifest_media_type() {
        let platform = host_platform();
        let json = format!(
            r#"{{"Id":"{}","Os":"{}","Architecture":"{}",
                 "Descriptor":{{"mediaType":"application/vnd.docker.distribution.manifest.v2+json","digest":"{}"}}}}"#,
            digest('a'),
            platform.os,
            platform.architecture,
            digest('c'),
        );
        match classify_docker_inspect(&json, &platform).unwrap() {
            SourceImage::Present(identity) => {
                assert_eq!(identity.manifest_digest().unwrap().as_str(), digest('c'));
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    /// A multi-platform index descriptor cannot be bound to one identity
    /// without exporting it.
    #[test]
    fn classify_docker_inspect_an_index_descriptor_is_unbound() {
        let platform = host_platform();
        let json = format!(
            r#"{{"Id":"{}","Os":"{}","Architecture":"{}",
                 "Descriptor":{{"mediaType":"application/vnd.oci.image.index.v1+json","digest":"{}"}}}}"#,
            digest('a'),
            platform.os,
            platform.architecture,
            digest('d'),
        );
        assert_eq!(
            classify_docker_inspect(&json, &platform).unwrap(),
            SourceImage::Unbound
        );
    }

    /// An image for a different platform is absent for this platform — it
    /// must be rebuilt, not imported.
    #[test]
    fn classify_docker_inspect_wrong_platform_is_absent() {
        let platform = host_platform();
        let wrong = other_platform(&platform);
        let json = format!(
            r#"{{"Id":"{}","Os":"{}","Architecture":"{}"}}"#,
            digest('a'),
            wrong.os,
            wrong.architecture,
        );
        assert_eq!(
            classify_docker_inspect(&json, &platform).unwrap(),
            SourceImage::Absent
        );
    }

    /// Unreadable JSON from `docker image inspect` is an error, not a
    /// silent `Absent` (which would trigger an unnecessary rebuild) or a
    /// silent `Present` (which would claim unverified content).
    #[test]
    fn classify_docker_inspect_malformed_json_is_an_error() {
        let platform = host_platform();
        assert!(classify_docker_inspect("not json", &platform).is_err());
    }

    // ── BuilderSelection::from_source ────────────────────────────────────

    fn resolved(source: ImageSourceSpec, docker_endpoint: Option<Endpoint>) -> ResolvedImageSource {
        ResolvedImageSource {
            source,
            reference: "awman-repo-claude:latest".into(),
            platform: host_platform(),
            store: "unix:///var/run/docker.sock".into(),
            docker_endpoint,
        }
    }

    #[test]
    fn no_source_is_unconfigured() {
        assert_eq!(
            BuilderSelection::from_source(None),
            BuilderSelection::Unconfigured
        );
    }

    #[test]
    fn docker_store_on_a_local_unix_socket_is_the_local_docker_builder() {
        let source = resolved(
            ImageSourceSpec::DockerStore {
                host: Some("unix:///var/run/docker.sock".into()),
                tls: None,
                reference: None,
            },
            Some(Endpoint::Unix("/var/run/docker.sock".into())),
        );
        assert_eq!(
            BuilderSelection::from_source(Some(&source)),
            BuilderSelection::Local(LocalBuilderTarget::Docker {
                endpoint: "unix:///var/run/docker.sock".into(),
            })
        );
    }

    /// Every TCP endpoint is incompatible for building, including loopback
    /// and TLS — only a local Unix socket is eligible in this item.
    #[test]
    fn docker_store_on_any_tcp_endpoint_is_incompatible() {
        for tls in [
            None,
            Some(DockerTlsConfig {
                ca: "/ca.pem".into(),
                cert: Some("/cert.pem".into()),
                key: Some("/key.pem".into()),
                verify: true,
            }),
        ] {
            let source = resolved(
                ImageSourceSpec::DockerStore {
                    host: Some("tcp://127.0.0.1:2375".into()),
                    tls: tls.clone(),
                    reference: None,
                },
                Some(Endpoint::Tcp {
                    host: "127.0.0.1".into(),
                    port: 2375,
                    tls,
                }),
            );
            assert_eq!(
                BuilderSelection::from_source(Some(&source)),
                BuilderSelection::Incompatible(ImageSourceKind::DockerStore),
                "TCP must be incompatible even for loopback"
            );
        }
    }

    #[test]
    fn apple_store_is_always_the_local_apple_builder() {
        let source = resolved(ImageSourceSpec::AppleStore { reference: None }, None);
        assert_eq!(
            BuilderSelection::from_source(Some(&source)),
            BuilderSelection::Local(LocalBuilderTarget::Apple)
        );
    }

    #[test]
    fn archive_and_registry_sources_are_incompatible() {
        let archive = resolved(
            ImageSourceSpec::Archive {
                path: "/exports/agent.tar".into(),
            },
            None,
        );
        assert_eq!(
            BuilderSelection::from_source(Some(&archive)),
            BuilderSelection::Incompatible(ImageSourceKind::Archive)
        );
        let registry = resolved(
            ImageSourceSpec::Registry {
                registry: Some("ghcr.io".into()),
                reference: None,
            },
            None,
        );
        assert_eq!(
            BuilderSelection::from_source(Some(&registry)),
            BuilderSelection::Incompatible(ImageSourceKind::Registry)
        );
    }
}
