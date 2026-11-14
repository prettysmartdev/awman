//! Refresh tokens are owner-only state outside config and command records.
use crate::data::{
    config::fields,
    error::DataError,
    fs::{data_paths::DataPaths, private_files},
};
use awman_wsp::LinkToken;
use std::path::PathBuf;

#[derive(Clone)]
pub struct LinkStore {
    dir: PathBuf,
}

impl LinkStore {
    pub fn new(paths: &DataPaths) -> Self {
        Self {
            dir: paths.root().join("work_servers"),
        }
    }

    fn path(&self, name: &str) -> Result<PathBuf, DataError> {
        fields::resolve(&format!("workServers.{name}.url"))
            .map_err(|error| DataError::Other(error.to_string()))?;
        Ok(self.dir.join(format!("{name}.json")))
    }

    pub fn read(&self, name: &str) -> Result<Option<LinkToken>, DataError> {
        private_files::read_private_json(&self.path(name)?)
    }

    pub fn write(&self, name: &str, token: &LinkToken) -> Result<(), DataError> {
        let path = self.path(name)?;
        private_files::create_private_dir(&self.dir)?;
        private_files::write_private_json_atomic(&path, token, || Ok(()))
    }

    pub fn remove(&self, name: &str) -> Result<(), DataError> {
        private_files::remove_private_file(&self.path(name)?)?;
        private_files::remove_private_file(&self.path(name)?.with_extension("retired"))
    }
    /// Durable unlink intent survives a crash before the last lease finishes.
    pub fn defer_remove(&self, name: &str) -> Result<(), DataError> {
        private_files::create_private_dir(&self.dir)?;
        private_files::write_private_json_atomic(
            &self.path(name)?.with_extension("retired"),
            &true,
            || Ok(()),
        )
    }
    pub fn retired(&self) -> Result<Vec<String>, DataError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => {
                return Err(DataError::Io {
                    path: self.dir.clone(),
                    source: error,
                });
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| DataError::Io {
                path: self.dir.clone(),
                source: error,
            })?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "retired") {
                if let Some(name) = path.file_stem().and_then(|name| name.to_str()) {
                    self.path(name)?;
                    if private_files::read_private_json::<bool>(&path)? == Some(true) {
                        names.push(name.to_string());
                    }
                }
            }
        }
        Ok(names)
    }
}
