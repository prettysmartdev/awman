//! Source-matched local image builders. Selection is pure; only an opted-in
//! ready build invokes a host CLI. Runtimes still execute in builtin.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::data::oci_identity::{BuiltImageIdentity, Digest, OciPlatform};
use crate::engine::container::host_build::{
    tail, BuildArgv, HostCommandRunner, ProcessHostCommandRunner, PROBE_TIMEOUT,
};
use crate::engine::container::BuildControl;
use crate::engine::error::{BuilderFailure, EngineError};
pub use crate::engine::oci::SourceImage;

/// How long one source-store inspection may take. Longer than a probe: the
/// Apple bridge may wait for launchd to start its service.
pub const INSPECT_TIMEOUT: Duration = Duration::from_secs(60);

pub use crate::engine::container::{BuildRequest, BuildTarget};

/// Fakeable seam for the whole ready flow. Implementations may hold a fake
/// store and script probe, inspect, build, failure and cancellation outcomes.
pub trait LocalImageBuilder: Send + Sync {
    fn probe(&self, target: &BuildTarget, control: &BuildControl) -> Result<(), EngineError>;
    fn inspect(
        &self,
        target: &BuildTarget,
        reference: &str,
        platform: &OciPlatform,
        control: &BuildControl,
    ) -> Result<SourceImage, EngineError>;
    fn build(
        &self,
        request: &BuildRequest,
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
    #[cfg(test)]
    pub(crate) fn with_runner(runner: Arc<dyn HostCommandRunner>) -> Self {
        Self { runner }
    }
}

impl HostLocalImageBuilder {
    fn bound_command(target: &BuildTarget) -> Command {
        match target {
            BuildTarget::Runtime => Command::new(crate::engine::host_cli::program("docker")),
            BuildTarget::Docker { endpoint } => Self::bound_docker_command(endpoint),
            BuildTarget::Apple => Command::new(crate::engine::host_cli::program("container")),
        }
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
impl HostLocalImageBuilder {
    /// Run one builder CLI command. A missing CLI or a failed process means
    /// the builder is unavailable; it is never reported as the selected
    /// runtime missing (which would advise installing Docker and exit 3).
    fn run_host(
        &self,
        target: &BuildTarget,
        command: Command,
        control: &BuildControl,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<std::process::ExitStatus, EngineError> {
        self.runner
            .run(command, control, on_line)
            .map_err(|e| match e {
                EngineError::ContainerRuntimeUnavailable { binary } => {
                    EngineError::BuilderUnavailable {
                        builder: target.describe(),
                        detail: format!("`{binary}` was not found on PATH"),
                    }
                }
                EngineError::Container(detail) => EngineError::BuilderUnavailable {
                    builder: target.describe(),
                    detail,
                },
                other => other,
            })
    }
}

impl HostLocalImageBuilder {
    pub fn classify_docker_inspect(
        json: &str,
        platform: &OciPlatform,
    ) -> Result<SourceImage, EngineError> {
        classify_docker_inspect_for_store(json, platform, false)
    }
}

fn classify_docker_inspect_for_store(
    json: &str,
    platform: &OciPlatform,
    containerd_store: bool,
) -> Result<SourceImage, EngineError> {
    let inspect: DockerInspect = serde_json::from_str(json).map_err(|e| {
        EngineError::ImageBuilder(BuilderFailure::UnreadableInspection {
            detail: e.to_string(),
        })
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
        target: &BuildTarget,
        control: &BuildControl,
    ) -> Result<bool, EngineError> {
        let mut command = Self::bound_command(target);
        command.args(["info", "--format", "{{json .DriverStatus}}"]);
        let mut lines = Vec::new();
        let status = self.run_host(
            target,
            command,
            &control.bounded(PROBE_TIMEOUT),
            &mut |line| {
                lines.push(line.to_string());
            },
        )?;
        if !status.success() {
            return Err(EngineError::ImageBuilder(
                BuilderFailure::ImageStoreUnknown {
                    builder: target.describe(),
                    exit_code: status.code().unwrap_or(-1),
                },
            ));
        }
        let raw = lines
            .iter()
            .find(|line| line.trim_start().starts_with('['))
            .ok_or_else(|| {
                EngineError::ImageBuilder(BuilderFailure::ImageStoreUnreported {
                    builder: target.describe(),
                })
            })?;
        let status: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
            EngineError::ImageBuilder(BuilderFailure::ImageStoreUnreadable {
                builder: target.describe(),
                detail: e.to_string(),
            })
        })?;
        Ok(status.to_string().contains("io.containerd.snapshotter.v1"))
    }

    fn inspect_docker(
        &self,
        target: &BuildTarget,
        reference: &str,
        platform: &OciPlatform,
        control: &BuildControl,
    ) -> Result<SourceImage, EngineError> {
        let mut command = Self::bound_command(target);
        command.args(["image", "inspect", "--format", "{{json .}}", reference]);
        let mut lines = Vec::new();
        let status = self.run_host(
            target,
            command,
            &control.bounded(INSPECT_TIMEOUT),
            &mut |line| lines.push(line.to_string()),
        )?;
        if !status.success() {
            let missing = lines
                .iter()
                .any(|l| l.contains("No such image") || l.contains("No such object"));
            return if missing {
                Ok(SourceImage::Absent)
            } else {
                Err(EngineError::ImageBuilder(BuilderFailure::InspectFailed {
                    builder: target.describe(),
                    reference: reference.to_string(),
                    exit_code: status.code().unwrap_or(-1),
                    output: tail(&lines),
                }))
            };
        }
        let json = lines
            .iter()
            .find(|line| line.trim_start().starts_with('{'))
            .ok_or_else(|| {
                EngineError::ImageBuilder(BuilderFailure::InspectionMissing {
                    builder: target.describe(),
                    reference: reference.to_string(),
                })
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
    fn probe(&self, target: &BuildTarget, control: &BuildControl) -> Result<(), EngineError> {
        let mut command = Self::bound_command(target);
        let what = match target {
            BuildTarget::Runtime => {
                return Err(EngineError::Config(
                    "source builder requires an explicit source store".into(),
                ))
            }
            BuildTarget::Docker { .. } => {
                command.arg("info");
                "docker info"
            }
            BuildTarget::Apple => {
                command.args(["system", "status"]);
                "container system status"
            }
        };
        let mut lines = Vec::new();
        let status = self.run_host(
            target,
            command,
            &control.bounded(PROBE_TIMEOUT),
            &mut |line| {
                lines.push(line.to_string());
                if lines.len() > 50 {
                    lines.remove(0);
                }
            },
        )?;
        if status.success() {
            Ok(())
        } else {
            Err(EngineError::BuilderUnavailable {
                builder: target.describe(),
                detail: format!(
                    "`{what}` exited {}: {}",
                    status.code().unwrap_or(-1),
                    tail(&lines)
                ),
            })
        }
    }

    fn inspect(
        &self,
        target: &BuildTarget,
        reference: &str,
        platform: &OciPlatform,
        control: &BuildControl,
    ) -> Result<SourceImage, EngineError> {
        self.probe(target, control)?;
        match target {
            BuildTarget::Runtime => Err(EngineError::Config(
                "source builder requires an explicit source store".into(),
            )),
            BuildTarget::Apple => {
                let transport =
                    crate::engine::oci::apple_store::installed_transport().ok_or_else(|| {
                        EngineError::BuilderUnavailable {
                            builder: target.describe(),
                            detail: "the Apple Containers image transport is unavailable".into(),
                        }
                    })?;
                crate::engine::oci::apple_store::inspect_local(
                    transport.as_ref(),
                    reference,
                    platform,
                    &control.bounded(INSPECT_TIMEOUT),
                )
            }
            BuildTarget::Docker { .. } => self.inspect_docker(target, reference, platform, control),
        }
    }

    fn build(
        &self,
        request: &BuildRequest,
        control: &BuildControl,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<BuiltImageIdentity, EngineError> {
        self.probe(&request.target, control)?;
        match &request.target {
            BuildTarget::Runtime => Err(EngineError::Config(
                "source builder requires an explicit source store".into(),
            )),
            BuildTarget::Docker { .. } => {
                let image_id = self.build_local_docker(request, control, on_line)?;
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
                    SourceImage::Present(_) => Err(EngineError::ImageBuilder(
                        BuilderFailure::BuiltImageChanged {
                            builder: request.target.describe(),
                            image_id: image_id.to_string(),
                            reference: request.reference.clone(),
                        },
                    )),
                    SourceImage::Unbound => Err(EngineError::ImageBuilder(
                        BuilderFailure::MultiPlatformOutput {
                            builder: request.target.describe(),
                            reference: request.reference.clone(),
                            platform: request.platform.to_string(),
                        },
                    )),
                    SourceImage::Absent => Err(EngineError::ImageBuilder(
                        BuilderFailure::BuiltImageMissing {
                            builder: request.target.describe(),
                            image_id: image_id.to_string(),
                            reference: request.reference.clone(),
                            platform: request.platform.to_string(),
                        },
                    )),
                }
            }
            BuildTarget::Apple => {
                let mut command = Self::bound_command(&request.target);
                let platform = request.platform.to_string();
                let argv = crate::engine::container::host_build::BuildArgv {
                    platform: Some(&platform),
                    build_args: &request.build_args,
                    ..crate::engine::container::host_build::BuildArgv::new(
                        &request.reference,
                        &request.dockerfile,
                        &request.context,
                        request.no_cache,
                    )
                };
                command.args(argv.to_args());
                let mut produced_manifest = None;
                let status = self.run_host(&request.target, command, control, &mut |line| {
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
                    EngineError::ImageBuilder(BuilderFailure::AppleManifestUnreported)
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
                        return Err(EngineError::ImageBuilder(BuilderFailure::AppleImageMissing))
                    }
                };
                if installed.manifest_digest() != Some(&produced_manifest) {
                    return Err(EngineError::ImageBuilder(
                        BuilderFailure::AppleImageChanged {
                            reference: request.reference.clone(),
                        },
                    ));
                }
                Ok(installed)
            }
        }
    }
}

impl HostLocalImageBuilder {
    pub(crate) fn bound_docker_command(endpoint: &str) -> Command {
        let mut command = Command::new(crate::engine::host_cli::program("docker"));
        command.args(["--host", endpoint]);
        for key in DOCKER_AMBIENT_ENV {
            command.env_remove(key);
        }
        command.env("DOCKER_HOST", endpoint);
        command
    }
}

impl HostLocalImageBuilder {
    pub(crate) fn build_local_docker(
        &self,
        request: &crate::engine::container::BuildRequest,
        control: &BuildControl,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<Digest, EngineError> {
        let crate::engine::container::BuildTarget::Docker { endpoint } = &request.target else {
            return Err(EngineError::Config(
                "source-bound Docker build needs a Docker endpoint".into(),
            ));
        };
        let path = self.docker_build_path(endpoint, control)?;
        let iid = tempfile::NamedTempFile::new().map_err(|e| {
            EngineError::ImageBuilder(BuilderFailure::ImageIdFileCreate {
                detail: e.to_string(),
            })
        })?;
        let platform = request.platform.to_string();
        let argv = BuildArgv {
            buildx: matches!(path, DockerBuildPath::BuildKit),
            platform: Some(&platform),
            iidfile: Some(iid.path()),
            build_args: &request.build_args,
            ..BuildArgv::new(
                &request.reference,
                &request.dockerfile,
                &request.context,
                request.no_cache,
            )
        };
        let mut command = Self::bound_docker_command(endpoint);
        command.args(argv.to_args());
        let status = self.run_host(&request.target, command, control, on_line)?;
        if !status.success() {
            return Err(EngineError::ImageBuildExitNonzero {
                tag: request.reference.clone(),
                exit_code: status.code().unwrap_or(-1),
            });
        }
        let raw = std::fs::read_to_string(iid.path()).map_err(|e| {
            EngineError::ImageBuilder(BuilderFailure::ImageIdFileRead {
                detail: e.to_string(),
            })
        })?;
        Digest::parse(raw.trim()).map_err(|e| {
            EngineError::ImageBuilder(BuilderFailure::ImageIdFileInvalid {
                detail: e.to_string(),
            })
        })
    }
}

impl HostLocalImageBuilder {
    fn docker_build_path(
        &self,
        endpoint: &str,
        control: &BuildControl,
    ) -> Result<DockerBuildPath, EngineError> {
        let probe = control.bounded(PROBE_TIMEOUT);
        let mut version = Self::bound_docker_command(endpoint);
        version.args(["buildx", "version"]);
        let target = BuildTarget::Docker {
            endpoint: endpoint.to_string(),
        };
        if !self
            .run_host(&target, version, &probe, &mut |_| {})?
            .success()
        {
            return Ok(DockerBuildPath::Legacy);
        }
        let mut inspect = Self::bound_docker_command(endpoint);
        inspect.args(["buildx", "inspect", "default"]);
        let mut lines = Vec::new();
        let status = self.run_host(&target, inspect, &probe, &mut |line| {
            lines.push(line.to_string())
        })?;
        let driver = lines.iter().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == "Driver").then(|| value.trim().to_string())
        });
        match (status.success(), driver.as_deref()) {
            (true, Some("docker")) => Ok(DockerBuildPath::BuildKit),
            (true, other) => Err(EngineError::ImageBuilder(
                BuilderFailure::UnsupportedBuildxDriver {
                    endpoint: endpoint.to_string(),
                    driver: other.unwrap_or("unknown").to_string(),
                },
            )),
            (false, _) => Err(EngineError::ImageBuilder(
                BuilderFailure::BuildxInspectFailed {
                    endpoint: endpoint.to_string(),
                    exit_code: status.code().unwrap_or(-1),
                    output: tail(&lines),
                },
            )),
        }
    }
}

/// Which Docker build path a bound endpoint uses, decided the way the
/// `docker` CLI decides it: BuildKit through buildx when buildx is
/// installed, the legacy builder otherwise. awman never downgrades a
/// buildx-capable CLI to the legacy builder.
enum DockerBuildPath {
    /// buildx is installed and the endpoint's `default` builder uses the
    /// `docker` driver, so its output is stored in that same Engine.
    BuildKit,
    /// No buildx: `docker build` uses the legacy builder, as it would for
    /// the Docker runtime on this host.
    Legacy,
}
/// Environment that could redirect a Docker command away from the endpoint
/// it is bound to.
const DOCKER_AMBIENT_ENV: &[&str] = &[
    "DOCKER_CONTEXT",
    "DOCKER_TLS_VERIFY",
    "DOCKER_CERT_PATH",
    "BUILDX_BUILDER",
];

#[cfg(test)]
mod tests {
    use super::*;
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
        let target = BuildTarget::Docker {
            endpoint: "unix:///run/explicit.sock".into(),
        };
        let image = builder
            .inspect(
                &target,
                "agent:latest",
                &OciPlatform::host_linux(),
                &BuildControl::source_build(crate::engine::oci::CancelToken::new()),
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
        match HostLocalImageBuilder::classify_docker_inspect(&json, &platform).unwrap() {
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
        match HostLocalImageBuilder::classify_docker_inspect(&json, &platform).unwrap() {
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
        match HostLocalImageBuilder::classify_docker_inspect(&json, &platform).unwrap() {
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
            HostLocalImageBuilder::classify_docker_inspect(&json, &platform).unwrap(),
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
            HostLocalImageBuilder::classify_docker_inspect(&json, &platform).unwrap(),
            SourceImage::Absent
        );
    }

    /// Unreadable JSON from `docker image inspect` is an error, not a
    /// silent `Absent` (which would trigger an unnecessary rebuild) or a
    /// silent `Present` (which would claim unverified content).
    #[test]
    fn classify_docker_inspect_malformed_json_is_an_error() {
        let platform = host_platform();
        assert!(HostLocalImageBuilder::classify_docker_inspect("not json", &platform).is_err());
    }
}
