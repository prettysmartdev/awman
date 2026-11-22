//! Lease image builds and budget policy. Reservations precede store access;
//! eviction takes the same private-state lock, so a starting lease cannot
//! race an ending lease's cleanup.
use std::{path::Path, sync::Arc};

use crate::{
    data::{
        config::env::EnvSnapshot,
        fs::{api_db::SqliteSessionStore, DataPaths},
        image_build_lock::BuildStoreKey,
        image_inputs::{InputsHash, RepoKey},
        message::{UserMessage, UserMessageSink},
        session::Session,
        setup_step::SetupStep,
        step_status::StepStatus,
        worker::{
            lease_image_store::LeaseImageStore,
            lease_images::{select_evictions, ImageBudget, LeaseImageKind, LeaseImageRecord},
            LeaseRecord,
        },
    },
    engine::{
        agent_runtime::AgentRuntimeEngine,
        container::BuildRequest,
        error::EngineError,
        image_prep::{AgentImagePreparer, ImageBuildContext, ImageBuildSpec, ImagePreparationSink},
    },
};

/// Import-only and kit runtimes retain their existing acquisition path.
/// They must not advertise lease_image_reuse until source builds/imports use
/// these same reservations and content tags.
pub(crate) fn supported(runtime: &dyn AgentRuntimeEngine) -> bool {
    runtime.capabilities().image_acquisition
        == crate::engine::agent_runtime::ImageAcquisition::Build
        && runtime.capabilities().has_image_store
        && matches!(runtime.runtime_name(), "docker" | "apple-containers")
}

pub(crate) fn store_key(
    runtime: &dyn AgentRuntimeEngine,
    env: &EnvSnapshot,
) -> Result<BuildStoreKey, EngineError> {
    crate::engine::image_prep::freshness::runtime_store_key(runtime, env)
}

pub(super) struct LeaseImageBuilder {
    runtime: Arc<dyn AgentRuntimeEngine>,
    pub(super) records: LeaseImageStore,
    data_home: std::path::PathBuf,
    key: BuildStoreKey,
    preparer: AgentImagePreparer,
    lease: crate::data::ids::LeaseId,
    server: String,
}

impl LeaseImageBuilder {
    pub(super) fn new(
        runtime: Arc<dyn AgentRuntimeEngine>,
        session: &Session,
        db: Arc<SqliteSessionStore>,
        lease: &LeaseRecord,
    ) -> Result<Self, EngineError> {
        let data_home = DataPaths::from_env(session.env())?.root().to_path_buf();
        let key = store_key(runtime.as_ref(), session.env())?;
        Ok(Self {
            preparer: AgentImagePreparer::for_runtime(
                runtime.clone(),
                data_home.clone(),
                ImageBuildContext::for_session(session, session.configured_agent().as_str()),
            ),
            runtime,
            records: LeaseImageStore::open(db)?,
            data_home,
            key,
            lease: lease.id.clone(),
            server: lease.server.clone(),
        })
    }

    pub(super) async fn prepare(
        &self,
        mut request: BuildRequest,
        inputs: InputsHash,
        repo: RepoKey,
        kind: LeaseImageKind,
    ) -> Result<String, EngineError> {
        let tag = request.reference.clone();
        let owner = owner_label(&self.data_home);
        request
            .labels
            .insert("dev.awman.lease-owner".into(), owner.clone());
        let records = self.records.clone();
        let data_home = self.data_home.clone();
        let key = self.key.describe();
        let lease = self.lease.clone();
        let server = self.server.clone();
        let intent = LeaseImageRecord {
            tag: tag.clone(),
            image_id: String::new(),
            repo_key: repo.clone(),
            inputs_hash: inputs.clone(),
            kind: kind.clone(),
            bytes: 0,
            last_used: chrono::Utc::now(),
        };
        self.preparer
            .blocking(move || -> Result<(), EngineError> {
                let _guard = LeaseImageStore::lock(&data_home)?;
                records.reserve(&key, &intent.tag, &lease, &server)?;
                records.intent(&key, &intent)?;
                Ok(())
            })
            .await??;
        self.preparer
            .build_if_needed(
                ImageBuildSpec {
                    request,
                    store: self.key.clone(),
                    inputs: inputs.clone(),
                    repo: repo.clone(),
                    force: false,
                },
                &mut BuildLog,
            )
            .await?;
        let runtime = self.runtime.clone();
        let records = self.records.clone();
        let data_home = self.data_home.clone();
        let key = self.key.describe();
        self.preparer
            .blocking(move || -> Result<String, EngineError> {
                let image = runtime.inspect_image(&tag)?.ok_or_else(|| {
                    EngineError::Other(format!("lease image {tag} disappeared after preparation"))
                })?;
                let _guard = LeaseImageStore::lock(&data_home)?;
                records.record(
                    &key,
                    &LeaseImageRecord {
                        tag,
                        image_id: image.id.clone(),
                        repo_key: repo,
                        inputs_hash: inputs,
                        kind,
                        bytes: image.size_bytes,
                        last_used: chrono::Utc::now(),
                    },
                    image.labels.get("dev.awman.lease-owner") == Some(&owner),
                )?;
                Ok(image.id)
            })
            .await?
    }
}

struct BuildLog;
impl UserMessageSink for BuildLog {
    fn write_message(&mut self, message: UserMessage) {
        tracing::info!(message = %message.text, "lease image preparation");
    }
    fn replay_queued(&mut self) {}
}
impl ImagePreparationSink for BuildLog {
    fn report_step(&mut self, step: &SetupStep, status: StepStatus) {
        tracing::info!(?step, ?status, "lease image build");
    }
}

fn owner_label(data_home: &Path) -> String {
    crate::data::fs::hash::sha256_hex(data_home.to_string_lossy().as_bytes())
}

pub(super) fn schedule_cleanup(
    runtime: Arc<dyn AgentRuntimeEngine>,
    env: EnvSnapshot,
    db: Arc<SqliteSessionStore>,
    budgets: std::collections::BTreeMap<String, ImageBudget>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || cleanup(runtime.as_ref(), &env, db, &budgets))
}

/// Best effort: neither store corruption nor a runtime removal failure can
/// turn a completed lease into a failure. Warning episodes are kept in the
/// private database and shared by every worker host.
pub(super) fn cleanup(
    runtime: &dyn AgentRuntimeEngine,
    env: &EnvSnapshot,
    db: Arc<SqliteSessionStore>,
    budgets: &std::collections::BTreeMap<String, ImageBudget>,
) {
    if !supported(runtime) {
        return;
    }
    if let Err(error) = cleanup_inner(runtime, env, db, budgets) {
        tracing::warn!(%error, "lease image cleanup failed");
    }
}

fn cleanup_inner(
    runtime: &dyn AgentRuntimeEngine,
    env: &EnvSnapshot,
    db: Arc<SqliteSessionStore>,
    budgets: &std::collections::BTreeMap<String, ImageBudget>,
) -> Result<(), EngineError> {
    let paths = DataPaths::from_env(env)?;
    let images = LeaseImageStore::open(db)?;
    let build_store = store_key(runtime, env)?;
    let key = build_store.describe();
    // Reconcile only persisted intents whose images carry this worker's label.
    // A matching external image is never adopted.
    for mut record in images.intents(&key)? {
        let locks =
            crate::data::image_build_lock::ImageBuildLockStore::under_data_home(paths.root());
        let reference = AgentImagePreparer::normalize_build_reference(&record.tag);
        let Some(_lease) = locks.try_lock(&build_store, &reference)? else {
            continue;
        };
        let image = match runtime.inspect_image(&record.tag) {
            Ok(image) => image,
            Err(error) => {
                tracing::warn!(%error, tag = %record.tag, "could not reconcile lease image intent");
                continue;
            }
        };
        if let Some(image) = image {
            if image.labels.get("dev.awman.lease-owner") == Some(&owner_label(paths.root()))
                && record.inputs_hash.matches_label(
                    image
                        .labels
                        .get(crate::data::image_inputs::INPUTS_LABEL)
                        .map(String::as_str),
                )
            {
                record.image_id = image.id;
                record.bytes = image.size_bytes;
                images.record(&key, &record, true)?;
            }
        }
    }
    images.compact()?;
    for server in images.servers(&key)? {
        let result = (|| -> Result<(), EngineError> {
            let budget = budgets.get(&server).copied().unwrap_or_default();
            let records = images.records(&key, Some(&server))?;
            let in_use = images.in_use(&key)?;
            let plan = select_evictions(&records, &in_use, &budget);
            if plan.budget_below_active {
                if images.budget_warning(&key, &server, true)? {
                    tracing::warn!(%server, "lease image budget is smaller than active leases' images; keeping active images");
                }
            } else if !in_use.is_empty() {
                images.budget_warning(&key, &server, false)?;
            }
            // Recompute after each attempt: a failed removal still consumes the
            // budget, so continue with newer eligible images when necessary.
            let mut candidates: Vec<_> = records.iter().collect();
            candidates.sort_by(|a, b| a.last_used.cmp(&b.last_used).then(a.tag.cmp(&b.tag)));
            for candidate in candidates {
                let tag = &candidate.tag;
                let remaining = images.records(&key, Some(&server))?;
                if budget.admits(remaining.len(), remaining.iter().map(|r| r.bytes).sum()) {
                    break;
                }
                if in_use.contains(tag) {
                    continue;
                }
                let Some(record) = remaining.iter().find(|r| &r.tag == tag) else {
                    continue;
                };
                if let Err(error) =
                    remove_owned(runtime, &images, &build_store, paths.root(), record)
                {
                    tracing::warn!(%error, %tag, "could not remove lease image");
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(%server, %error, "lease image budget cleanup failed for server");
        }
    }
    Ok(())
}

/// Revalidate identity before removal: another tool may have retagged the
/// name since the worker built it. Removal is by tag, never by ID/force.
pub(crate) fn remove_owned(
    runtime: &dyn AgentRuntimeEngine,
    images: &LeaseImageStore,
    store: &BuildStoreKey,
    data_home: &Path,
    record: &LeaseImageRecord,
) -> Result<bool, EngineError> {
    let locks = crate::data::image_build_lock::ImageBuildLockStore::under_data_home(data_home);
    let reference = AgentImagePreparer::normalize_build_reference(&record.tag);
    let Some(_lease) = locks.try_lock(store, &reference)? else {
        return Ok(false);
    };
    let key = &store.describe();
    {
        let _guard = LeaseImageStore::lock(data_home)?;
        if images.in_use(key)?.contains(&record.tag) {
            return Ok(false);
        }
    }
    if let Some(image) = runtime.inspect_image(&record.tag)? {
        if image.id != record.image_id
            || !record.inputs_hash.matches_label(
                image
                    .labels
                    .get(crate::data::image_inputs::INPUTS_LABEL)
                    .map(String::as_str),
            )
        {
            images.forget(key, &record.tag)?;
            return Ok(false);
        }
        runtime.remove_image(&record.tag)?;
    }
    let _guard = LeaseImageStore::lock(data_home)?;
    images.forget(key, &record.tag)?;
    Ok(true)
}

pub(super) fn budgets(
    settings: &crate::data::config::daemon::WorkerSettings,
) -> std::collections::BTreeMap<String, ImageBudget> {
    settings
        .work_servers
        .iter()
        .map(|(name, server)| (name.clone(), server.image_budget))
        .collect()
}

/// Dockerfiles that name the ordinary project tag must use the content-keyed
/// base for this build. Keep the checked-out recipe unchanged; its original
/// bytes (plus the actual base ID) remain the agent's hash inputs.
pub(super) fn agent_recipe(
    bytes: &[u8],
    named_base: &str,
    lease_base: &str,
    path: &Path,
) -> Result<std::path::PathBuf, EngineError> {
    let text = crate::data::image_inputs::bind_project_base(bytes, named_base, lease_base)
        .map_err(EngineError::Config)?;
    crate::data::fs::config_file::write_config_file_within(
        path.parent().expect("private recipe parent"),
        path,
        &text,
    )?;
    Ok(path.into())
}
