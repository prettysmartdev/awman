use super::*;

pub(crate) struct BuildPreparation {
    /// The content the import must install, when it is known: the build's
    /// receipt, or the identity the source store reports for the reference.
    pub(crate) expected: Option<BuiltImageIdentity>,
    pub(crate) force_refresh: bool,
    pub(crate) pending_key: Option<PendingBuildKey>,
    // The base and agent references cannot be rebuilt or retagged by
    // another awman process until the resulting archive has been imported
    // and the receipt settled.
    pub(super) _leases: Vec<ImageBuildLease>,
    pub(super) status: StepStatus,
    pub(super) warning: Option<ReadyImageWarning>,
    /// The build succeeded but the import must not run (cancelled, or its
    /// receipt could not be recorded): the import row fails with this.
    pub(super) blocked: Option<String>,
}

impl BuildPreparation {
    /// Nothing to build and nothing to bind: import exactly as with the
    /// builder disabled.
    pub(super) fn import_only(status: StepStatus, warning: Option<ReadyImageWarning>) -> Self {
        Self {
            expected: None,
            force_refresh: false,
            pending_key: None,
            _leases: Vec::new(),
            status,
            warning,
            blocked: None,
        }
    }
}

impl AgentImagePreparer {
    /// Take the cross-process build lock on `reference` in `store`. When
    /// another awman process holds it (e.g. a running `awman ready --build`),
    /// say so before waiting rather than block silently.
    pub(super) async fn build_lock(
        &self,
        store: BuildStoreKey,
        reference: String,
        frontend: &mut (impl crate::data::message::UserMessageSink + ?Sized),
    ) -> Result<ImageBuildLease, String> {
        let locks = self.build_locks.clone();
        let (try_locks, try_store, try_reference) =
            (locks.clone(), store.clone(), reference.clone());
        if let Some(lease) = self
            .blocking(move || try_locks.try_lock(&try_store, &try_reference))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?
        {
            return Ok(lease);
        }
        frontend.write_message(crate::data::message::UserMessage {
            level: crate::data::message::MessageLevel::Info,
            text: format!(
                "Waiting for another awman process to finish with {reference} in {}…",
                store.describe()
            ),
        });
        let cancel = self.cancel.clone();
        self.blocking(move || {
            locks.lock(&store, &reference, || {
                if cancel.is_cancelled() {
                    Err(crate::data::error::DataError::Other(
                        "image build cancelled".into(),
                    ))
                } else {
                    Ok(())
                }
            })
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
    }

    pub(super) async fn load_receipt(
        &self,
        key: &PendingBuildKey,
    ) -> Result<Option<PendingBuiltImage>, String> {
        let store = self.pending_builds.clone();
        let key = key.clone();
        self.blocking(move || store.load(&key))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    async fn save_receipt(&self, receipt: PendingBuiltImage) -> Result<(), String> {
        let store = self.pending_builds.clone();
        let cancel = self.cancel.clone();
        self.blocking(move || {
            store.save(&receipt, || {
                if cancel.is_cancelled() {
                    Err(crate::data::error::DataError::Other(
                        "image build cancelled".into(),
                    ))
                } else {
                    Ok(())
                }
            })
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
    }

    /// Remove the receipt for `key` only if it still describes `installed`
    /// (compare-and-delete). Callers hold the agent's build lease, which
    /// every receipt writer also holds, so no newer receipt can be lost.
    pub(super) async fn settle_receipt(
        &self,
        key: &PendingBuildKey,
        installed: &crate::data::oci_identity::ImageIdentity,
    ) -> Result<(), String> {
        let store = self.pending_builds.clone();
        let key = key.clone();
        let installed = installed.clone();
        self.blocking(move || match store.load(&key)? {
            Some(receipt) if receipt.identity.matches(&installed) => store.remove(&key),
            _ => Ok(()),
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
    }

    /// Drop a receipt whose build the source reference no longer holds.
    async fn discard_receipt(&self, key: &PendingBuildKey) -> Result<(), String> {
        let store = self.pending_builds.clone();
        let key = key.clone();
        self.blocking(move || store.remove(&key))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    pub(super) async fn prepare_local_image(
        &mut self,
        frontend: &mut (impl ImagePreparationSink + ?Sized),
        resolved: &crate::engine::oci::ResolvedImageSource,
        selected: &BuilderSelection,
        request: &AgentImageRequest<'_>,
    ) -> Result<BuildPreparation, String> {
        let tag = request.tag;
        let agent = request.agent;
        let policy = request.policy;
        let git_root_buf = self.context.git_root.clone();
        let git_root = git_root_buf.as_path();
        use crate::data::image_tags::project_image_tag;
        use crate::data::message::{MessageLevel, UserMessage};
        use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
        use crate::engine::oci::SourceImage;
        let target = match selected {
            BuilderSelection::Local(target) => target.clone(),
            BuilderSelection::Incompatible(source_kind) => {
                let attempted =
                    policy.force_build() || !self.runtime.image_exists(tag).unwrap_or(false);
                let warning = (attempted && self.warned_tags.insert(tag.to_string())).then(|| {
                    ReadyImageWarning::IncompatibleBuilder {
                        tag: tag.to_string(),
                        source_kind: *source_kind,
                        reason: "the configured source is not a local writable image store".into(),
                    }
                });
                if let Some(w) = &warning {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: w.to_string(),
                    });
                }
                return Ok(BuildPreparation::import_only(StepStatus::Skipped, warning));
            }
            BuilderSelection::Unconfigured | BuilderSelection::Disabled => {
                return Ok(BuildPreparation::import_only(StepStatus::Skipped, None))
            }
        };
        crate::engine::oci::docker_engine::validate_reference(&resolved.reference)
            .map_err(|e| e.to_string())?;
        let store = resolved
            .store_key
            .clone()
            .expect("local builder has a store");
        let agent_ref = AgentImagePreparer::normalize_build_reference(&resolved.reference);
        let project_tag = project_image_tag(git_root);
        let project_ref = AgentImagePreparer::normalize_build_reference(&project_tag);
        let key = PendingBuildKey {
            store: store.clone(),
            reference: agent_ref.clone(),
            platform: resolved.platform.clone(),
            install_tag: tag.to_string(),
        };
        // Leases first, base before agent in every process, held through
        // the import and the receipt update: no other awman process can
        // rebuild or retag either reference between inspection and import.
        let mut leases = vec![
            self.build_lock(store.clone(), project_ref.clone(), frontend)
                .await?,
        ];
        if agent_ref != project_ref {
            leases.push(
                self.build_lock(store.clone(), agent_ref.clone(), frontend)
                    .await?,
            );
        }
        let pending = self.load_receipt(&key).await?;
        if !policy.force_build() {
            let installed = if self.runtime.image_exists(tag).unwrap_or(false) {
                self.runtime.image_identity(tag).ok().flatten()
            } else {
                None
            };
            let inspected = self
                .inspect_source(
                    target.clone(),
                    resolved.reference.clone(),
                    resolved.platform.clone(),
                )
                .await;
            match inspected {
                // Nothing may need building; keep the installed image rather
                // than fail, unless an earlier build still awaits import.
                Err(e)
                    if pending.is_none()
                        && installed.is_some()
                        && !crate::engine::oci::retry::is_cancelled(&e) =>
                {
                    let note = format!(
                        "{} is unreachable; using the installed image ({e})",
                        target.describe()
                    );
                    let warning = ReadyImageWarning::BuilderUnreachable {
                        tag: tag.to_string(),
                        builder: target.describe(),
                        detail: e.to_string(),
                    };
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: warning.to_string(),
                    });
                    // Keep the store/reference leases through the import, as
                    // every other path does: another awman process must not
                    // rebuild or retag the reference meanwhile.
                    let mut prepared =
                        BuildPreparation::import_only(StepStatus::Warn(note), Some(warning));
                    prepared._leases = leases;
                    return Ok(prepared);
                }
                // The build row is a `StepStatus`, whose failure is display
                // text by design (Layer 0 result data); the typed error's
                // `Display` is that text, so every frontend shows the same.
                Err(e) => return Err(e.to_string()),
                // Importable as the builder-disabled path imports it; the
                // store cannot vouch for one platform manifest.
                Ok(SourceImage::Unbound) => {
                    if pending.is_some() {
                        let warning = ReadyImageWarning::BuiltImageNotExposed {
                            tag: tag.to_string(),
                            reference: resolved.reference.clone(),
                        };
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Warning,
                            text: warning.to_string(),
                        });
                        self.discard_receipt(&key).await?;
                        return Ok(BuildPreparation {
                            expected: None,
                            force_refresh: true,
                            pending_key: Some(key),
                            _leases: leases,
                            status: StepStatus::Skipped,
                            warning: Some(warning),
                            blocked: None,
                        });
                    }
                    let mut prepared = BuildPreparation::import_only(StepStatus::Skipped, None);
                    prepared._leases = leases;
                    return Ok(prepared);
                }
                Ok(SourceImage::Present(existing)) => {
                    let mut warning = None;
                    if let Some(receipt) = &pending {
                        if receipt.identity != existing {
                            let stale = ReadyImageWarning::StaleBuildReceipt {
                                tag: tag.to_string(),
                                reference: resolved.reference.clone(),
                            };
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Warning,
                                text: stale.to_string(),
                            });
                            self.discard_receipt(&key).await?;
                            warning = Some(stale);
                        }
                    }
                    // Already installed and nothing pending: the ordinary
                    // reuse path, without re-exporting the image.
                    let current = pending.is_none()
                        && installed.as_ref().is_some_and(|i| existing.matches(i));
                    return Ok(BuildPreparation {
                        expected: (!current).then_some(existing),
                        force_refresh: !current,
                        pending_key: Some(key),
                        _leases: leases,
                        status: StepStatus::Skipped,
                        warning,
                        blocked: None,
                    });
                }
                Ok(SourceImage::Absent) => {}
            }
        }
        if resolved.reference.contains('@') {
            return Err(format!(
                "{tag}: digest-addressed image references are not writable build outputs"
            ));
        }
        if agent_ref == project_ref {
            return Err(format!(
                "{tag}: agent build output collides with project base image {project_tag}"
            ));
        }
        let base_key = (store.clone(), project_ref.clone());
        if let Some(Err(error)) = self.prepared_bases.get(&base_key) {
            return Err(error.clone());
        }
        if !self.prepared_bases.contains_key(&base_key) {
            let base_exists = if policy.force_build() {
                false
            } else {
                !matches!(
                    self.inspect_source(
                        target.clone(),
                        project_tag.clone(),
                        resolved.platform.clone()
                    )
                    .await
                    .map_err(|e| e.to_string())?,
                    SourceImage::Absent
                )
            };
            if !base_exists {
                let dockerfile = self.context.project_dockerfile.clone();
                let step: SetupStep = ReadyStep::BuildBaseImageIn {
                    store: store.describe(),
                    tag: project_tag.clone(),
                }
                .into();
                frontend.report_step(&step, StepStatus::Running);
                let request = BuildRequest {
                    target: target.clone(),
                    reference: project_tag.clone(),
                    dockerfile,
                    context: git_root.to_path_buf(),
                    platform: resolved.platform.clone(),
                    build_args: Vec::new(),
                    no_cache: policy.no_cache(),
                };
                let result = self.build_source(request, frontend).await;
                let result = match result {
                    Ok(receipt) => match self
                        .inspect_source(
                            target.clone(),
                            project_tag.clone(),
                            resolved.platform.clone(),
                        )
                        .await
                    {
                        Ok(SourceImage::Present(current)) if current == receipt => Ok(receipt),
                        Ok(_) => Err(EngineError::ImageBuilder(
                            crate::engine::error::BuilderFailure::BaseImageChanged {
                                tag: project_tag.clone(),
                            },
                        )),
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                };
                let status = match &result {
                    Ok(_) => StepStatus::Done,
                    Err(e) => StepStatus::Failed(e.to_string()),
                };
                frontend.report_step(&step, status.clone());
                self.base_results.push(ReadyBaseImageResult {
                    store: store.describe(),
                    tag: project_tag.clone(),
                    build: status,
                    optional: agent != self.context.default_agent,
                });
                if let Err(e) = result {
                    let message = e.to_string();
                    self.prepared_bases.insert(base_key, Err(message.clone()));
                    return Err(message);
                }
            }
            self.prepared_bases.insert(base_key, Ok(()));
        }
        let dockerfile = RepoDockerfilePaths::new(git_root).agent_dockerfile(agent);
        self.provision_agent_dockerfile(frontend, git_root, agent, &dockerfile)
            .await;
        if !dockerfile.exists() {
            return Err(format!(
                "agent Dockerfile {} is unavailable",
                dockerfile.display()
            ));
        }
        let request = BuildRequest {
            target,
            reference: resolved.reference.clone(),
            dockerfile,
            context: git_root.to_path_buf(),
            platform: resolved.platform.clone(),
            build_args: Vec::new(),
            no_cache: policy.no_cache(),
        };
        let receipt = self
            .build_source(request, frontend)
            .await
            .map_err(|e| e.to_string())?;
        // The build succeeded. Anything that stops the import from here on
        // fails the import, not the build.
        let blocked = self
            .save_receipt(PendingBuiltImage {
                key: key.clone(),
                identity: receipt.clone(),
            })
            .await
            .err()
            .map(|e| format!("the build succeeded, but its receipt was not recorded: {e}"));
        Ok(BuildPreparation {
            expected: Some(receipt),
            force_refresh: true,
            pending_key: Some(key),
            _leases: leases,
            status: StepStatus::Done,
            warning: None,
            blocked,
        })
    }
}
