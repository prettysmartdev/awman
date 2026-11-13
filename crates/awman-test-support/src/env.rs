//! [`EnvMap`]: the complete environment of a child process, built from nothing.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// A complete child environment. It never starts from the parent process:
/// [`crate::TestSandbox`] fills it with the sandbox's roots and a fixed
/// system `PATH`, and tests change it through [`crate::TestSandbox::env_mut`]
/// or [`crate::OwnedCommand::env`].
///
/// On Windows, variable names compare case-insensitively (`Path` == `PATH`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvMap {
    vars: BTreeMap<OsString, OsString>,
}

fn key(name: &OsStr) -> OsString {
    if cfg!(windows) {
        OsString::from(name.to_string_lossy().to_uppercase())
    } else {
        name.to_os_string()
    }
}

impl EnvMap {
    /// An empty map.
    pub fn new() -> EnvMap {
        EnvMap::default()
    }

    /// Set `name` to `value`, replacing any previous value.
    pub fn set(&mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut EnvMap {
        self.vars
            .insert(key(name.as_ref()), value.as_ref().to_os_string());
        self
    }

    /// Remove `name`.
    pub fn remove(&mut self, name: impl AsRef<OsStr>) -> &mut EnvMap {
        self.vars.remove(&key(name.as_ref()));
        self
    }

    /// The value of `name`.
    pub fn get(&self, name: impl AsRef<OsStr>) -> Option<&OsStr> {
        self.vars.get(&key(name.as_ref())).map(OsString::as_os_str)
    }

    /// The value of `name` as UTF-8 text.
    pub fn get_str(&self, name: impl AsRef<OsStr>) -> Option<&str> {
        self.get(name).and_then(OsStr::to_str)
    }

    /// Every variable, sorted by name.
    pub fn iter(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.vars
            .iter()
            .map(|(k, v)| (k.as_os_str(), v.as_os_str()))
    }

    /// Number of variables.
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// Put `dir` first on `PATH` (never removes the existing entries, so a
    /// test cannot empty `PATH` by accident).
    pub fn prepend_path(&mut self, dir: &Path) -> &mut EnvMap {
        let mut dirs = vec![dir.to_path_buf()];
        dirs.extend(self.path_dirs().into_iter().filter(|d| d != dir));
        let joined = std::env::join_paths(dirs).expect("PATH entry contains a separator");
        self.set("PATH", joined)
    }

    /// The `PATH` entries, in order.
    pub fn path_dirs(&self) -> Vec<PathBuf> {
        self.get("PATH")
            .map(|p| std::env::split_paths(p).collect())
            .unwrap_or_default()
    }

    /// Resolve a bare program name through this map's `PATH` (never the
    /// parent's). A name containing a path separator is returned as is.
    pub fn resolve(&self, program: &OsStr) -> Option<PathBuf> {
        let candidate = Path::new(program);
        if candidate.components().count() > 1 || candidate.is_absolute() {
            return Some(candidate.to_path_buf());
        }
        let extensions: Vec<OsString> = if cfg!(windows) && candidate.extension().is_none() {
            vec![".exe".into(), ".cmd".into(), ".bat".into(), ".com".into()]
        } else {
            vec![OsString::new()]
        };
        for dir in self.path_dirs() {
            for ext in &extensions {
                let mut name = program.to_os_string();
                name.push(ext);
                let full = dir.join(&name);
                if full.is_file() {
                    return Some(full);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_remove() {
        let mut env = EnvMap::new();
        env.set("A", "1").set("B", "2");
        assert_eq!(env.get_str("A"), Some("1"));
        env.remove("A");
        assert_eq!(env.get("A"), None);
        assert_eq!(env.len(), 1);
        assert!(!env.is_empty());
    }

    #[test]
    fn iteration_is_sorted() {
        let mut env = EnvMap::new();
        env.set("Z", "z").set("A", "a");
        let names: Vec<_> = env.iter().map(|(k, _)| k.to_owned()).collect();
        assert_eq!(names, vec![OsString::from("A"), OsString::from("Z")]);
    }

    /// Guards: 8b4d5273 tests emptied `PATH`.
    #[test]
    fn prepend_path_keeps_existing_entries() {
        let mut env = EnvMap::new();
        let sys = std::env::join_paths([Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
        env.set("PATH", sys);
        env.prepend_path(Path::new("/sandbox/bin"));
        env.prepend_path(Path::new("/sandbox/bin"));
        assert_eq!(
            env.path_dirs(),
            vec![
                PathBuf::from("/sandbox/bin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin")
            ]
        );
    }

    #[test]
    fn resolve_uses_only_this_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir
            .path()
            .join(if cfg!(windows) { "tool.exe" } else { "tool" });
        std::fs::write(&exe, b"").unwrap();
        let mut env = EnvMap::new();
        assert_eq!(env.resolve(OsStr::new("tool")), None);
        env.prepend_path(dir.path());
        assert_eq!(env.resolve(OsStr::new("tool")), Some(exe));
        assert_eq!(
            env.resolve(OsStr::new("/abs/tool")),
            Some(PathBuf::from("/abs/tool"))
        );
    }
}
