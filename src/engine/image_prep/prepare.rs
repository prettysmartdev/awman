use super::*;
use crate::engine::oci::ImageBuildGuidance;
impl AgentImagePreparer {
    /// Prepare one image. Build/import failures are separate result stages;
    /// inability to use the runtime and check-only refusals return `Err`.
    /// Configured sources always pass through provenance-checked acquisition,
    /// even when this tag is already installed.
    pub async fn prepare(
        &mut self,
        request: &AgentImageRequest<'_>,
        frontend: &mut (impl ImagePreparationSink + ?Sized),
    ) -> Result<ReadyImageResult, EngineError> {
        use crate::data::message::{MessageLevel, UserMessage};
        let tag = request.tag.to_string();
        let agent = request.agent;
        let policy = request.policy;
        let git_root_buf = self.context.git_root.clone();
        let git_root = git_root_buf.as_path();
        let hint = self.context.hint(agent);
        self.prevalidate_build_targets(policy);
        let plan = self.plan(&tag, agent);
        let mut image_result = ReadyImageResult::new(agent.to_string(), tag.clone());
        image_result.optional = agent != self.context.default_agent;
        image_result.build = StepStatus::Skipped;
        if policy == BuildPolicy::CheckOnly {
            // Disabled-builder presence-only behavior is retained from WI0125.
            // Enabled builders must not run an older installation after a
            // failed import or a source change. Hold the source/reference
            // lease until the runtime verifies its installed provenance.
            if !matches!(plan.selection, BuilderSelection::Disabled)
                && self.runtime.image_exists(&tag)?
            {
                if plan.error.is_some() || plan.pinned_default.is_some() {
                    return Err(EngineError::BuiltinImageNeedsPreparation { image: tag });
                }
                if let Some(source) = &plan.source {
                    let pending = self.pending_import(source, &tag, frontend).await?;
                    let check = ImageImportRequest {
                        tag: tag.clone(),
                        source: source.source.clone(),
                        platform: source.platform.clone(),
                        refresh: false,
                        expected_built_identity: pending.expected.clone(),
                        immutable_export_selector: None,
                        registries: self.image_sources.registries.clone(),
                    };
                    if !self.runtime.image_matches_source(&check)? {
                        return Err(EngineError::BuiltinImageNeedsPreparation { image: tag });
                    }
                }
            }
            if self.runtime.image_exists(&tag)? {
                image_result.import = StepStatus::Done;
                return Ok(image_result);
            }
            return Err(if plan.source.is_some() || plan.error.is_some() {
                EngineError::BuiltinImageNotImported { image: tag.clone() }
            } else {
                EngineError::ImageSourceUnconfigured {
                    guidance: Box::new(ImageBuildGuidance::new(
                        &tag,
                        hint.clone(),
                        plan.selection.clone(),
                    )),
                }
            });
        }
        // Import mode keeps the runtime's concrete reason (e.g. /dev/kvm
        // permission, test isolation) instead of a bare "not available".
        if let Err(error) = self.runtime.availability() {
            return Err(EngineError::Other(format!(
                "{} is not available: {error}",
                self.runtime.display_name()
            )));
        }

        // A fixed default source cannot represent another agent. Decide
        // before opening a progress row, just as ready's old skip path did.
        if let Some(reason) = plan.pinned_default {
            if policy.allows_build() && matches!(plan.selection, BuilderSelection::Local(_)) {
                image_result.build = StepStatus::Failed(reason);
                image_result.import = StepStatus::Skipped;
            } else {
                image_result.import = StepStatus::Warn(reason);
            }
            return Ok(image_result);
        }
        // WI 0125 reports endpoint/configuration failures on the build row
        // only when this caller can build. They never provision a Dockerfile.
        if let Some(error) = &plan.error {
            if let Some((kind, reference)) = &plan.source_description {
                image_result.source_kind = Some(*kind);
                image_result.reference = Some(reference.clone());
            }
            if policy.allows_build() && !matches!(plan.selection, BuilderSelection::Disabled) {
                image_result.build = StepStatus::Failed(error.clone());
                image_result.import = StepStatus::Skipped;
            } else {
                image_result.import = StepStatus::Failed(error.clone());
            }
            return Ok(image_result);
        }
        let resolved = match (&plan.source, &plan.error) {
            (Some(source), _) => Ok(source.clone()),
            (_, Some(error)) => Err(EngineError::Other(error.clone())),
            _ => Err(EngineError::ImageSourceUnconfigured {
                guidance: Box::new(ImageBuildGuidance::new(
                    &tag,
                    hint.clone(),
                    plan.selection.clone(),
                )),
            }),
        };
        let refresh = policy.refresh_import();
        // Progress rows reported Running here are resolved to Done/Failed
        // once the import finishes, so no row is left spinning.
        let mut pending: Vec<SetupStep> = Vec::new();
        if !refresh && self.runtime.image_exists(&tag).unwrap_or(false) {
            let identity = self.runtime.image_identity(&tag).ok().flatten();
            // Nothing configured to compare against: keep the image.
            let current = matches!(resolved, Err(EngineError::ImageSourceUnconfigured { .. }));
            if current {
                if let Some(identity) = identity {
                    frontend.report_step(
                        &SetupStep::output(format!(
                            "{tag}: cached {} ({}, from {})",
                            identity.manifest_digest, identity.platform, identity.source
                        )),
                        StepStatus::Done,
                    );
                }
                image_result.import = StepStatus::Done;
                return Ok(image_result);
            }
            if let (Ok(source), Some(id)) = (&resolved, &identity) {
                let reference = &source.reference;
                let step = SetupStep::output(format!(
                    "{tag}: cached from {} ({}); the configured source is {} ({reference}), \
                     importing again",
                    id.source,
                    id.reference,
                    source.source.kind()
                ));
                frontend.report_step(&step, StepStatus::Running);
                pending.push(step);
            }
        }

        let frozen = match resolved {
            Ok(resolved) => resolved,
            Err(e) => {
                // The error carries the external build instruction; make
                // sure the Dockerfile it names exists.
                if policy.allows_build() {
                    self.provision_agent_dockerfile(
                        frontend,
                        git_root,
                        agent,
                        &hint.agent_dockerfile,
                    )
                    .await;
                }
                let text = e.to_string();
                if policy.allows_build() {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: text.clone(),
                    });
                }
                image_result.build = StepStatus::Skipped;
                image_result.import = StepStatus::Failed(text.clone());

                return Ok(image_result);
            }
        };

        let source = frozen.source.clone();
        let reference = frozen.reference.clone();
        let platform = frozen.platform.clone();
        image_result.source_kind = Some(source.kind());
        image_result.reference = Some(reference.clone());
        // Every import request gets a result, with the build stage Skipped
        // when the builder is off, so build and import always show apart.
        image_result.build = StepStatus::Skipped;
        let preparation =
            if policy.allows_build() && !matches!(plan.selection, BuilderSelection::Disabled) {
                if let Some(error) = self.prevalidation_failures.get(&tag).cloned() {
                    image_result.build = StepStatus::Failed(error.clone());
                    image_result.import = StepStatus::Skipped;

                    for step in &pending {
                        frontend.report_step(step, StepStatus::Failed(error.clone()));
                    }
                    return Ok(image_result);
                }
                match self
                    .prepare_local_image(frontend, &frozen, &plan.selection, request)
                    .await
                {
                    Ok(prepared) => {
                        image_result.build = prepared.status.clone();
                        if let Some(warning) = &prepared.warning {
                            image_result.warnings.push(warning.clone());
                        }
                        Some(prepared)
                    }
                    Err(error) => {
                        image_result.build = StepStatus::Failed(error.clone());
                        image_result.import = StepStatus::Skipped;

                        for step in &pending {
                            frontend.report_step(step, StepStatus::Failed(error.clone()));
                        }
                        return Ok(image_result);
                    }
                }
            } else if matches!(plan.selection, BuilderSelection::Disabled) {
                // WI 0125 disabled mode does not take build leases or consult
                // receipts. Import the configured source with the caller's
                // refresh flags, even if an earlier build left a receipt.
                None
            } else {
                match self.pending_import(&frozen, &tag, frontend).await {
                    Ok(prepared) => Some(prepared),
                    Err(error) => {
                        let text = error.to_string();
                        image_result.import = StepStatus::Failed(text.clone());
                        for step in &pending {
                            frontend.report_step(step, StepStatus::Failed(text.clone()));
                        }
                        return Ok(image_result);
                    }
                }
            };
        if let Some(blocked) = preparation.as_ref().and_then(|p| p.blocked.clone()) {
            image_result.import = StepStatus::Failed(blocked.clone());

            for step in &pending {
                frontend.report_step(step, StepStatus::Failed(blocked.clone()));
            }
            return Ok(image_result);
        }
        let built_here = preparation
            .as_ref()
            .is_some_and(|p| p.status == StepStatus::Done);
        let local_builder = preparation
            .as_ref()
            .is_some_and(|p| p.pending_key.is_some());

        // Preflight and squad prepare every step agent on every run; an image
        // already installed is verified against its source (normally a cache
        // hit), which is not worth a line each time.
        if policy == BuildPolicy::ImportOnly && !self.runtime.image_exists(&tag).unwrap_or(false) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!(
                    "Importing image for agent '{agent}' ({tag}) from {} ({reference})…",
                    source.kind()
                ),
            });
        }
        let importing = SetupStep::output(format!(
            "Importing {tag} from {} ({reference})",
            source.kind()
        ));
        frontend.report_step(&importing, StepStatus::Running);
        pending.push(importing);
        let docker_source = matches!(
            source,
            crate::data::config::image_source::ImageSourceSpec::DockerStore { .. }
        );
        let expected = preparation.as_ref().and_then(|p| p.expected.clone());
        let request = ImageImportRequest {
            tag: tag.clone(),
            source,
            platform,
            refresh: refresh || preparation.as_ref().is_some_and(|p| p.force_refresh),
            expected_built_identity: expected.clone(),
            // Export the exact image the receipt names, by its immutable
            // Engine ID, whichever digest kind that store uses for IDs.
            immutable_export_selector: expected.as_ref().filter(|_| docker_source).and_then(|id| {
                id.config_digest()
                    .or(id.manifest_digest())
                    .map(ToString::to_string)
            }),
            // Pull with the session registry settings the source was
            // resolved with.
            registries: self.image_sources.registries.clone(),
        };
        let imported_result = self.import_source(request, frontend).await;
        match imported_result {
            Ok(imported) => {
                image_result.import = StepStatus::Done;

                // The image is installed; a receipt that cannot be cleared
                // only means a later run re-imports the same content.
                if let Some(key) = preparation.as_ref().and_then(|p| p.pending_key.as_ref()) {
                    if let Err(e) = self.settle_receipt(key, &imported.identity).await {
                        warn(
                            frontend,
                            &mut image_result,
                            ReadyImageWarning::ReceiptNotCleared {
                                tag: tag.clone(),
                                detail: e,
                            },
                        );
                    }
                }
                for step in &pending {
                    frontend.report_step(step, StepStatus::Done);
                }
                frontend.report_step(
                    &SetupStep::output(format!(
                        "{tag}: imported {} ({}, from {})",
                        imported.identity.manifest_digest,
                        imported.identity.platform,
                        imported.identity.source
                    )),
                    StepStatus::Done,
                );
                Ok(image_result)
            }
            Err(e) => {
                let text = e.to_string();
                image_result.import = StepStatus::Failed(text.clone());

                for step in &pending {
                    frontend.report_step(step, StepStatus::Failed(text.clone()));
                }
                if crate::engine::oci::retry::is_cancelled(&e) {
                    return Ok(image_result);
                }
                if built_here {
                    // The new image exists in its source store; only the
                    // import failed, and its receipt lets a later run retry it.
                    warn(
                        frontend,
                        &mut image_result,
                        ReadyImageWarning::BuildOkImportFailed { tag: tag.clone() },
                    );
                } else if local_builder && policy.allows_build() {
                    warn(
                        frontend,
                        &mut image_result,
                        ReadyImageWarning::RebuildInSourceStore { tag: tag.clone() },
                    );
                } else if policy.allows_build()
                    && matches!(
                        e,
                        EngineError::SourceImageNotFound { .. } | EngineError::Container(_)
                    )
                {
                    // A missing source image is the common case; say how to
                    // produce it rather than only that it is missing.
                    if policy.allows_build() {
                        self.provision_agent_dockerfile(
                            frontend,
                            git_root,
                            agent,
                            &hint.agent_dockerfile,
                        )
                        .await;
                    }
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: ImageBuildGuidance::new(&tag, hint.clone(), plan.selection.clone())
                            .to_string(),
                    });
                }
                Ok(image_result)
            }
        }
    }

    /// Import-only never inspects/probes a builder. A pending receipt still
    /// forces a fresh, identity-bound acquisition, under the same build lock.
    async fn pending_import(
        &self,
        source: &ResolvedImageSource,
        tag: &str,
        frontend: &mut (impl ImagePreparationSink + ?Sized),
    ) -> Result<BuildPreparation, EngineError> {
        let Some(store) = &source.store_key else {
            return Ok(BuildPreparation::import_only(StepStatus::Skipped, None));
        };
        let reference = AgentImagePreparer::normalize_build_reference(&source.reference);
        let lease = self
            .build_lock(store.clone(), reference.clone(), frontend)
            .await
            .map_err(EngineError::Other)?;
        let key = PendingBuildKey {
            store: store.clone(),
            reference,
            platform: source.platform.clone(),
            install_tag: tag.to_string(),
        };
        let receipt = self.load_receipt(&key).await.map_err(EngineError::Other)?;
        let mut prepared = BuildPreparation::import_only(StepStatus::Skipped, None);
        prepared._leases.push(lease);
        if let Some(receipt) = receipt {
            prepared.expected = Some(receipt.identity);
            prepared.force_refresh = true;
            prepared.pending_key = Some(key);
        }
        Ok(prepared)
    }
}

/// Report a typed image warning: once as a message, as it happens, and on
/// the image's result, so `ready --json` and the summary carry the same text.
fn warn(
    frontend: &mut (impl ImagePreparationSink + ?Sized),
    result: &mut ReadyImageResult,
    warning: ReadyImageWarning,
) {
    use crate::data::message::{MessageLevel, UserMessage};
    frontend.write_message(UserMessage {
        level: MessageLevel::Warning,
        text: warning.to_string(),
    });
    result.warnings.push(warning);
}
