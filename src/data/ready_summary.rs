use serde::{Deserialize, Serialize};

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
        // The ready engine reports a single consolidated entry — either
        // "Other agents" (all OK) or "Missing images" (warn) — and its label
        // is rendered verbatim.
        for (label, status) in &self.non_default_agent_images {
            rows.push((label.clone(), status.clone()));
        }
        for health in &self.agent_credentials {
            rows.push(health.row());
        }
        rows
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
