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
    /// Local metadata, without acquiring or building an image. Unknown
    /// metadata must never be treated as a matching inputs label.
    fn inspect_metadata(
        &self,
        _target: &BuildTarget,
        _reference: &str,
        _control: &BuildControl,
    ) -> Result<Option<crate::engine::container::runtime::ImageMetadata>, EngineError> {
        Err(EngineError::NotImplemented("source image metadata"))
    }
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
    env: crate::data::config::env::EnvSnapshot,
    runner: Arc<dyn HostCommandRunner>,
}

impl Default for HostLocalImageBuilder {
    fn default() -> Self {
        Self {
            runner: Arc::new(ProcessHostCommandRunner),
            env: crate::engine::host_cli::process_env(),
        }
    }
}

impl HostLocalImageBuilder {
    pub fn with_env(env: crate::data::config::env::EnvSnapshot) -> Self {
        Self {
            env,
            runner: Arc::new(ProcessHostCommandRunner),
        }
    }
}

impl HostLocalImageBuilder {
    fn run_build(
        &self,
        request: &BuildRequest,
        command: Command,
        control: &BuildControl,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        let output = crate::engine::agent_runtime::OutputTail::new(20);
        let status = self.run_host(&request.target, command, control, &mut |line| {
            output.push_bytes(line.as_bytes());
            output.push_bytes(b"\n");
            on_line(line);
        })?;
        if !status.success() {
            return Err(EngineError::ImageBuildExitNonzero {
                tag: request.reference.clone(),
                exit_code: status.code().unwrap_or(-1),
                output: output.snapshot_text(),
            });
        }
        Ok(())
    }

    fn bound_command(&self, target: &BuildTarget) -> Command {
        match target {
            BuildTarget::Runtime => crate::engine::host_cli::command(&self.env, "docker"),
            BuildTarget::Docker { endpoint } => self.docker_command(endpoint),
            BuildTarget::Apple => crate::engine::host_cli::command(&self.env, "container"),
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
        let status = self
            .runner
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
            })?;
        if !status.success() {
            if let BuildTarget::Docker { endpoint } = target {
                crate::engine::oci::docker_engine::check_local_connection(endpoint, control)?;
            }
        }
        Ok(status)
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
        let mut command = self.bound_command(target);
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
        let mut command = self.bound_command(target);
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
    fn inspect_metadata(
        &self,
        target: &BuildTarget,
        reference: &str,
        control: &BuildControl,
    ) -> Result<Option<crate::engine::container::runtime::ImageMetadata>, EngineError> {
        let mut command = self.bound_command(target);
        command.args(["image", "inspect", reference]);
        let mut lines = Vec::new();
        let status = self.run_host(
            target,
            command,
            &control.bounded(INSPECT_TIMEOUT),
            &mut |line| lines.push(line.to_owned()),
        )?;
        if !status.success() {
            let error = lines.join("\n").to_ascii_lowercase();
            if error.contains("no such image") || error.contains("not found") {
                return Ok(None);
            }
            return Err(EngineError::Container(
                "source image metadata inspection failed".into(),
            ));
        }
        crate::engine::container::host_build::parse_image_inspection(
            lines.join("\n").as_bytes(),
            reference,
            *target == BuildTarget::Apple,
        )
        .map(Some)
    }
    fn probe(&self, target: &BuildTarget, control: &BuildControl) -> Result<(), EngineError> {
        let mut command = self.bound_command(target);
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
        let request = super::freshness::label_build(request, &self.env, |reference| {
            if let Some(image) = self.inspect_metadata(&request.target, reference, control)? {
                return Ok(Some(image.id));
            }
            match self.inspect(&request.target, reference, &request.platform, control)? {
                SourceImage::Present(id) => Ok(id
                    .config_digest()
                    .or(id.manifest_digest())
                    .map(ToString::to_string)),
                _ => Ok(None),
            }
        })?;
        let request = &request;
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
                let mut command = self.bound_command(&request.target);
                let platform = request.platform.to_string();
                let argv = crate::engine::container::host_build::BuildArgv {
                    platform: Some(&platform),
                    build_args: &request.build_args,
                    labels: &request.labels,
                    ..crate::engine::container::host_build::BuildArgv::new(
                        &request.reference,
                        &request.dockerfile,
                        &request.context,
                        request.no_cache,
                    )
                };
                command.args(argv.to_args());
                let mut produced_manifest = None;
                self.run_build(request, command, control, &mut |line| {
                    if let Some(marker) = line.find("exporting manifest sha256:") {
                        let digest = line[marker + "exporting manifest ".len()..]
                            .split_whitespace()
                            .next()
                            .unwrap_or("");
                        produced_manifest = Digest::parse(digest).ok();
                    }
                    on_line(line);
                })?;
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
    fn docker_command(&self, endpoint: &str) -> Command {
        let mut command = crate::engine::host_cli::command(&self.env, "docker");
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
            labels: &request.labels,
            ..BuildArgv::new(
                &request.reference,
                &request.dockerfile,
                &request.context,
                request.no_cache,
            )
        };
        let mut command = self.docker_command(endpoint);
        command.args(argv.to_args());
        self.run_build(request, command, control, on_line)?;
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
        let mut version = self.docker_command(endpoint);
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
        let mut inspect = self.docker_command(endpoint);
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

#[cfg(all(test, unix))]
mod contracts {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    struct Inspection(serde_json::Value);
    impl HostCommandRunner for Inspection {
        fn run(
            &self,
            command: Command,
            _: &BuildControl,
            on_line: &mut dyn FnMut(&str),
        ) -> Result<std::process::ExitStatus, EngineError> {
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                ["image", "inspect", "project:latest"]
            );
            for line in serde_json::to_string_pretty(&self.0).unwrap().lines() {
                on_line(line);
            }
            Ok(std::process::ExitStatus::from_raw(0))
        }
    }

    #[test]
    fn apple_metadata_accepts_legacy_index_and_current_manifest_without_confusing_receipts() {
        use crate::data::image_inputs::{InputsHash, INPUTS_LABEL};
        use crate::engine::image_prep::ImageFreshness;
        let index = format!("sha256:{}", "1".repeat(64));
        let manifest = format!("sha256:{}", "2".repeat(64));
        let variant = serde_json::json!({
            "platform": {"os": "linux", "architecture": OciPlatform::host_linux().architecture},
            "size": 123,
            "config": {"config": {"Labels": {INPUTS_LABEL: "inputs"}}}
        });
        // Apple 0.12: ClientImage.details() -> ImageDetail; no variant digest.
        let legacy = serde_json::json!([{
            "name": "project:latest", "index": {"digest": index},
            "variants": [variant.clone()]
        }]);
        let mut current_variant = variant;
        current_variant["digest"] = manifest.clone().into();
        // Apple 1.4/1.5: ImageResource with the selected manifest's digest.
        let current = serde_json::json!([{
            "configuration": {"name": "project:latest", "descriptor": {"digest": index}},
            "variants": [
                {"platform": {"os": "unknown", "architecture": "unknown"}},
                current_variant
            ]
        }]);
        let control = BuildControl::source_build(crate::engine::oci::CancelToken::new());
        for (json, expected_id, expected_manifest) in [
            (legacy.clone(), index.as_str(), None),
            (
                current,
                manifest.as_str(),
                Some(Digest::parse(&manifest).unwrap()),
            ),
        ] {
            let builder = HostLocalImageBuilder {
                env: crate::data::config::env::EnvSnapshot::from_full_env(Default::default()),
                runner: Arc::new(Inspection(json)),
            };
            let image = builder
                .inspect_metadata(&BuildTarget::Apple, "project:latest", &control)
                .unwrap()
                .unwrap();
            assert_eq!(image.id, expected_id);
            assert_eq!(image.manifest_digest, expected_manifest);
            assert_eq!(image.tags, ["project:latest"]);
            assert_eq!(image.size_bytes, 123);
            assert!(matches!(
                ImageFreshness::from_image(Some(image), &InputsHash::from_label("inputs")),
                ImageFreshness::Fresh(_)
            ));
        }
        for (pointer, value, reason) in [
            (
                "/0/index/digest",
                serde_json::Value::Null,
                "missing image identity",
            ),
            (
                "/0/variants/0/platform/os",
                "windows".into(),
                "no variant for the host Linux platform",
            ),
            (
                "/0/variants/0/digest",
                "broken".into(),
                "invalid variant manifest digest",
            ),
            (
                "/0/variants/0/config/config/Labels",
                serde_json::json!({"key": 1}),
                "labels must be a string map",
            ),
        ] {
            let mut json = legacy.clone();
            if pointer.ends_with("/0/digest") {
                json[0]["variants"][0]["digest"] = value;
            } else {
                *json.pointer_mut(pointer).unwrap() = value;
            }
            let builder = HostLocalImageBuilder {
                env: crate::data::config::env::EnvSnapshot::from_full_env(Default::default()),
                runner: Arc::new(Inspection(json)),
            };
            let error = builder
                .inspect_metadata(&BuildTarget::Apple, "project:latest", &control)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("project:latest (Apple Containers)"),
                "{error}"
            );
            assert!(error.contains(reason), "{error}");
        }
    }

    struct Failing;
    impl HostCommandRunner for Failing {
        fn run(
            &self,
            _: Command,
            _: &BuildControl,
            on_line: &mut dyn FnMut(&str),
        ) -> Result<std::process::ExitStatus, EngineError> {
            for i in 0..40 {
                on_line(&format!("build progress {i}"));
            }
            on_line("stderr: invalid build option");
            Ok(std::process::ExitStatus::from_raw(64 << 8))
        }
    }
    #[test]
    fn stopped_source_matched_docker_builder_keeps_socket_diagnosis() {
        use awman_test_support::{TestCase, TestSandbox};
        let s = TestSandbox::for_test(TestCase::component("engine::image_prep::builder::contracts::stopped_source_matched_docker_builder_keeps_socket_diagnosis"));
        let endpoint = format!("unix://{}", s.ports().unix_socket("missing.sock").display());
        let target = BuildTarget::Docker {
            endpoint: endpoint.clone(),
        };
        let builder = HostLocalImageBuilder {
            env: crate::data::config::env::EnvSnapshot::empty(),
            runner: Arc::new(Failing),
        };
        let control = BuildControl::source_build(crate::engine::oci::CancelToken::new());
        let request = BuildRequest {
            labels: Default::default(),
            target: target.clone(),
            reference: "agent:latest".into(),
            dockerfile: s.work().join("Dockerfile"),
            context: s.work(),
            platform: OciPlatform::host_linux(),
            build_args: vec![],
            no_cache: false,
        };
        for error in [
            builder.probe(&target, &control).unwrap_err(),
            builder
                .inspect_metadata(&target, "agent:latest", &control)
                .unwrap_err(),
            builder
                .run_build(
                    &request,
                    builder.bound_command(&target),
                    &control,
                    &mut |_| {},
                )
                .unwrap_err(),
        ] {
            assert!(
                matches!(
                    crate::engine::oci::docker_engine::local_unavailability(&error),
                    Some(crate::engine::oci::docker_engine::LocalDockerUnavailable::Stopped { .. })
                ),
                "{error}"
            );
            assert!(!crate::engine::oci::retry::is_retryable(&error));
            assert!(error.to_string().contains(&endpoint));
            assert!(error
                .to_string()
                .contains("Start Docker, then run `awman ready` again."));
        }
        s.finish();
    }

    #[tokio::test]
    async fn stopped_docker_builder_reuses_installed_image_with_warning_but_required_build_fails() {
        use crate::data::{config::image_source::ImageSourceSpec, step_status::StepStatus};
        use crate::engine::image_prep::{
            contracts::{identity, preparer, Sink, Store},
            AgentImageRequest, BuildPolicy,
        };
        use awman_test_support::{TestCase, TestSandbox};
        let s = TestSandbox::for_test(TestCase::component("engine::image_prep::builder::contracts::stopped_docker_builder_reuses_installed_image_with_warning_but_required_build_fails"));
        let endpoint = format!("unix://{}", s.ports().unix_socket("missing.sock").display());
        let source = ImageSourceSpec::DockerStore {
            host: Some(endpoint.clone()),
            tls: None,
            reference: None,
        };
        let store = Arc::new(Store::default());
        let tag = crate::data::image_tags::agent_image_tag(&s.work(), "claude");
        let mut installed = identity(&tag);
        installed.source = crate::data::config::image_source::ImageSourceKind::DockerStore;
        store
            .installed
            .lock()
            .unwrap()
            .insert(tag.clone(), installed);
        store
            .installed_sources
            .lock()
            .unwrap()
            .insert(tag.clone(), source.clone());
        let mut prep = preparer(&s, store.clone(), true, Some(source));
        prep.local_builder = Some(Arc::new(HostLocalImageBuilder {
            env: crate::data::config::env::EnvSnapshot::empty(),
            runner: Arc::new(Failing),
        }));
        let result = prep
            .prepare(
                &AgentImageRequest {
                    tag: &tag,
                    agent: "claude",
                    policy: BuildPolicy::SessionSetup,
                },
                &mut Sink::default(),
            )
            .await
            .unwrap();
        assert!(
            matches!(result.build, StepStatus::Warn(ref text) if text.contains("Docker is not running")),
            "{result:?}"
        );
        assert_eq!(result.import, StepStatus::Done);
        let StepStatus::Warn(text) = &result.build else {
            panic!("{result:?}")
        };
        assert_eq!(
            text,
            &format!(
                "Docker is not running at {endpoint}. Start Docker, then run `awman ready` again."
            )
        );
        assert!(store.imports.lock().unwrap().is_empty());
        // Explicit ready must fail even with an installed matching image.
        let result = prep
            .prepare(
                &AgentImageRequest {
                    tag: &tag,
                    agent: "claude",
                    policy: BuildPolicy::Ready {
                        build: false,
                        no_cache: false,
                        refresh: false,
                    },
                },
                &mut Sink::default(),
            )
            .await
            .unwrap();
        assert!(result.has_failure(), "{result:?}");
        assert_eq!(result.build, StepStatus::Failed(text.clone()));
        assert_eq!(result.import, StepStatus::Skipped);
        assert!(store.imports.lock().unwrap().is_empty());
        assert!(store.built.lock().unwrap().is_empty());
        store.installed.lock().unwrap().clear();
        let result = prep
            .prepare(
                &AgentImageRequest {
                    tag: &tag,
                    agent: "claude",
                    policy: BuildPolicy::SessionSetup,
                },
                &mut Sink::default(),
            )
            .await
            .unwrap();
        assert!(
            matches!(result.build, StepStatus::Failed(ref text) if text.contains("Docker is not running")),
            "{result:?}"
        );
        assert_eq!(result.import, StepStatus::Skipped);
        assert!(store.imports.lock().unwrap().is_empty());
        assert!(store.built.lock().unwrap().is_empty());
        s.finish();
    }

    /// The import seam invokes production acquisition against only the selected
    /// private socket. A successful acquire would be a test fixture failure.
    struct StoppedImporter {
        acquirer: crate::engine::oci::CachingAcquirer,
        endpoint: String,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[tokio::test]
    async fn default_socket_diagnosis_survives_a_source_matched_builder() {
        use crate::data::{config::image_source::ImageSourceSpec, step_status::StepStatus};
        use crate::engine::image_prep::{
            contracts::{preparer, Sink, Store},
            AgentImageRequest, BuildPolicy,
        };
        use crate::engine::oci::docker_engine::Endpoint;
        use awman_test_support::{TestCase, TestSandbox};
        let s = TestSandbox::for_test(TestCase::component("engine::image_prep::builder::contracts::default_socket_diagnosis_survives_a_source_matched_builder"));
        let path = s.ports().unix_socket("default.sock");
        let endpoint = format!("unix://{}", path.display());
        let store = Arc::new(Store::default());
        let source = ImageSourceSpec::DockerStore {
            host: Some(endpoint.clone()),
            tls: None,
            reference: None,
        };
        let mut prep = preparer(&s, store.clone(), true, Some(source));
        let tag = crate::data::image_tags::agent_image_tag(&s.work(), "claude");
        // Inject only the platform default's path, keeping every actual socket
        // connect in this sandbox. The frozen builder host stays explicit.
        let mut plan = prep.plan(&tag, "claude");
        plan.source.as_mut().unwrap().docker_endpoint = Some(Endpoint::DefaultUnix(path));
        prep.plans.insert((tag.clone(), "claude".into()), plan);
        prep.local_builder = Some(Arc::new(HostLocalImageBuilder {
            env: crate::data::config::env::EnvSnapshot::empty(),
            runner: Arc::new(Failing),
        }));
        let result = prep
            .prepare(
                &AgentImageRequest {
                    tag: &tag,
                    agent: "claude",
                    policy: BuildPolicy::Ready {
                        build: false,
                        no_cache: false,
                        refresh: false,
                    },
                },
                &mut Sink::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.build, StepStatus::Failed(format!("No Docker daemon at the default socket {endpoint}. Start Docker, or set the source's `host` (or DOCKER_HOST) if Docker uses another socket/context, then run `awman ready` again.")));
        assert_eq!(result.import, StepStatus::Skipped);
        assert!(store.imports.lock().unwrap().is_empty());
        assert!(store.built.lock().unwrap().is_empty());
        // Build completion reports the same normalized, origin-aware reason
        // when a daemon fails during a build rather than the freshness probe.
        #[derive(Default)]
        struct Completion(Option<String>);
        impl crate::data::message::UserMessageSink for Completion {
            fn write_message(&mut self, _: crate::data::message::UserMessage) {}
            fn replay_queued(&mut self) {}
        }
        impl crate::engine::image_prep::ImagePreparationSink for Completion {
            fn report_step(&mut self, _: &crate::data::setup_step::SetupStep, _: StepStatus) {}
            fn build_finished(&mut self, _: &str, error: Option<&str>) {
                self.0 = error.map(str::to_string);
            }
        }
        let resolved = prep.plan(&tag, "claude").source.unwrap();
        let request = BuildRequest {
            labels: Default::default(),
            target: BuildTarget::Docker { endpoint },
            reference: tag,
            dockerfile: s.work().join("Dockerfile.dev"),
            context: s.work(),
            platform: OciPlatform::host_linux(),
            build_args: vec![],
            no_cache: false,
        };
        let mut completion = Completion::default();
        prep.build_source(request, resolved.docker_endpoint.as_ref(), &mut completion)
            .await
            .unwrap_err();
        let StepStatus::Failed(reason) = result.build else {
            unreachable!()
        };
        assert_eq!(completion.0, Some(reason));
        s.finish();
    }
    impl crate::engine::image_prep::ImageImporter for StoppedImporter {
        fn import(
            &self,
            request: &crate::engine::agent_runtime::ImageImportRequest,
            _: &crate::engine::oci::CancelToken,
            _: &mut dyn crate::data::message::UserMessageSink,
        ) -> Result<crate::engine::agent_runtime::ImportedImage, EngineError> {
            use crate::data::config::image_source::ImageSourceSpec;
            use crate::engine::oci::{AcquirePolicy, AcquireRequest, ImageAcquirer};
            assert!(
                matches!(&request.source, ImageSourceSpec::DockerStore { host: Some(host), .. } if host == &self.endpoint)
            );
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.acquirer.acquire(
                &AcquireRequest {
                    tag: request.tag.clone(),
                    source: request.source.clone(),
                    platform: request.platform.clone(),
                    policy: AcquirePolicy::IfMissing,
                    registries: request.registries.clone(),
                },
                &mut |_| {},
            )?;
            panic!("stopped socket must fail acquisition")
        }
    }

    #[tokio::test]
    async fn stopped_docker_import_fails_once_without_building_or_falling_back() {
        use crate::data::{config::image_source::ImageSourceSpec, step_status::StepStatus};
        use crate::engine::image_prep::{
            contracts::{preparer, Sink, Store},
            AgentImageRequest, BuildPolicy,
        };
        use crate::engine::oci::{AcquireLimits, CachingAcquirer, DiskSpace};
        use awman_test_support::{TestCase, TestSandbox};
        struct Space;
        impl DiskSpace for Space {
            fn available(&self, _: &std::path::Path) -> Option<u64> {
                Some(u64::MAX)
            }
        }
        let s = TestSandbox::for_test(TestCase::component("engine::image_prep::builder::contracts::stopped_docker_import_fails_once_without_building_or_falling_back"));
        let endpoint = format!("unix://{}", s.ports().unix_socket("missing.sock").display());
        let source = ImageSourceSpec::DockerStore {
            host: Some(endpoint.clone()),
            tls: None,
            reference: None,
        };
        let importer = Arc::new(StoppedImporter {
            acquirer: CachingAcquirer::new(
                &s.work().join("cache"),
                AcquireLimits {
                    min_free_bytes: 0,
                    ..Default::default()
                },
                crate::data::config::env::EnvSnapshot::empty(),
                Arc::new(Space),
            ),
            endpoint: endpoint.clone(),
            calls: Default::default(),
        });
        for (enabled, policy, default_socket) in [
            (true, BuildPolicy::ImportOnly, false),
            (true, BuildPolicy::ImportOnly, true),
            (
                false,
                BuildPolicy::Ready {
                    build: false,
                    no_cache: false,
                    refresh: false,
                },
                false,
            ),
            (
                false,
                BuildPolicy::Ready {
                    build: false,
                    no_cache: false,
                    refresh: false,
                },
                true,
            ),
        ] {
            let store = Arc::new(Store::default());
            let mut prep = preparer(&s, store.clone(), enabled, Some(source.clone()));
            // Any builder probe fails the test, even before a build is attempted.
            prep.local_builder = Some(Arc::new(NoBuilder));
            prep.image_importer = importer.clone();
            let tag = crate::data::image_tags::agent_image_tag(&s.work(), "claude");
            if default_socket {
                let mut plan = prep.plan(&tag, "claude");
                plan.source.as_mut().unwrap().docker_endpoint =
                    Some(crate::engine::oci::docker_engine::Endpoint::DefaultUnix(
                        endpoint.strip_prefix("unix://").unwrap().into(),
                    ));
                prep.plans.insert((tag.clone(), "claude".into()), plan);
            }
            let result = prep
                .prepare(
                    &AgentImageRequest {
                        tag: &tag,
                        agent: "claude",
                        policy,
                    },
                    &mut Sink::default(),
                )
                .await
                .unwrap();
            assert!(result.has_failure());
            assert_eq!(result.build, StepStatus::Skipped);
            let expected = if default_socket {
                format!("No Docker daemon at the default socket {endpoint}. Start Docker, or set the source's `host` (or DOCKER_HOST) if Docker uses another socket/context, then run `awman ready` again.")
            } else {
                format!("Docker is not running at {endpoint}. Start Docker, then run `awman ready` again.")
            };
            assert_eq!(result.import, StepStatus::Failed(expected));
            assert!(store.imports.lock().unwrap().is_empty());
            assert!(store.built.lock().unwrap().is_empty());
        }
        assert_eq!(importer.calls.load(std::sync::atomic::Ordering::SeqCst), 4);
        s.finish();
    }

    struct NoBuilder;
    impl LocalImageBuilder for NoBuilder {
        fn probe(&self, _: &BuildTarget, _: &BuildControl) -> Result<(), EngineError> {
            panic!("unexpected builder probe")
        }
        fn inspect_metadata(
            &self,
            _: &BuildTarget,
            _: &str,
            _: &BuildControl,
        ) -> Result<Option<crate::engine::container::runtime::ImageMetadata>, EngineError> {
            panic!("unexpected builder metadata")
        }
        fn inspect(
            &self,
            _: &BuildTarget,
            _: &str,
            _: &OciPlatform,
            _: &BuildControl,
        ) -> Result<SourceImage, EngineError> {
            panic!("unexpected builder inspect")
        }
        fn build(
            &self,
            _: &BuildRequest,
            _: &BuildControl,
            _: &mut dyn FnMut(&str),
        ) -> Result<BuiltImageIdentity, EngineError> {
            panic!("unexpected builder build")
        }
    }
    #[test]
    fn apple_and_docker_build_failures_retain_bounded_output_without_hiding_streamed_lines() {
        let builder = HostLocalImageBuilder {
            env: crate::data::config::env::EnvSnapshot::from_full_env(Default::default()),
            runner: Arc::new(Failing),
        };
        for target in [
            BuildTarget::Apple,
            BuildTarget::Docker {
                // A remote endpoint keeps the CLI's failure; no local
                // socket diagnosis (or connection) applies to this test.
                endpoint: "tcp://127.0.0.1:2375".into(),
            },
        ] {
            let request = BuildRequest {
                labels: Default::default(),
                target,
                reference: "test:latest".into(),
                dockerfile: "Dockerfile".into(),
                context: ".".into(),
                platform: OciPlatform::host_linux(),
                build_args: vec![],
                no_cache: false,
            };
            let mut lines = vec![];
            let error = builder
                .run_build(
                    &request,
                    builder.bound_command(&request.target),
                    &BuildControl::source_build(crate::engine::oci::CancelToken::new()),
                    &mut |line| lines.push(line.to_string()),
                )
                .unwrap_err();
            assert_eq!(lines.len(), 41);
            let EngineError::ImageBuildExitNonzero {
                output, exit_code, ..
            } = &error
            else {
                panic!("wrong error")
            };
            assert_eq!(*exit_code, 64);
            assert_eq!(output.lines().count(), 20);
            assert!(error.to_string().contains("stderr: invalid build option"));
            assert!(!output.contains("build progress 0\n"));
        }
    }
}
