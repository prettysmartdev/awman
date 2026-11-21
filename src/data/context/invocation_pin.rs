//! Persistent command-to-workflow pins fenced by execution attempt.
use crate::data::{
    error::DataError,
    fs::{api_paths::ApiPaths, private_files},
};
use std::collections::BTreeSet;
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InvocationPin {
    pub attempt: u32,
    pub invocation_id: uuid::Uuid,
    #[serde(default)]
    pub previous: BTreeSet<uuid::Uuid>,
    #[serde(with = "crate::data::wire::timestamp")]
    pub pinned_at: chrono::DateTime<chrono::Utc>,
}
pub struct CommandInvocationPins {
    paths: ApiPaths,
}
impl CommandInvocationPins {
    pub fn new(paths: ApiPaths) -> Self {
        Self { paths }
    }
    pub fn pin(
        &self,
        session_id: &str,
        command_id: &str,
        attempt: u32,
        invocation_id: uuid::Uuid,
    ) -> Result<(), DataError> {
        self.paths.prepare_command_dir(session_id, command_id)?;
        let path = self
            .paths
            .command_invocation_pin_path(session_id, command_id);
        // Cross-process serialization makes the attempt comparison survive late
        // writers, including an old daemon shutting down during a restart.
        let _lock = private_files::lock_private_exclusive(&path.with_extension("lock"), || Ok(()))?;
        let previous_pin = private_files::read_private_json::<InvocationPin>(&path)?;
        let mut previous = BTreeSet::new();
        if let Some(mut pin) = previous_pin {
            if pin.attempt > attempt {
                return Ok(());
            }
            if pin.attempt == attempt {
                if pin.invocation_id != invocation_id {
                    pin.previous.insert(invocation_id);
                    private_files::write_private_json_atomic(&path, &pin, || Ok(()))?;
                }
                return Ok(());
            }
            previous = pin.previous;
            previous.insert(pin.invocation_id);
        }
        private_files::write_private_json_atomic(
            &path,
            &InvocationPin {
                attempt,
                invocation_id,
                previous,
                pinned_at: chrono::Utc::now(),
            },
            || Ok(()),
        )
    }
    pub fn current(
        &self,
        session_id: &str,
        command_id: &str,
        current_attempt: u32,
    ) -> Result<Option<InvocationPin>, DataError> {
        Ok(private_files::read_private_json::<InvocationPin>(
            &self
                .paths
                .command_invocation_pin_path(session_id, command_id),
        )?
        .filter(|pin| pin.attempt == current_attempt))
    }
    pub fn all_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, InvocationPin)>, DataError> {
        let mut pins = Vec::new();
        for command in directory_names(&self.paths.session_commands_dir(session_id))? {
            match private_files::read_private_json::<InvocationPin>(
                &self.paths.command_invocation_pin_path(session_id, &command),
            ) {
                Ok(Some(pin)) => pins.push((command, pin)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "ignoring unreadable invocation pin during enumeration")
                }
            }
        }
        Ok(pins)
    }
    pub fn remove(&self, session_id: &str, command_id: &str) -> Result<(), DataError> {
        private_files::remove_private_file(
            &self
                .paths
                .command_invocation_pin_path(session_id, command_id),
        )
    }
    pub fn session_dirs(&self) -> Result<Vec<String>, DataError> {
        directory_names(&self.paths.sessions_dir())
    }
    pub fn pinned_invocations(&self) -> Result<BTreeSet<uuid::Uuid>, DataError> {
        let mut ids = BTreeSet::new();
        for session in self.session_dirs()? {
            for (_, pin) in self.all_for_session(&session)? {
                ids.insert(pin.invocation_id);
                ids.extend(pin.previous);
            }
        }
        Ok(ids)
    }
}
fn directory_names(path: &std::path::Path) -> Result<Vec<String>, DataError> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(DataError::io(path, e)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| DataError::io(path, e))?;
        if entry
            .file_type()
            .map_err(|e| DataError::io(path, e))?
            .is_dir()
        {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_owned());
            }
        }
    }
    names.sort();
    Ok(names)
}
