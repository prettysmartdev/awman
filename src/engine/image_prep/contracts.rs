//! Build and import contracts without a real runtime or host agent.
use super::builder::SourceImage;
use super::*;
use crate::data::{
    message::{UserMessage, UserMessageSink},
    oci_identity::{Digest, ImageIdentity, OciPlatform},
    session::Session,
};
use crate::engine::{
    agent_runtime::*,
    container::{BuildControl, BuildTarget},
};
use awman_test_support::{TestCase, TestSandbox};
use std::sync::Mutex;

#[derive(Default)]
pub(crate) struct Store {
    pub source_metadata: Mutex<HashMap<String, crate::engine::container::runtime::ImageMetadata>>,
    pub installed_metadata:
        Mutex<HashMap<String, crate::engine::container::runtime::ImageMetadata>>,
    pub source: Mutex<HashSet<String>>,
    pub installed: Mutex<HashMap<String, ImageIdentity>>,
    pub built: Mutex<Vec<String>>,
    pub imports: Mutex<Vec<String>>,
    pub fail_build: std::sync::atomic::AtomicBool,
    pub apple_missing: std::sync::atomic::AtomicBool,
    pub apple_unbound: std::sync::atomic::AtomicBool,
    pub import_entered: tokio::sync::Notify,
    pub release_import: (Mutex<bool>, std::sync::Condvar),
    pub hold_import: std::sync::atomic::AtomicBool,
}
pub(crate) fn identity(reference: &str) -> ImageIdentity {
    ImageIdentity {
        reference: reference.into(),
        manifest_digest: Digest::parse(&format!("sha256:{}", "1".repeat(64))).unwrap(),
        config_digest: Digest::parse(&format!("sha256:{}", "2".repeat(64))).unwrap(),
        platform: OciPlatform::host_linux(),
        source: ImageSourceKind::AppleStore,
    }
}
fn built_identity() -> BuiltImageIdentity {
    let id = identity("unused");
    BuiltImageIdentity::new(
        Some(id.config_digest),
        Some(id.manifest_digest),
        id.platform,
    )
    .unwrap()
}
impl LocalImageBuilder for Store {
    fn inspect_metadata(
        &self,
        _: &BuildTarget,
        reference: &str,
        _: &BuildControl,
    ) -> Result<Option<crate::engine::container::runtime::ImageMetadata>, EngineError> {
        Ok(self.source_metadata.lock().unwrap().get(reference).cloned())
    }
    fn probe(&self, _: &BuildTarget, _: &BuildControl) -> Result<(), EngineError> {
        Ok(())
    }
    fn inspect(
        &self,
        _: &BuildTarget,
        reference: &str,
        _: &OciPlatform,
        _: &BuildControl,
    ) -> Result<SourceImage, EngineError> {
        Ok(if self.source.lock().unwrap().contains(reference) {
            if self.apple_unbound.load(std::sync::atomic::Ordering::SeqCst) {
                SourceImage::Unbound
            } else {
                SourceImage::Present(built_identity())
            }
        } else {
            SourceImage::Absent
        })
    }
    fn build(
        &self,
        request: &BuildRequest,
        _: &BuildControl,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<BuiltImageIdentity, EngineError> {
        on_line("builder stdout");
        self.built.lock().unwrap().push(request.reference.clone());
        if self.fail_build.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(EngineError::ImageBuildExitNonzero {
                tag: request.reference.clone(),
                exit_code: 64,
                output: "builder stderr: invalid Dockerfile".into(),
            });
        }
        self.source
            .lock()
            .unwrap()
            .insert(request.reference.clone());
        self.source_metadata.lock().unwrap().insert(
            request.reference.clone(),
            crate::engine::container::runtime::ImageMetadata {
                id: identity(&request.reference).config_digest.to_string(),
                tags: vec![request.reference.clone()],
                labels: request.labels.clone(),
                size_bytes: 100,
            },
        );
        Ok(built_identity())
    }
}
impl AgentRuntimeEngine for Store {
    fn inspect_image(
        &self,
        tag: &str,
    ) -> Result<Option<crate::engine::container::runtime::ImageMetadata>, EngineError> {
        Ok(self.installed_metadata.lock().unwrap().get(tag).cloned())
    }
    fn list_images_by_repo(
        &self,
        repo: &crate::data::image_inputs::RepoKey,
    ) -> Result<Vec<crate::engine::container::runtime::ImageMetadata>, EngineError> {
        Ok(self
            .installed_metadata
            .lock()
            .unwrap()
            .values()
            .filter(|image| {
                image
                    .labels
                    .get(crate::data::image_inputs::REPO_LABEL)
                    .map(String::as_str)
                    == Some(repo.as_str())
            })
            .cloned()
            .collect())
    }
    fn remove_image(&self, reference: &str) -> Result<(), EngineError> {
        self.installed_metadata
            .lock()
            .unwrap()
            .retain(|tag, image| tag != reference && image.id != reference);
        Ok(())
    }
    fn runtime_name(&self) -> &'static str {
        "builtin"
    }
    fn display_name(&self) -> &'static str {
        "test importer"
    }
    fn capabilities(&self) -> &Capabilities {
        static CAPS: Capabilities = Capabilities {
            arbitrary_host_mounts: true,
            kit_declarative: false,
            has_image_store: true,
            image_acquisition: ImageAcquisition::Import,
            per_launch_vm_settings: true,
            reports_resource_stats: false,
        };
        &CAPS
    }
    fn is_available(&self) -> bool {
        true
    }
    fn image_exists(&self, tag: &str) -> Result<bool, EngineError> {
        Ok(self.installed.lock().unwrap().contains_key(tag))
    }
    fn image_identity(&self, tag: &str) -> Result<Option<ImageIdentity>, EngineError> {
        Ok(self.installed.lock().unwrap().get(tag).cloned())
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        Ok(None)
    }
    fn import_image(
        &self,
        request: &ImageImportRequest,
        _: &mut dyn UserMessageSink,
    ) -> Result<ImportedImage, EngineError> {
        self.imports.lock().unwrap().push(request.tag.clone());
        if self
            .apple_missing
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(EngineError::AppleImageContentMissing {
                reference: request.tag.clone(),
                platform: request.platform.to_string(),
                detail: "lost layer".into(),
            });
        }
        self.import_entered.notify_one();
        if self.hold_import.load(std::sync::atomic::Ordering::SeqCst) {
            let (lock, wake) = &self.release_import;
            let (guard, _) = wake
                .wait_timeout_while(
                    lock.lock().unwrap(),
                    std::time::Duration::from_secs(5),
                    |released| !*released,
                )
                .unwrap();
            assert!(*guard, "test must release held import");
        }
        let identity = identity(&request.tag);
        self.installed
            .lock()
            .unwrap()
            .insert(request.tag.clone(), identity.clone());
        Ok(ImportedImage {
            tag: request.tag.clone(),
            identity,
            home_dir: None,
            user: None,
        })
    }
    fn build(&self, _: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("no agents")
    }
    fn build_image(
        &self,
        request: &BuildRequest,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        LocalImageBuilder::build(
            self,
            request,
            &BuildControl::source_build(crate::engine::oci::CancelToken::new()),
            on_line,
        )?;
        let metadata = self
            .source_metadata
            .lock()
            .unwrap()
            .get(&request.reference)
            .unwrap()
            .clone();
        self.installed_metadata
            .lock()
            .unwrap()
            .insert(request.reference.clone(), metadata);
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
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("no agents")
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("no agents")
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("no agents")
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
pub(crate) fn preparer(
    s: &TestSandbox,
    store: Arc<Store>,
    enabled: bool,
    source: Option<crate::data::config::image_source::ImageSourceSpec>,
) -> AgentImagePreparer {
    let root = s.work();
    std::fs::create_dir_all(root.join(".awman")).unwrap();
    std::fs::write(root.join("Dockerfile.dev"), "FROM scratch\n").unwrap();
    std::fs::write(root.join(".awman/Dockerfile.claude"), "FROM scratch\n").unwrap();
    let env = crate::data::config::env::EnvSnapshot::from_full_env(
        s.env()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect(),
    );
    let mut prep = AgentImagePreparer::with_inputs(
        ImageSources {
            default: source,
            ..Default::default()
        },
        enabled,
        store.clone(),
        s.root().join("data"),
        ImageBuildContext {
            git_root: root.clone(),
            project_dockerfile: root.join("Dockerfile.dev"),
            default_agent: "claude".into(),
        },
    )
    .with_definition_env(env);
    prep.local_builder = Some(store);
    prep
}
#[derive(Default)]
pub(crate) struct Sink(pub Vec<String>);
impl UserMessageSink for Sink {
    fn write_message(&mut self, message: UserMessage) {
        self.0.push(message.text);
    }
    fn replay_queued(&mut self) {}
}
impl ImagePreparationSink for Sink {
    fn report_step(&mut self, _: &SetupStep, _: StepStatus) {}
}

#[test]
fn missing_image_policy_preserves_installed_disabled_and_unconfigured_behavior() {
    use crate::data::config::image_source::ImageSourceSpec;
    let s = TestSandbox::for_test(TestCase::component("engine::image_prep::contracts::missing_image_policy_preserves_installed_disabled_and_unconfigured_behavior"));
    let store = Arc::new(Store::default());
    let tag = crate::data::image_tags::agent_image_tag(&s.work(), "claude");
    for source in [
        Some(ImageSourceSpec::AppleStore { reference: None }),
        Some(ImageSourceSpec::Registry {
            registry: None,
            reference: None,
        }),
        None,
    ] {
        for enabled in [false, true] {
            for installed in [false, true] {
                store.installed.lock().unwrap().clear();
                if installed {
                    store
                        .installed
                        .lock()
                        .unwrap()
                        .insert(tag.clone(), identity(&tag));
                }
                let mut prep = preparer(&s, store.clone(), enabled, source.clone());
                for fallback in [BuildPolicy::CheckOnly, BuildPolicy::ImportOnly] {
                    assert_eq!(
                        prep.missing_image_policy(&tag, "claude", fallback).unwrap(),
                        if enabled && source.is_some() && (!installed || matches!(source, Some(crate::data::config::image_source::ImageSourceSpec::AppleStore { .. }) | Some(crate::data::config::image_source::ImageSourceSpec::DockerStore { .. }))) {
                            BuildPolicy::SessionSetup
                        } else {
                            fallback
                        }
                    );
                }
            }
        }
    }
    assert!(store.built.lock().unwrap().is_empty());
    assert!(store.imports.lock().unwrap().is_empty());
    s.finish();
}

fn build_spec(s: &TestSandbox, tag: &str, inputs: &str) -> ImageBuildSpec {
    ImageBuildSpec {
        request: BuildRequest::runtime(tag, &s.work().join("Dockerfile.dev"), &s.work(), false),
        store: BuildStoreKey::AppleLocal,
        inputs: crate::data::image_inputs::InputsHash::from_label(inputs),
        repo: crate::data::image_inputs::RepoKey::from_label("example.test/team/repo"),
        force: false,
    }
}

#[test]
fn freshness_distinguishes_missing_unlabelled_stale_and_fresh() {
    use crate::data::image_inputs::{InputsHash, INPUTS_LABEL};
    use crate::engine::container::runtime::ImageMetadata;
    let expected = InputsHash::from_label("expected");
    assert_eq!(
        ImageFreshness::from_image(None, &expected),
        ImageFreshness::Missing
    );
    let mut image = ImageMetadata {
        id: "id".into(),
        tags: vec!["tag".into()],
        labels: Default::default(),
        size_bytes: 123,
    };
    assert!(matches!(
        ImageFreshness::from_image(Some(image.clone()), &expected),
        ImageFreshness::Unlabelled(_)
    ));
    image.labels.insert(INPUTS_LABEL.into(), "old".into());
    assert!(matches!(
        ImageFreshness::from_image(Some(image.clone()), &expected),
        ImageFreshness::Stale(_)
    ));
    image.labels.insert(INPUTS_LABEL.into(), "expected".into());
    assert!(matches!(
        ImageFreshness::from_image(Some(image), &expected),
        ImageFreshness::Fresh(_)
    ));
}

struct WaitSink(tokio::sync::mpsc::UnboundedSender<()>);
impl UserMessageSink for WaitSink {
    fn write_message(&mut self, message: UserMessage) {
        if message
            .text
            .starts_with("Waiting for another awman process")
        {
            let _ = self.0.send(());
        }
    }
    fn replay_queued(&mut self) {}
}
impl ImagePreparationSink for WaitSink {
    fn report_step(&mut self, _: &SetupStep, _: StepStatus) {}
}

#[tokio::test]
async fn concurrent_runtime_builds_recheck_inputs_after_the_existing_lease() {
    let s = TestSandbox::for_test(TestCase::component("engine::image_prep::contracts::concurrent_runtime_builds_recheck_inputs_after_the_existing_lease"));
    let store = Arc::new(Store::default());
    let first = preparer(&s, store.clone(), false, None);
    let second = preparer(&s, store.clone(), false, None);
    let spec = build_spec(&s, "shared:latest", "inputs");
    let held = first
        .build_locks
        .try_lock(&spec.store, "shared:latest")
        .unwrap()
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let other_spec = build_spec(&s, "docker.io/library/shared:latest", "inputs");
    // Use one installed tag; aliases already share the same lock key.
    assert_eq!(
        AgentImagePreparer::normalize_build_reference(&other_spec.request.reference),
        "shared:latest"
    );
    let second_spec = build_spec(&s, "shared:latest", "inputs");
    let first_tx = tx.clone();
    let a = tokio::spawn(async move { first.build_if_needed(spec, &mut WaitSink(first_tx)).await });
    let b =
        tokio::spawn(async move { second.build_if_needed(second_spec, &mut WaitSink(tx)).await });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        rx.recv().await.unwrap();
        rx.recv().await.unwrap();
    })
    .await
    .unwrap();
    assert!(store.built.lock().unwrap().is_empty());
    drop(held);
    let (a, b) = tokio::join!(a, b);
    let outcomes = [a.unwrap().unwrap(), b.unwrap().unwrap()];
    assert!(outcomes.contains(&ImageBuildOutcome::Built));
    assert!(outcomes.contains(&ImageBuildOutcome::Reused));
    assert_eq!(*store.built.lock().unwrap(), ["shared:latest"]);
    let metadata = AgentRuntimeEngine::inspect_image(store.as_ref(), "shared:latest")
        .unwrap()
        .unwrap();
    assert_eq!(
        metadata.labels[crate::data::image_inputs::INPUTS_LABEL],
        "inputs"
    );
    assert_eq!(
        metadata.labels[crate::data::image_inputs::REPO_LABEL],
        "example.test/team/repo"
    );
    s.finish();
}

#[tokio::test]
async fn failed_rebuild_preserves_old_image_and_releases_build_lease() {
    let s = TestSandbox::for_test(TestCase::component("engine::image_prep::contracts::failed_rebuild_preserves_old_image_and_releases_build_lease"));
    let store = Arc::new(Store::default());
    let prep = preparer(&s, store.clone(), false, None);
    prep.build_if_needed(
        build_spec(&s, "project:latest", "old"),
        &mut Sink::default(),
    )
    .await
    .unwrap();
    store
        .fail_build
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(prep
        .build_if_needed(
            build_spec(&s, "project:latest", "new"),
            &mut Sink::default()
        )
        .await
        .is_err());
    assert!(matches!(
        prep.freshness(
            "project:latest",
            &crate::data::image_inputs::InputsHash::from_label("old")
        )
        .unwrap(),
        ImageFreshness::Fresh(_)
    ));
    assert!(prep
        .build_locks
        .try_lock(&BuildStoreKey::AppleLocal, "project:latest")
        .unwrap()
        .is_some());
    store
        .fail_build
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        prep.build_if_needed(
            build_spec(&s, "project:latest", "new"),
            &mut Sink::default()
        )
        .await
        .unwrap(),
        ImageBuildOutcome::Built
    );
    s.finish();
}

#[tokio::test]
async fn source_build_labels_and_reuses_matching_inputs_under_lease() {
    let s = TestSandbox::for_test(TestCase::component(
        "engine::image_prep::contracts::source_build_labels_and_reuses_matching_inputs_under_lease",
    ));
    let store = Arc::new(Store::default());
    let mut prep = preparer(&s, store.clone(), false, None);
    let mut spec = build_spec(&s, "source:latest", "source-inputs");
    spec.request.target = BuildTarget::Apple;
    let first = prep
        .build_source_if_needed(&spec, &mut Sink::default())
        .await
        .unwrap();
    assert_eq!(first.outcome, ImageBuildOutcome::Built);
    assert!(prep
        .build_locks
        .try_lock(&spec.store, "source:latest")
        .unwrap()
        .is_none());
    drop(first);
    store
        .apple_unbound
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let second = prep
        .build_source_if_needed(&spec, &mut Sink::default())
        .await
        .unwrap();
    assert_eq!(second.outcome, ImageBuildOutcome::Reused);
    assert_eq!(store.built.lock().unwrap().len(), 1);
    drop(second);
    s.finish();
}

#[tokio::test]
async fn ready_source_builds_label_project_and_agent_with_layer_zero_hashes() {
    use crate::data::image_inputs::{
        agent_inputs_hash, project_inputs, DirBuildContext, INPUTS_LABEL, REPO_LABEL,
    };
    let s = TestSandbox::for_test(TestCase::component("engine::image_prep::contracts::ready_source_builds_label_project_and_agent_with_layer_zero_hashes"));
    let store = Arc::new(Store::default());
    let mut prep = preparer(
        &s,
        store.clone(),
        true,
        Some(crate::data::config::image_source::ImageSourceSpec::AppleStore { reference: None }),
    );
    let tag = crate::data::image_tags::agent_image_tag(&s.work(), "claude");
    let result = prep
        .prepare(
            &AgentImageRequest {
                tag: &tag,
                agent: "claude",
                policy: BuildPolicy::Ready {
                    build: true,
                    no_cache: false,
                    refresh: false,
                },
            },
            &mut Sink::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.build, StepStatus::Done);
    let project_tag = crate::data::image_tags::project_image_tag(&s.work());
    let metadata = store.source_metadata.lock().unwrap();
    assert_eq!(
        metadata[&project_tag].labels[INPUTS_LABEL],
        project_inputs(Some(b"FROM scratch\n"), &DirBuildContext::new(s.work()))
            .hash
            .as_str()
    );
    let base_id = metadata[&project_tag].id.clone();
    assert_eq!(
        metadata[&tag].labels[INPUTS_LABEL],
        agent_inputs_hash(b"FROM scratch\n", &base_id).as_str()
    );
    assert_eq!(
        metadata[&project_tag].labels[REPO_LABEL],
        crate::data::image_inputs::RepoKey::local(&s.work()).as_str()
    );
    drop(metadata);
    s.finish();
}

#[test]
fn build_labels_use_layer_zero_hashes_and_explicit_lease_inputs() {
    use crate::data::image_inputs::*;
    let s = TestSandbox::for_test(TestCase::component("engine::image_prep::contracts::build_labels_use_layer_zero_hashes_and_explicit_lease_inputs"));
    std::fs::write(
        s.work().join("Dockerfile.dev"),
        "FROM scratch\nCOPY tool /tool\n",
    )
    .unwrap();
    std::fs::write(s.work().join("tool"), "one").unwrap();
    let env = crate::data::config::env::EnvSnapshot::from_full_env(Default::default());
    let request = BuildRequest::runtime(
        "project:latest",
        &s.work().join("Dockerfile.dev"),
        &s.work(),
        false,
    );
    let first =
        super::freshness::label_build(&request, &env, |_| panic!("project has no base lookup"))
            .unwrap();
    let expected = project_inputs(
        Some(b"FROM scratch\nCOPY tool /tool\n"),
        &DirBuildContext::new(s.work()),
    )
    .hash;
    assert_eq!(first.labels[INPUTS_LABEL], expected.as_str());
    assert_eq!(first.labels[REPO_LABEL], RepoKey::local(&s.work()).as_str());
    std::fs::write(s.work().join("tool"), "two").unwrap();
    let changed = super::freshness::label_build(&request, &env, |_| Ok(None)).unwrap();
    assert_ne!(first.labels[INPUTS_LABEL], changed.labels[INPUTS_LABEL]);
    std::fs::create_dir_all(s.work().join(".awman")).unwrap();
    let agent_file = s.work().join(".awman/Dockerfile.claude");
    std::fs::write(&agent_file, "FROM project:latest\n").unwrap();
    let request = BuildRequest::runtime("agent:latest", &agent_file, &s.work(), false);
    let labels = super::freshness::label_build(&request, &env, |_| Ok(Some("project-id".into())))
        .unwrap()
        .labels;
    assert_eq!(
        labels[INPUTS_LABEL],
        agent_inputs_hash(b"FROM project:latest\n", "project-id").as_str()
    );
    let request = request.with_inputs(
        &InputsHash::from_label("lease-inputs"),
        &RepoKey::from_label("example.test/repo"),
    );
    let labels = super::freshness::label_build(&request, &env, |_| panic!("explicit lease hash"))
        .unwrap()
        .labels;
    assert_eq!(labels[INPUTS_LABEL], "lease-inputs");
    assert_eq!(labels[REPO_LABEL], "example.test/repo");
    s.finish();
}
