//! Rebuild stale project and agent images before a session (WI 0163).
//!
//! A session's named tags (`project_image_tag`, `agent_image_tag`, including a
//! squad task's `squad-<slug>` stem) carry the `dev.awman.inputs` label of the
//! working tree they were built from. Before a session or workflow launches,
//! [`refresh_stale_images`] hashes the working tree's inputs with Layer 0's
//! pure functions and compares them with the installed tags' labels. A
//! mismatch, or an image without the label, is rebuilt under the shared build
//! lease: the project image first, then each agent image, whose hash covers
//! the (possibly new) project image id.
//!
//! The policy is warn-and-continue. A rebuild that fails keeps the previous
//! image and reports a structured [`ImageStaleWarning`] through
//! [`UserMessageSink::report_image_stale`], so a session that repairs a
//! broken `Dockerfile.dev` can still run. A *missing* image is never handled
//! here: callers keep their existing missing-image behaviour (build it, or
//! fail with `AgentRequiresProjectImage`). `ready --build` does not use this
//! path and still fails hard.
//!
//! Only runtimes that build images themselves (Docker, Apple Containers) are
//! checked. Importing runtimes acquire images through their configured source
//! and are left to that path.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::data::config::effective::EffectiveConfig;
use crate::data::config::env::EnvSnapshot;
use crate::data::execution_event::{ImageStaleWarning, StaleImageKind};
use crate::data::image_build_lock::BuildStoreKey;
use crate::data::image_inputs::{
    agent_inputs_hash, project_inputs_at, DirBuildContext, InputsHash, INPUTS_LABEL,
};
use crate::data::image_tags::{agent_image_tag, project_image_tag};
use crate::data::message::{MessageLevel, UserMessage};
use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
use crate::engine::agent_runtime::{AgentRuntimeEngine, ImageAcquisition};
use crate::engine::container::runtime::ImageMetadata;
use crate::engine::container::BuildRequest;
use crate::engine::error::EngineError;
use crate::engine::image_prep::{
    AgentImagePreparer, ImageBuildContext, ImageBuildOutcome, ImageBuildSpec, ImageFreshness,
    ImagePreparationSink,
};

/// Longest `reason` carried by an `image_stale` warning.
const MAX_REASON_CHARS: usize = 300;

/// Where a session's images come from: the repository (or squad task
/// workspace) root and the project Dockerfile it builds with.
#[derive(Debug, Clone)]
pub struct SessionImageInputs {
    pub git_root: PathBuf,
    /// The configured project Dockerfile (`runtime.dockerfile` honoured).
    pub project_dockerfile: PathBuf,
    /// The environment used for repository attribution and the Docker
    /// endpoint that keys the build lease.
    pub env: EnvSnapshot,
    /// Root of the per-user build-lease directory.
    pub data_home: PathBuf,
}

impl SessionImageInputs {
    pub fn for_repo(git_root: &Path, config: &EffectiveConfig) -> Result<Self, EngineError> {
        Ok(Self {
            git_root: git_root.to_path_buf(),
            project_dockerfile: config.dockerfile_path(git_root),
            env: config.env().clone(),
            data_home: crate::data::fs::data_paths::data_home(config.env())?,
        })
    }
}

/// What one refresh did. Warnings have already been reported to the sink.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StaleImageRefresh {
    /// Tags rebuilt by this call (a concurrent caller's build is not listed).
    pub rebuilt: Vec<String>,
    /// Failed rebuilds; the previous image was kept for each.
    pub warnings: Vec<ImageStaleWarning>,
}

/// Rebuild the session's stale project image and the stale images of
/// `agents`. Missing images, missing Dockerfiles and runtimes that cannot
/// report image labels are left untouched. See the module docs.
///
/// Errors are reserved for failures that are not a build of a stale image
/// (for example a build lease that cannot be taken); a failed rebuild is a
/// warning in the returned value.
pub async fn refresh_stale_images<S>(
    runtime: Arc<dyn AgentRuntimeEngine>,
    inputs: &SessionImageInputs,
    agents: &[String],
    sink: &mut S,
) -> Result<StaleImageRefresh, EngineError>
where
    S: ImagePreparationSink + ?Sized,
{
    let mut refresh = StaleImageRefresh::default();
    if runtime.capabilities().image_acquisition != ImageAcquisition::Build {
        return Ok(refresh);
    }
    let preparer = AgentImagePreparer::for_runtime(
        runtime.clone(),
        inputs.data_home.clone(),
        ImageBuildContext {
            git_root: inputs.git_root.clone(),
            project_dockerfile: inputs.project_dockerfile.clone(),
            default_agent: String::new(),
        },
    );
    let store = runtime_build_store(runtime.as_ref(), &inputs.env);
    let repo = crate::engine::image_prep::repo_key(&inputs.git_root, &inputs.env);
    let root = inputs.git_root.as_path();

    // Project image first: every agent image is layered on it and keyed on
    // its id.
    let project_tag = project_image_tag(root);
    if let Some(dockerfile) = read_optional(&inputs.project_dockerfile)? {
        let expected = project_inputs_at(
            Some(&dockerfile),
            &DirBuildContext::new(root),
            &inputs
                .project_dockerfile
                .strip_prefix(root)
                .unwrap_or(&inputs.project_dockerfile)
                .to_string_lossy(),
        )
        .hash;
        if let Some(installed) = stale_image(&preparer, &project_tag, &expected, sink) {
            let request =
                BuildRequest::runtime(&project_tag, &inputs.project_dockerfile, root, false);
            rebuild(
                &preparer,
                ImageBuildSpec {
                    request,
                    store: store.clone(),
                    inputs: expected,
                    repo: repo.clone(),
                    force: false,
                },
                StaleTarget {
                    kind: StaleImageKind::Project,
                    agent: None,
                    installed,
                },
                &mut refresh,
                sink,
            )
            .await?;
        }
    }

    let paths = RepoDockerfilePaths::new(root);
    for agent in agents {
        let dockerfile = paths.agent_dockerfile(agent);
        let Some(bytes) = read_optional(&dockerfile)? else {
            continue;
        };
        // The project image actually installed now: rebuilt above, or the
        // previous one kept after a failed rebuild.
        let project_id = match runtime.inspect_image(&project_tag) {
            Ok(Some(image)) => image.id,
            Ok(None) => break,
            Err(error) => {
                skip_unreadable(&project_tag, &error, sink);
                break;
            }
        };
        let tag = agent_image_tag(root, agent);
        let expected = agent_inputs_hash(&bytes, &project_id);
        let Some(installed) = stale_image(&preparer, &tag, &expected, sink) else {
            continue;
        };
        let text = crate::data::image_inputs::bind_project_base(&bytes, &project_tag, &project_id)
            .map_err(EngineError::Config)?;
        let recipes = inputs.data_home.join("image-recipes");
        crate::data::fs::private_files::create_private_dir(&recipes)?;
        let pinned =
            tempfile::NamedTempFile::new_in(&recipes).map_err(|e| EngineError::io(&recipes, e))?;
        std::fs::write(pinned.path(), text).map_err(|e| EngineError::io(pinned.path(), e))?;
        let request = BuildRequest::runtime(&tag, pinned.path(), root, false);
        rebuild(
            &preparer,
            ImageBuildSpec {
                request,
                store: store.clone(),
                inputs: expected,
                repo: repo.clone(),
                force: false,
            },
            StaleTarget {
                kind: StaleImageKind::Agent,
                agent: Some(agent.clone()),
                installed,
            },
            &mut refresh,
            sink,
        )
        .await?;
    }
    Ok(refresh)
}

/// The build-lease key for a build into the runtime's own image store, so
/// concurrent session setups against one store share a single build.
pub fn runtime_build_store(runtime: &dyn AgentRuntimeEngine, env: &EnvSnapshot) -> BuildStoreKey {
    crate::engine::image_prep::freshness::runtime_store_key(runtime, env).unwrap_or_else(|_| {
        BuildStoreKey::Docker {
            endpoint: runtime.runtime_name().to_string(),
        }
    })
}

struct StaleTarget {
    kind: StaleImageKind,
    agent: Option<String>,
    /// The kept image's inputs label (`None` when unlabeled).
    installed: Option<String>,
}

/// `Some(installed label)` when `tag` is installed but stale or unlabeled.
fn stale_image<S: ImagePreparationSink + ?Sized>(
    preparer: &AgentImagePreparer,
    tag: &str,
    expected: &InputsHash,
    sink: &mut S,
) -> Option<Option<String>> {
    let label = |image: &ImageMetadata| image.labels.get(INPUTS_LABEL).cloned();
    match preparer.freshness(tag, expected) {
        Ok(ImageFreshness::Fresh(_) | ImageFreshness::Missing) => None,
        Ok(ImageFreshness::Stale(image)) => {
            sink.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!("Image {tag} is out of date with its Dockerfile; rebuilding…"),
            });
            Some(label(&image))
        }
        Ok(ImageFreshness::Unlabelled(_)) => {
            sink.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!(
                    "Image {tag} has no inputs label (built by an older awman); rebuilding…"
                ),
            });
            Some(None)
        }
        Err(error) => {
            skip_unreadable(tag, &error, sink);
            None
        }
    }
}

/// A runtime (or test double) without image metadata is not checked; any
/// other inspection failure is reported once and the session carries on with
/// the installed image, as it did before labels existed.
fn skip_unreadable<S: ImagePreparationSink + ?Sized>(tag: &str, error: &EngineError, sink: &mut S) {
    if matches!(
        error,
        EngineError::UnsupportedOnRuntime { .. } | EngineError::NotImplemented(_)
    ) {
        return;
    }
    sink.write_message(UserMessage {
        level: MessageLevel::Warning,
        text: format!("could not check whether image {tag} is up to date: {error}"),
    });
}

async fn rebuild<S: ImagePreparationSink + ?Sized>(
    preparer: &AgentImagePreparer,
    spec: ImageBuildSpec,
    target: StaleTarget,
    refresh: &mut StaleImageRefresh,
    sink: &mut S,
) -> Result<(), EngineError> {
    let tag = spec.request.reference.clone();
    let expected = spec.inputs.clone();
    match preparer.build_if_needed(spec, sink).await {
        Ok(ImageBuildOutcome::Built) => {
            refresh.rebuilt.push(tag);
            Ok(())
        }
        // Another session rebuilt it while this one waited for the lease.
        Ok(ImageBuildOutcome::Reused) => Ok(()),
        // Cancellation is not a build failure to warn about.
        Err(error @ EngineError::ImageAcquisitionCancelled) => Err(error),
        Err(error) => {
            let build = match target.kind {
                StaleImageKind::Project => format!("build project image {tag}"),
                StaleImageKind::Agent => format!(
                    "build agent image {tag} for '{}'",
                    target.agent.as_deref().unwrap_or_default()
                ),
            };
            let warning = ImageStaleWarning {
                build,
                image: tag,
                kind: target.kind,
                agent: target.agent,
                expected_inputs: expected.as_str().to_string(),
                installed_inputs: target.installed,
                reason: failure_reason(&error),
            };
            sink.report_image_stale(&warning);
            refresh.warnings.push(warning);
            Ok(())
        }
    }
}

/// A single, bounded line naming why a build failed: the exit code and the
/// last line of build output when there is one. Event frontends redact it
/// again before it leaves the process.
pub(crate) fn failure_reason(error: &EngineError) -> String {
    let text = match error {
        EngineError::ImageBuildExitNonzero {
            exit_code, output, ..
        } => match output.lines().map(str::trim).rfind(|line| !line.is_empty()) {
            Some(line) => format!("exit code {exit_code}: {line}"),
            None => format!("exit code {exit_code}"),
        },
        other => other
            .to_string()
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("build failed")
            .to_string(),
    };
    if text.chars().count() > MAX_REASON_CHARS {
        let mut cut: String = text.chars().take(MAX_REASON_CHARS - 1).collect();
        cut.push('…');
        cut
    } else {
        text
    }
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, EngineError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(EngineError::io(path, error)),
    }
}

#[cfg(test)]
mod failure_reason_tests {
    use super::*;

    #[test]
    fn build_failure_reason_is_one_bounded_line() {
        let error = EngineError::ImageBuildExitNonzero {
            tag: "t".into(),
            exit_code: 2,
            output: "step 1\nERROR: no such file\n\n".into(),
        };
        assert_eq!(failure_reason(&error), "exit code 2: ERROR: no such file");
        let long = EngineError::Other(format!("{}\nsecond", "x".repeat(400)));
        let reason = failure_reason(&long);
        assert_eq!(reason.chars().count(), MAX_REASON_CHARS);
        assert!(!reason.contains('\n'));
    }
}

/// A building runtime (Docker-like) for session-image contracts: records
/// builds, gives every build a new image id, installs the request's labels,
/// and can fail or hold a build. No daemon, no host agent.
#[cfg(test)]
pub(crate) mod test_runtime {
    use super::*;
    use crate::data::message::UserMessageSink;
    use crate::data::session::Session;
    use crate::engine::agent_runtime::{
        AgentHandle, AgentInstance, AgentStats, Capabilities, ReadyAgentOptions,
        ResolvedAgentOptions,
    };
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};

    #[derive(Default)]
    pub(crate) struct BuildRuntime {
        pub images: Mutex<HashMap<String, ImageMetadata>>,
        pub builds: Mutex<Vec<String>>,
        pub recipes: Mutex<Vec<String>>,
        /// Tags whose builds exit non-zero.
        pub failing: Mutex<HashSet<String>>,
        next_id: AtomicUsize,
        /// Hold every build until `release` is called.
        pub hold: AtomicBool,
        pub build_entered: tokio::sync::Notify,
        released: (Mutex<bool>, Condvar),
        /// What `<binary> --version` prints inside every image (WI 0157).
        /// `None` keeps the trait default (`Unknown`).
        pub cli_version_line: Mutex<Option<String>>,
        /// `(tag, binary)` of every in-image version probe.
        pub version_probes: Mutex<Vec<(String, String)>>,
    }

    impl BuildRuntime {
        /// Install `tag` as if an earlier awman had built it.
        pub(crate) fn install(&self, tag: &str, label: Option<&str>) -> String {
            let id = self.new_id();
            let labels = label
                .map(|value| [(INPUTS_LABEL.to_string(), value.to_string())].into())
                .unwrap_or_default();
            self.images.lock().unwrap().insert(
                tag.into(),
                ImageMetadata {
                    manifest_digest: None,
                    id: id.clone(),
                    tags: vec![tag.into()],
                    labels,
                    size_bytes: 1,
                },
            );
            id
        }
        pub(crate) fn label(&self, tag: &str) -> Option<String> {
            self.images
                .lock()
                .unwrap()
                .get(tag)
                .and_then(|image| image.labels.get(INPUTS_LABEL).cloned())
        }
        pub(crate) fn id(&self, tag: &str) -> Option<String> {
            self.images
                .lock()
                .unwrap()
                .get(tag)
                .map(|image| image.id.clone())
        }
        pub(crate) fn built(&self) -> Vec<String> {
            self.builds.lock().unwrap().clone()
        }
        pub(crate) fn fail(&self, tag: &str, failing: bool) {
            let mut set = self.failing.lock().unwrap();
            if failing {
                set.insert(tag.into());
            } else {
                set.remove(tag);
            }
        }
        pub(crate) fn release(&self) {
            *self.released.0.lock().unwrap() = true;
            self.released.1.notify_all();
        }
        fn new_id(&self) -> String {
            format!(
                "sha256:{:064x}",
                self.next_id.fetch_add(1, Ordering::SeqCst) + 1
            )
        }
    }

    impl AgentRuntimeEngine for BuildRuntime {
        fn runtime_name(&self) -> &'static str {
            "docker"
        }
        fn display_name(&self) -> &'static str {
            "test builder"
        }
        fn capabilities(&self) -> &Capabilities {
            static CAPS: Capabilities = Capabilities {
                arbitrary_host_mounts: true,
                kit_declarative: false,
                has_image_store: true,
                image_acquisition: ImageAcquisition::Build,
                per_launch_vm_settings: false,
                reports_resource_stats: false,
            };
            &CAPS
        }
        fn is_available(&self) -> bool {
            true
        }
        fn image_exists(&self, tag: &str) -> Result<bool, EngineError> {
            Ok(self.images.lock().unwrap().contains_key(tag))
        }
        fn inspect_image(&self, tag: &str) -> Result<Option<ImageMetadata>, EngineError> {
            Ok(self.images.lock().unwrap().get(tag).cloned())
        }
        fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
            Ok(None)
        }
        fn probe_agent_cli_version(
            &self,
            tag: &str,
            pin: &crate::engine::agent::definition::CliVersionPin,
        ) -> crate::engine::container::agent_version::CliVersionStatus {
            use crate::engine::container::agent_version::{classify_cli_version, CliVersionStatus};
            self.version_probes
                .lock()
                .unwrap()
                .push((tag.into(), pin.binary.clone()));
            match self.cli_version_line.lock().unwrap().as_deref() {
                Some(line) => classify_cli_version(pin, line),
                None => CliVersionStatus::Unknown {
                    reason: "test builder cannot probe images".into(),
                },
            }
        }
        fn build_image(
            &self,
            request: &BuildRequest,
            on_line: &mut dyn FnMut(&str),
        ) -> Result<(), EngineError> {
            self.recipes
                .lock()
                .unwrap()
                .push(std::fs::read_to_string(&request.dockerfile).unwrap());
            self.builds.lock().unwrap().push(request.reference.clone());
            on_line("builder stdout");
            if self.hold.load(Ordering::SeqCst) {
                self.build_entered.notify_one();
                let (lock, wake) = &self.released;
                let (released, _) = wake
                    .wait_timeout_while(
                        lock.lock().unwrap(),
                        std::time::Duration::from_secs(10),
                        |released| !*released,
                    )
                    .unwrap();
                assert!(*released, "test must release a held build");
            }
            if self.failing.lock().unwrap().contains(&request.reference) {
                return Err(EngineError::ImageBuildExitNonzero {
                    tag: request.reference.clone(),
                    exit_code: 1,
                    output: "Step 2/3 : RUN false\nERROR: process \"false\" exited 1".into(),
                });
            }
            let id = self.new_id();
            self.images.lock().unwrap().insert(
                request.reference.clone(),
                ImageMetadata {
                    manifest_digest: None,
                    id,
                    tags: vec![request.reference.clone()],
                    labels: request.labels.clone(),
                    size_bytes: 1,
                },
            );
            Ok(())
        }
        fn ready_agent(
            &self,
            _: &str,
            _: ReadyAgentOptions,
            _: &mut dyn UserMessageSink,
        ) -> Result<(), EngineError> {
            panic!("no agents")
        }
        fn build(&self, _: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
            panic!("no agents")
        }
        fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
            Ok(Vec::new())
        }
        fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
            Ok(Vec::new())
        }
        fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
            Ok(Vec::new())
        }
        fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
            panic!("no agents")
        }
        fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
            panic!("no agents")
        }
        fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
            panic!("no agents")
        }
    }
}
