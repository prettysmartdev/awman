//! Hash snapshots and persisted attempt-specific change records.
use super::ContextFileMeta;
use crate::data::{
    error::DataError,
    fs::{api_paths::ApiPaths, private_files},
};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextSnapshot {
    pub files: BTreeMap<String, ContextFileMeta>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextChange {
    pub path: String,
    pub change: ChangeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}
impl ContextSnapshot {
    pub fn from_files(files: Vec<ContextFileMeta>) -> Self {
        Self {
            files: files.into_iter().map(|f| (f.path.clone(), f)).collect(),
        }
    }
    pub fn diff(&self, end: &ContextSnapshot) -> Vec<ContextChange> {
        self.files
            .keys()
            .chain(end.files.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|path| {
                let before = self.files.get(path);
                let after = end.files.get(path);
                let change = match (before, after) {
                    (None, Some(_)) => ChangeKind::Added,
                    (Some(_), None) => ChangeKind::Deleted,
                    (Some(a), Some(b)) if a.sha256 != b.sha256 => ChangeKind::Modified,
                    _ => return None,
                };
                Some(ContextChange {
                    path: path.clone(),
                    change,
                    size_bytes: after.map(|f| f.size_bytes),
                    sha256: after.map(|f| f.sha256.clone()),
                })
            })
            .collect()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CommandContextRecord {
    Start {
        attempt: u32,
        snapshot: ContextSnapshot,
    },
    Final {
        attempt: u32,
        changes: Vec<ContextChange>,
    },
    Unavailable {
        attempt: u32,
        reason: String,
    },
}
impl CommandContextRecord {
    pub fn write_start(
        paths: &ApiPaths,
        session_id: &str,
        command_id: &str,
        record: &Self,
    ) -> Result<(), DataError> {
        Self::write_fenced(paths, session_id, command_id, record, false)
    }
    pub fn write_final(
        paths: &ApiPaths,
        session_id: &str,
        command_id: &str,
        record: &Self,
    ) -> Result<(), DataError> {
        Self::write_fenced(paths, session_id, command_id, record, true)
    }
    fn attempt(&self) -> u32 {
        match self {
            Self::Start { attempt, .. }
            | Self::Final { attempt, .. }
            | Self::Unavailable { attempt, .. } => *attempt,
        }
    }
    fn write_fenced(
        paths: &ApiPaths,
        session: &str,
        command: &str,
        record: &Self,
        final_record: bool,
    ) -> Result<(), DataError> {
        paths.prepare_command_dir(session, command)?;
        let lock = paths
            .command_context_start_path(session, command)
            .with_extension("lock");
        let _guard = private_files::lock_private_exclusive(&lock, || Ok(()))?;
        for previous in [
            Self::read_start(paths, session, command)?,
            Self::read_final(paths, session, command)?,
        ]
        .into_iter()
        .flatten()
        {
            if previous.attempt() > record.attempt() {
                return Ok(());
            }
        }
        let path = if final_record {
            paths.command_context_changes_path(session, command)
        } else {
            paths.command_context_start_path(session, command)
        };
        private_files::write_private_json_atomic(&path, record, || Ok(()))
    }
    pub fn read_start(
        paths: &ApiPaths,
        session_id: &str,
        command_id: &str,
    ) -> Result<Option<Self>, DataError> {
        private_files::read_private_json(&paths.command_context_start_path(session_id, command_id))
    }
    pub fn read_final(
        paths: &ApiPaths,
        session_id: &str,
        command_id: &str,
    ) -> Result<Option<Self>, DataError> {
        private_files::read_private_json(
            &paths.command_context_changes_path(session_id, command_id),
        )
    }
}
