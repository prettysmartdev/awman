//! `InitSummary` — final report from an `InitEngine` run.

use serde::{Deserialize, Serialize};

use crate::data::init_files::InitFile;
use crate::data::step_status::StepStatus;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitSummary {
    pub config: StepStatus,
    pub aspec_folder: StepStatus,
    pub dockerfile: StepStatus,
    pub audit: StepStatus,
    pub image_build: StepStatus,
    pub agent_image_build: StepStatus,
    /// Result of rebuilding images after the audit phase modifies Dockerfile.dev.
    pub image_rebuild: StepStatus,
    pub work_items_setup: StepStatus,
    /// Every project file init created, updated, kept or (for the project
    /// Dockerfile) skipped, relative to the git root, in the order init
    /// reached them. `dockerfile` above is the step status; the
    /// `ProjectDockerfile` entry here says what happened to the file.
    #[serde(default)]
    pub files: Vec<InitFile>,
}

impl InitSummary {
    /// Record what happened to a file; a later report for the same path
    /// replaces an earlier one.
    pub fn record_file(&mut self, file: InitFile) {
        match self.files.iter_mut().find(|f| f.path == file.path) {
            Some(existing) => *existing = file,
            None => self.files.push(file),
        }
    }

    /// The project Dockerfile's entry, if init reached that step.
    pub fn project_dockerfile(&self) -> Option<&InitFile> {
        self.files
            .iter()
            .find(|f| f.kind == crate::data::init_files::InitFileKind::ProjectDockerfile)
    }

    /// The files init wrote (created or updated) — what a client commits.
    pub fn written_files(&self) -> impl Iterator<Item = &InitFile> {
        self.files.iter().filter(|f| f.status.is_written())
    }
}

impl Default for InitSummary {
    fn default() -> Self {
        Self {
            config: StepStatus::Pending,
            aspec_folder: StepStatus::Pending,
            dockerfile: StepStatus::Pending,
            audit: StepStatus::Pending,
            image_build: StepStatus::Pending,
            agent_image_build: StepStatus::Pending,
            image_rebuild: StepStatus::Pending,
            work_items_setup: StepStatus::Pending,
            files: Vec::new(),
        }
    }
}
