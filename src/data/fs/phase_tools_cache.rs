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

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(at: &str, git: bool) -> PhaseTools {
        PhaseTools {
            tools: BTreeMap::from([("git".to_string(), git)]),
            user: Some("root".into()),
            uid: Some(0),
            probed_at: at.into(),
        }
    }

    #[test]
    fn put_get_roundtrip_persists_under_the_data_home() {
        let home = tempfile::tempdir().unwrap();
        let mut cache = PhaseToolsCache::load(home.path());
        assert!(cache.get("sha256:a").is_none());
        cache.put("sha256:a".into(), tools("1", true)).unwrap();
        assert!(PhaseToolsCache::path_for(home.path()).ends_with("cache/phase-tools.json"));
        let reloaded = PhaseToolsCache::load(home.path());
        assert_eq!(reloaded.get("sha256:a"), Some(&tools("1", true)));
        assert!(reloaded.get("sha256:b").is_none());
    }

    #[test]
    fn eviction_drops_the_oldest_probe_beyond_the_cap() {
        let mut cache = PhaseToolsCache::in_memory();
        for i in 0..PHASE_TOOLS_CACHE_MAX_ENTRIES {
            cache
                .put(format!("img{i}"), tools(&format!("{:05}", i + 1), true))
                .unwrap();
        }
        assert!(cache.get("img0").is_some());
        cache.put("fresh".into(), tools("99999", false)).unwrap();
        assert!(cache.get("img0").is_none(), "oldest entry evicted");
        assert!(cache.get("img1").is_some());
        assert_eq!(cache.get("fresh"), Some(&tools("99999", false)));
    }

    #[test]
    fn in_memory_cache_writes_nothing_to_disk() {
        let mut cache = PhaseToolsCache::in_memory();
        cache.put("a".into(), tools("1", true)).unwrap();
        assert!(cache.get("a").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn corrupt_or_oversized_cache_loads_empty() {
        let home = tempfile::tempdir().unwrap();
        let path = PhaseToolsCache::path_for(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "garbage").unwrap();
        assert!(PhaseToolsCache::load(home.path()).get("a").is_none());
        // Reading a missing file is an empty cache, not a panic.
        let empty = tempfile::tempdir().unwrap();
        assert!(PhaseToolsCache::load(empty.path()).get("a").is_none());
    }
}
