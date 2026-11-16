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
    pub source: Mutex<HashSet<String>>,
    pub installed: Mutex<HashMap<String, ImageIdentity>>,
    pub built: Mutex<Vec<String>>,
    pub imports: Mutex<Vec<String>>,
    pub fail_build: std::sync::atomic::AtomicBool,
    pub apple_missing: std::sync::atomic::AtomicBool,
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
            SourceImage::Present(built_identity())
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
        Ok(built_identity())
    }
}
impl AgentRuntimeEngine for Store {
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
    fn build_image(&self, _: &BuildRequest, _: &mut dyn FnMut(&str)) -> Result<(), EngineError> {
        panic!("source builder only")
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
                        if enabled && !installed && source.is_some() {
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
