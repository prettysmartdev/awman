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
                })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_roundtrip_is_private_and_names_cannot_traverse() {
        let dir = tempfile::tempdir().unwrap();
        let store = LinkStore::new(&DataPaths::at_root(dir.path().join("data")));
        let token = LinkToken {
            refresh_token: "secret".into(),
        };
        store.write("team", &token).unwrap();
        assert_eq!(store.read("team").unwrap(), Some(token));
        assert!(store
            .write(
                "../escape",
                &LinkToken {
                    refresh_token: "x".into()
                }
            )
            .is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("data/work_servers/team.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        store.remove("team").unwrap();
        assert!(store.read("team").unwrap().is_none());
    }
}

#[cfg(test)]
mod more_tests {
    use super::*;

    fn store() -> (tempfile::TempDir, LinkStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = LinkStore::new(&DataPaths::at_root(dir.path().join("data")));
        (dir, store)
    }

    #[test]
    fn bad_names_never_touch_the_filesystem() {
        let (dir, store) = store();
        let token = LinkToken {
            refresh_token: "x".into(),
        };
        for bad in ["", "..", "a/b", "A", "_x", "a\\b", "x.json/../y"] {
            assert!(store.write(bad, &token).is_err(), "{bad}");
            assert!(store.read(bad).is_err(), "{bad}");
            assert!(store.remove(bad).is_err(), "{bad}");
        }
        assert!(!dir.path().join("data/work_servers").exists());
    }

    #[test]
    fn removing_a_missing_token_is_fine_and_rewrite_replaces() {
        let (_dir, store) = store();
        store.remove("team").unwrap();
        for secret in ["one", "two"] {
            store
                .write(
                    "team",
                    &LinkToken {
                        refresh_token: secret.into(),
                    },
                )
                .unwrap();
        }
        assert_eq!(store.read("team").unwrap().unwrap().refresh_token, "two");
    }

    #[cfg(unix)]
    #[test]
    fn the_directory_and_file_are_owner_only_and_hold_only_the_token() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, store) = store();
        store
            .write(
                "team",
                &LinkToken {
                    refresh_token: "secret".into(),
                },
            )
            .unwrap();
        let root = dir.path().join("data/work_servers");
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o077,
            0
        );
        let file = root.join("team.json");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(doc, serde_json::json!({"refresh_token": "secret"}));
        // Leftover temp files from the atomic write must not linger.
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;
    #[test]
    fn deferred_retirement_survives_reopen_and_removes_token_and_marker() {
        let root = tempfile::tempdir().unwrap();
        let paths = DataPaths::at_root(root.path().join("data"));
        let store = LinkStore::new(&paths);
        store
            .write(
                "team",
                &LinkToken {
                    refresh_token: "secret".into(),
                },
            )
            .unwrap();
        store.defer_remove("team").unwrap();
        let recovered = LinkStore::new(&paths);
        assert_eq!(recovered.retired().unwrap(), vec!["team"]);
        assert!(recovered.read("team").unwrap().is_some());
        recovered.remove("team").unwrap();
        assert!(recovered.retired().unwrap().is_empty());
        assert!(recovered.read("team").unwrap().is_none());
    }
}
