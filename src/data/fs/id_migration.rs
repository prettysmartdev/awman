//! Filesystem half of the WI 0139 persisted-ID migration.

use crate::data::{
    error::DataError,
    fs::api_paths::ApiPaths,
    ids::{CommandId, WorkerSessionId},
};
use std::path::Path;

/// Rename legacy UUID directories to their resource-prefixed names. Re-running
/// after an interrupted filesystem migration is safe: prefixed names are
/// skipped and already-moved session directories are scanned again.
pub fn rename_session_dirs(paths: &ApiPaths) -> Result<(), DataError> {
    let root = paths.sessions_dir();
    if !root.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&root).map_err(|e| DataError::io(&root, e))? {
        let entry = entry.map_err(|e| DataError::io(&root, e))?;
        if !entry
            .file_type()
            .map_err(|e| DataError::io(entry.path(), e))?
            .is_dir()
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let session_id = WorkerSessionId::from_legacy_uuid(&name)
            .map(|id| id.to_string())
            .unwrap_or(name);
        let session_path = if session_id != entry.file_name().to_string_lossy() {
            rename_if_destination_free(&entry.path(), &root.join(&session_id))?;
            root.join(&session_id)
        } else {
            entry.path()
        };
        let commands = session_path.join("commands");
        if !commands.is_dir() {
            continue;
        }
        for command in std::fs::read_dir(&commands).map_err(|e| DataError::io(&commands, e))? {
            let command = command.map_err(|e| DataError::io(&commands, e))?;
            if !command
                .file_type()
                .map_err(|e| DataError::io(command.path(), e))?
                .is_dir()
            {
                continue;
            }
            let name = command.file_name().to_string_lossy().into_owned();
            if let Some(id) = CommandId::from_legacy_uuid(&name) {
                rename_if_destination_free(&command.path(), &commands.join(id.as_str()))?;
            }
        }
    }
    Ok(())
}

fn rename_if_destination_free(from: &Path, to: &Path) -> Result<(), DataError> {
    if to.exists() {
        return Err(DataError::Other(format!(
            "cannot migrate id directory {}: destination {} already exists; inspect both directories and merge or move the legacy UUID directory, then restart awman",
            from.display(),
            to.display()
        )));
    }
    std::fs::rename(from, to).map_err(|e| DataError::io(from, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renames_session_and_nested_command_dirs_idempotently() {
        let t = tempfile::tempdir().unwrap();
        let paths = ApiPaths::from_root(t.path());
        let old = "00000000-0000-0000-0000-000000000001";
        let old_cmd = "00000000-0000-0000-0000-000000000002";
        let cmd_dir = paths
            .sessions_dir()
            .join(old)
            .join("commands")
            .join(old_cmd);
        std::fs::create_dir_all(&cmd_dir).unwrap();
        rename_session_dirs(&paths).unwrap();
        rename_session_dirs(&paths).unwrap();
        assert!(paths.sessions_dir().join("ses_00000000000000000000000000000001/commands/cmd_00000000000000000000000000000002").is_dir());
    }

    #[test]
    fn conflicting_legacy_directory_error_includes_recovery_guidance() {
        let t = tempfile::tempdir().unwrap();
        let paths = ApiPaths::from_root(t.path());
        let legacy = paths
            .sessions_dir()
            .join("99999999-8888-7777-6666-555555555555");
        let current = paths
            .sessions_dir()
            .join("ses_99999999888877776666555555555555");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        let error = rename_session_dirs(&paths).unwrap_err().to_string();
        assert!(error.contains("inspect both directories"));
        assert!(legacy.is_dir());
        assert!(current.is_dir());
    }
}
