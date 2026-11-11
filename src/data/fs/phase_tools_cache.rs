//! Digest-keyed cache of nonsecret phase-container tool metadata.

use crate::data::error::DataError;
use crate::data::fs::private_files::{
    create_private_dir, read_private_json_or_miss, write_private_json_atomic,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const PHASE_TOOLS_CACHE_RELATIVE: &str = "cache/phase-tools.json";
pub const PHASE_TOOLS_CACHE_MAX_ENTRIES: usize = 256;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseTools {
    pub tools: BTreeMap<String, bool>,
    pub user: Option<String>,
    pub uid: Option<u32>,
    pub probed_at: String,
}
#[derive(Debug, Clone)]
pub struct PhaseToolsCache {
    path: PathBuf,
    entries: BTreeMap<String, PhaseTools>,
}
impl PhaseToolsCache {
    pub fn path_for(data_home: &Path) -> PathBuf {
        data_home.join(PHASE_TOOLS_CACHE_RELATIVE)
    }
    pub fn load(data_home: &Path) -> Self {
        let path = Self::path_for(data_home);
        let entries = read_private_json_or_miss::<BTreeMap<String, PhaseTools>>(&path, 1024 * 1024)
            .ok()
            .flatten()
            .filter(|entries| entries.len() <= PHASE_TOOLS_CACHE_MAX_ENTRIES)
            .unwrap_or_default();
        Self { path, entries }
    }
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            entries: BTreeMap::new(),
        }
    }
    pub fn get(&self, image_key: &str) -> Option<&PhaseTools> {
        self.entries.get(image_key)
    }
    pub fn put(&mut self, image_key: String, tools: PhaseTools) -> Result<(), DataError> {
        let mut entries = self.entries.clone();
        entries.insert(image_key, tools);
        while entries.len() > PHASE_TOOLS_CACHE_MAX_ENTRIES {
            let oldest = entries
                .iter()
                .min_by(|a, b| a.1.probed_at.cmp(&b.1.probed_at))
                .map(|(key, _)| key.clone())
                .expect("nonempty cache");
            entries.remove(&oldest);
        }
        if !self.path.as_os_str().is_empty() {
            let parent = self.path.parent().unwrap_or(Path::new("."));
            if !parent.exists() {
                create_private_dir(parent)?;
            }
            write_private_json_atomic(&self.path, &entries, || Ok(()))?;
        }
        self.entries = entries;
        Ok(())
    }
}
