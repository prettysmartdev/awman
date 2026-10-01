use serde::{Deserialize, Serialize};

use crate::data::ready_image::{ReadyBaseImageResult, ReadyImageResult, ReadyImageWarning};
use crate::data::step_status::StepStatus;

/// Non-secret host credential health shown by `awman ready`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCredentialHealth {
    pub agent: String,
    pub refreshable: bool,
    pub expires_in_secs: Option<i64>,
    pub expired: bool,
    pub read_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadySummary {
    pub runtime_name: String,
    pub dockerfile: StepStatus,
    pub base_image: StepStatus,
    pub agent_image: StepStatus,
    pub local_agent: StepStatus,
    pub audit: StepStatus,
    pub image_rebuild: StepStatus,
    pub aspec_folder: StepStatus,
    pub work_items_config: StepStatus,
    #[serde(default)]
    pub agent_credentials: Vec<AgentCredentialHealth>,
    pub non_default_agent_images: Vec<(String, StepStatus)>,
    /// Where the agent image came from, for runtimes that import images
    /// instead of building them (builtin). `None` on build runtimes, which
    /// show no row for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_source: Option<StepStatus>,
    /// Per-image build and import results, for runtimes that import images.
    /// Empty on build runtimes and in summaries written before they existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_results: Vec<ReadyImageResult>,
    /// Project base images built into source stores, one per store.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base_image_results: Vec<ReadyBaseImageResult>,
    /// How `ready --build` prepares agent images here; selects the hint's
    /// wording.
    #[serde(default)]
    pub image_action: ReadyImageAction,
}

impl ReadySummary {
    pub fn new(runtime_name: impl Into<String>) -> Self {
        Self {
            runtime_name: runtime_name.into(),
            dockerfile: StepStatus::Pending,
            base_image: StepStatus::Pending,
            agent_image: StepStatus::Pending,
            local_agent: StepStatus::Pending,
            audit: StepStatus::Pending,
            image_rebuild: StepStatus::Pending,
            aspec_folder: StepStatus::Pending,
            work_items_config: StepStatus::Pending,
            agent_credentials: Vec::new(),
            non_default_agent_images: Vec::new(),
            image_source: None,
            image_results: Vec::new(),
            base_image_results: Vec::new(),
            image_action: ReadyImageAction::Build,
        }
    }

    /// The summary's rows, in display order, as `(label, status)`.
    ///
    /// One list for every frontend. Before WI 0114 F-23 the CLI, the TUI and
    /// `remote session start` each assembled their own: the CLI omitted the
    /// aspec and work-items rows entirely, the TUI called them "aspec folder"
    /// and "Config", and the remote renderer called them "aspec/" and "Work
    /// items config" and added an "Image rebuild" row the other two did not
    /// have. Three tables for one summary is the mode drift the architecture
    /// exists to prevent.
    ///
    /// The credential rows are classified here too. Turning an
    /// [`AgentCredentialHealth`] into a status and a label is a reading of
    /// the data, not a rendering choice, and both frontends had a
    /// byte-identical copy of it.
    pub fn rows(&self) -> Vec<(String, StepStatus)> {
        let mut rows: Vec<(String, StepStatus)> = vec![
            ("Dockerfile".to_string(), self.dockerfile.clone()),
            ("Base image".to_string(), self.base_image.clone()),
            ("Agent image".to_string(), self.agent_image.clone()),
            ("Local agent".to_string(), self.local_agent.clone()),
            ("Audit".to_string(), self.audit.clone()),
            ("Image rebuild".to_string(), self.image_rebuild.clone()),
            ("aspec folder".to_string(), self.aspec_folder.clone()),
            (
                "Work items config".to_string(),
                self.work_items_config.clone(),
            ),
        ];
        if let Some(status) = &self.image_source {
            rows.insert(3, ("Image source".to_string(), status.clone()));
        }
        // The ready engine reports a single consolidated entry — either
        // "Other agents" (all OK) or "Missing images" (warn) — and its label
        // is rendered verbatim.
        for (label, status) in &self.non_default_agent_images {
            rows.push((label.clone(), status.clone()));
        }
        for base in &self.base_image_results {
            rows.push((format!("Base image in {}", base.store), base.build.clone()));
        }
        for image in &self.image_results {
            rows.push((format!("Build image: {}", image.agent), image.build.clone()));
            rows.push((
                format!("Import image: {}", image.agent),
                image.import.clone(),
            ));
        }
        for health in &self.agent_credentials {
            rows.push(health.row());
        }
        rows
    }

    /// Every warning raised while preparing images, in result order.
    pub fn warnings(&self) -> impl Iterator<Item = &ReadyImageWarning> {
        self.image_results.iter().flat_map(|r| r.warnings.iter())
    }

    /// Whether a required step failed: a top-level step `awman ready` owns,
    /// a non-default agent image, a base-image build, or either stage of any
    /// per-image result. Warnings and the audit are not failures. This is
    /// the broad reading the legacy `ready --json` `ready` field reports;
    /// the exit status and outcome line use [`Self::is_ready`].
    pub fn has_failures(&self) -> bool {
        let failed = |s: &StepStatus| matches!(s, StepStatus::Failed(_));
        [&self.dockerfile, &self.local_agent, &self.image_rebuild]
            .into_iter()
            .any(failed)
            || self.non_default_agent_images.iter().any(|(_, s)| failed(s))
            || self.has_image_failures()
    }

    /// Whether a required image stage failed: the base or default agent
    /// image, the image source, a base-image build in a source store, or
    /// either stage of a per-image result. Host-side checks (local agent,
    /// Dockerfile prompt) and optional non-default images are not image
    /// stages and keep their pre-existing, non-fatal reading.
    fn has_image_failures(&self) -> bool {
        let failed = |s: &StepStatus| matches!(s, StepStatus::Failed(_));
        failed(&self.base_image)
            || failed(&self.agent_image)
            || self.image_source.as_ref().is_some_and(failed)
            || self.base_image_results.iter().any(|b| failed(&b.build))
            || self
                .image_results
                .iter()
                .filter(|r| !r.optional)
                .any(ReadyImageResult::has_failure)
    }

    /// Whether `awman ready` prepared every required image: no image stage
    /// failed and no per-image or base-image stage was left pending or
    /// running. Drives the outcome line, the process exit status and remote
    /// session setup.
    pub fn is_ready(&self) -> bool {
        !self.has_image_failures()
            && self.base_image.is_terminal()
            && self.agent_image.is_terminal()
            && self
                .base_image_results
                .iter()
                .all(|b| b.build.is_terminal())
            && !self
                .image_results
                .iter()
                .filter(|r| !r.optional)
                .any(ReadyImageResult::is_unfinished)
    }

    /// Why [`Self::is_ready`] is false: the first failed or unfinished
    /// required image stage, as `"<row label>: <message>"`. `None` when
    /// ready.
    pub fn not_ready_reason(&self) -> Option<String> {
        if self.is_ready() {
            return None;
        }
        let mut stages: Vec<(String, &StepStatus)> = vec![
            ("Base image".into(), &self.base_image),
            ("Agent image".into(), &self.agent_image),
        ];
        if let Some(status) = &self.image_source {
            stages.push(("Image source".into(), status));
        }
        for base in &self.base_image_results {
            stages.push((format!("Base image in {}", base.store), &base.build));
        }
        for image in self.image_results.iter().filter(|r| !r.optional) {
            stages.push((format!("Build image: {}", image.agent), &image.build));
            stages.push((format!("Import image: {}", image.agent), &image.import));
        }
        let failed = stages
            .iter()
            .find(|(_, s)| matches!(s, StepStatus::Failed(_)));
        let unfinished = || stages.iter().find(|(_, s)| !s.is_terminal());
        Some(match failed.or_else(unfinished) {
            Some((label, StepStatus::Failed(message))) => format!("{label}: {message}"),
            Some((label, status)) => format!("{label}: {}", status.label()),
            None => "a required image stage did not complete".to_string(),
        })
    }

    /// The closing line every frontend prints after the summary.
    pub fn outcome_line(&self) -> &'static str {
        if self.is_ready() {
            "awman is ready."
        } else {
            "awman is not ready."
        }
    }

    /// What to do next, if anything: how to recover from a failure, or how
    /// to prepare the non-default agent images that are missing. The verb
    /// follows how this runtime acquires images ([`ReadyImageAction`]).
    pub fn hint(&self) -> Option<String> {
        if !self.is_ready() {
            return Some(
                "complete the required image steps, then run \"awman ready\" again.".to_string(),
            );
        }
        let missing = self
            .non_default_agent_images
            .iter()
            .any(|(_, s)| matches!(s, StepStatus::Warn(_)));
        missing.then(|| {
            format!(
                "run \"ready --build\" to {} all available agent images.",
                self.image_action.verb()
            )
        })
    }
}

/// How `ready --build` prepares agent images on the summary's runtime, set
/// by the ready engine from the runtime's image-acquisition capability and
/// whether the builtin builder is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReadyImageAction {
    /// The runtime builds images itself (Docker, Apple Containers).
    #[default]
    Build,
    /// The runtime imports images from their configured sources.
    Import,
    /// The runtime imports images, building them first in their source
    /// store where the source allows it.
    BuildAndImport,
}

impl ReadyImageAction {
    /// The verb phrase used in the hint.
    pub fn verb(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Import => "import",
            Self::BuildAndImport => "build and import",
        }
    }
}

impl AgentCredentialHealth {
    /// This credential's summary row: `(label, status)`.
    pub fn row(&self) -> (String, StepStatus) {
        let status = if let Some(error) = &self.read_error {
            StepStatus::Warn(format!("credential unreadable: {error}"))
        } else if self.expired {
            StepStatus::Warn("credential expired".to_string())
        } else if self.expires_in_secs.is_some() {
            StepStatus::Done
        } else {
            StepStatus::Warn("credential expiry unknown".to_string())
        };
        let label = match self.expires_in_secs {
            Some(secs) if !self.expired && self.read_error.is_none() => {
                format!("Credential {} ({secs}s remaining)", self.agent)
            }
            _ => format!("Credential {}", self.agent),
        };
        (label, status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfinished_required_images_are_not_ready() {
        let mut summary = ReadySummary::new("docker");
        assert!(!summary.is_ready());
        assert_eq!(
            summary.not_ready_reason().as_deref(),
            Some("Base image: pending")
        );
        summary.base_image = StepStatus::Done;
        assert_eq!(
            summary.not_ready_reason().as_deref(),
            Some("Agent image: pending")
        );
        summary.agent_image = StepStatus::Done;
        assert!(summary.is_ready());
    }

    #[test]
    fn a_failed_local_agent_keeps_completed_images_ready() {
        let mut summary = ReadySummary::new("docker");
        summary.base_image = StepStatus::Done;
        summary.agent_image = StepStatus::Done;
        summary.local_agent = StepStatus::Failed("unavailable".into());
        assert!(summary.is_ready());
        assert!(summary.has_failures());
    }

    #[test]
    fn build_runtimes_show_no_image_source_row() {
        let summary = ReadySummary::new("docker");
        assert!(summary
            .rows()
            .iter()
            .all(|(label, _)| label != "Image source"));
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("image_source"), "{json}");
    }

    #[test]
    fn image_source_row_follows_the_agent_image_row() {
        let mut summary = ReadySummary::new("builtin");
        summary.image_source = Some(StepStatus::Done);
        let labels: Vec<String> = summary.rows().into_iter().map(|(l, _)| l).collect();
        let agent = labels.iter().position(|l| l == "Agent image").unwrap();
        assert_eq!(labels[agent + 1], "Image source");
    }
}
