use super::*;
use crate::data::session::Session;
use crate::engine::image_prep::builder::LocalImageBuilder;
use crate::engine::oci::SourceImage;
use crate::engine::ready::tests as support;
use crate::engine::ready::tests::fakes::*;
use crate::engine::ready::{ReadyEngine, ReadySummary};
use support::{
    archive_sources, built_identity, docker_store_sources, last_status, local_docker_sources,
    script_missing_image_build, write_agent_dockerfiles,
};

mod regression;

/// Keep the original summary assertions, but feed them with direct preparer
/// results. Ready's phase/audit integration is separately tested in ready/tests.
struct PreparationRun {
    preparer: AgentImagePreparer,
    session: Arc<Session>,
    image_policy: BuildPolicy,
    summary: ReadySummary,
}
impl From<ReadyEngine> for PreparationRun {
    fn from(engine: ReadyEngine) -> Self {
        let (session, preparer, image_policy) = engine.into_image_test_parts();
        Self {
            session,
            preparer,
            image_policy,
            summary: ReadySummary::new("builtin"),
        }
    }
}
impl PreparationRun {
    fn summary(&self) -> ReadySummary {
        self.summary.clone()
    }
    fn with_local_builder(mut self, builder: Arc<dyn LocalImageBuilder>) -> Self {
        self.preparer = self.preparer.with_local_builder(builder);
        self
    }
    async fn prepare_agent_image(
        &mut self,
        sink: &mut FakeReadyFrontend,
        root: &std::path::Path,
        agent: &str,
    ) -> Result<(), String> {
        let tag = crate::data::image_tags::agent_image_tag(root, agent);
        let result = self
            .preparer
            .prepare(
                &AgentImageRequest {
                    tag: &tag,
                    agent,
                    policy: self.image_policy,
                },
                sink,
            )
            .await
            .map_err(|e| e.to_string())?;
        self.summary
            .base_image_results
            .extend(self.preparer.take_base_results());
        let error = [&result.build, &result.import]
            .into_iter()
            .find_map(|status| match status {
                StepStatus::Failed(text) => Some(text.clone()),
                _ => None,
            });
        self.summary.image_results.push(result);
        if let Some(error) = error {
            Err(error)
        } else {
            Ok(())
        }
    }
}
async fn step_through_image_phases(run: &mut PreparationRun, sink: &mut FakeReadyFrontend) {
    run.preparer.prevalidate_build_targets(run.image_policy);
    run.summary.base_image = StepStatus::Skipped;
    let root = run.session.git_root().to_path_buf();
    run.summary.agent_image = match run.prepare_agent_image(sink, &root, "claude").await {
        Ok(()) => StepStatus::Done,
        Err(error) => StepStatus::Failed(error),
    };
    if run.image_policy.force_build() {
        for (agent, _) in crate::data::RepoDockerfilePaths::new(&root).discover_agent_dockerfiles()
        {
            if agent != "claude" {
                let _ = run.prepare_agent_image(sink, &root, &agent).await;
            }
        }
    }
}
fn import_engine(
    runtime: Arc<FakeImportRuntime>,
    build: bool,
    sources: ImageSources,
) -> (PreparationRun, FakeReadyFrontend, tempfile::TempDir) {
    let (engine, sink, tmp) = support::import_engine(runtime, build, sources);
    (engine.into(), sink, tmp)
}
fn builder_engine(
    runtime: Arc<FakeImportRuntime>,
    build: bool,
    no_cache: bool,
    sources: ImageSources,
    builder: Arc<FakeLocalBuilder>,
) -> (
    PreparationRun,
    FakeReadyFrontend,
    tempfile::TempDir,
    ImageBuildLockStore,
    PendingBuiltImageStore,
) {
    let (engine, sink, tmp, locks, receipts) =
        support::builder_engine(runtime, build, no_cache, sources, builder);
    (engine.into(), sink, tmp, locks, receipts)
}
fn reopen_builder_engine(
    root: &std::path::Path,
    runtime: Arc<FakeImportRuntime>,
    build: bool,
    sources: ImageSources,
    builder: Arc<FakeLocalBuilder>,
    locks: ImageBuildLockStore,
    receipts: PendingBuiltImageStore,
) -> (PreparationRun, FakeReadyFrontend) {
    let (engine, sink) =
        support::reopen_builder_engine(root, runtime, build, sources, builder, locks, receipts);
    (engine.into(), sink)
}

mod policies;
