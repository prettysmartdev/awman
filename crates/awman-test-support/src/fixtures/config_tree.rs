//! awman configuration files: global (`$AWMAN_HOME/config.json`, i.e.
//! `$HOME/.awman/config.json`) and per repository (`<repo>/.awman/config.json`).

use crate::TestSandbox;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Global and repository config written from JSON values.
///
/// ```
/// use awman_test_support::{fixtures::ConfigTree, TestSandbox};
/// let sandbox = TestSandbox::new();
/// let paths = ConfigTree::new()
///     .global(serde_json::json!({"agent": "claude"}))
///     .write(&sandbox, None);
/// assert!(paths.global.unwrap().ends_with(".awman/config.json"));
/// ```
#[derive(Debug, Clone, Default)]
pub struct ConfigTree {
    global: Option<Value>,
    repo: Option<Value>,
}

/// Where [`ConfigTree::write`] put the files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigPaths {
    /// The global config file, if written.
    pub global: Option<PathBuf>,
    /// The repository config file, if written.
    pub repo: Option<PathBuf>,
}

impl ConfigTree {
    /// No files.
    pub fn new() -> ConfigTree {
        ConfigTree::default()
    }

    /// The global config.
    pub fn global(mut self, value: Value) -> Self {
        self.global = Some(value);
        self
    }

    /// The repository config.
    pub fn repo(mut self, value: Value) -> Self {
        self.repo = Some(value);
        self
    }

    /// Write the files: global into the sandbox's `AWMAN_HOME`, repository
    /// config into `<repo_root>/.awman/` (required when a repo config is set).
    pub fn write(&self, sandbox: &TestSandbox, repo_root: Option<&Path>) -> ConfigPaths {
        let mut paths = ConfigPaths::default();
        if let Some(global) = &self.global {
            paths.global = Some(write_json(
                &sandbox.awman_home().join("config.json"),
                global,
            ));
        }
        if let Some(repo) = &self.repo {
            let root = repo_root.expect("a repository config needs repo_root");
            paths.repo = Some(write_json(&root.join(".awman/config.json"), repo));
        }
        paths
    }
}

fn write_json(path: &Path, value: &Value) -> PathBuf {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create config dir");
    let mut text = serde_json::to_string_pretty(value).expect("JSON");
    text.push('\n');
    std::fs::write(path, text).expect("write config");
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn writes_both_levels() {
        let sandbox = TestSandbox::new();
        let repo = sandbox.work().join("r");
        let paths = ConfigTree::new()
            .global(json!({"a": 1}))
            .repo(json!({"b": 2}))
            .write(&sandbox, Some(&repo));
        let global = std::fs::read_to_string(paths.global.unwrap()).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&global).unwrap(),
            json!({"a": 1})
        );
        assert_eq!(paths.repo.unwrap(), repo.join(".awman/config.json"));
    }
}
