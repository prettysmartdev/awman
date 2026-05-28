//! One-time migration from legacy `amux` paths and env vars to `awman`.

use std::path::Path;

const LEGACY_GLOBAL_DIR: &str = ".amux";
const NEW_GLOBAL_DIR: &str = ".awman";

const LEGACY_REPO_DIR: &str = ".amux";
const NEW_REPO_DIR: &str = ".awman";

const LEGACY_ENV_VARS: &[(&str, &str)] = &[
    ("AMUX_CONFIG_HOME", "AWMAN_CONFIG_HOME"),
    ("AMUX_API_ROOT", "AWMAN_API_ROOT"),
    ("AMUX_OVERLAYS", "AWMAN_OVERLAYS"),
    ("AMUX_REMOTE_ADDR", "AWMAN_REMOTE_ADDR"),
    ("AMUX_REMOTE_SESSION", "AWMAN_REMOTE_SESSION"),
    ("AMUX_API_KEY", "AWMAN_API_KEY"),
];

/// Migrate `~/.amux/` → `~/.awman/` if the old directory exists and the new
/// one does not. Returns a human-readable message if migration occurred or was
/// skipped due to collision.
pub fn migrate_global_dir() -> Option<String> {
    let home = dirs::home_dir()?;
    migrate_dir(&home, LEGACY_GLOBAL_DIR, NEW_GLOBAL_DIR)
}

/// Migrate `<git_root>/.amux/` → `<git_root>/.awman/` if the old directory
/// exists and the new one does not.
pub fn migrate_repo_dir(git_root: &Path) -> Option<String> {
    migrate_dir(git_root, LEGACY_REPO_DIR, NEW_REPO_DIR)
}

fn migrate_dir(parent: &Path, old_name: &str, new_name: &str) -> Option<String> {
    let old = parent.join(old_name);
    let new = parent.join(new_name);

    if !old.exists() {
        return None;
    }

    if new.exists() {
        return Some(format!(
            "warning: both {old_name}/ and {new_name}/ exist in {}; \
             {old_name}/ was not migrated — remove it manually once you have \
             verified {new_name}/ is correct.",
            parent.display()
        ));
    }

    match std::fs::rename(&old, &new) {
        Ok(()) => Some(format!("Migrated {} → {}", old.display(), new.display())),
        Err(e) => Some(format!(
            "warning: failed to migrate {} → {}: {e}",
            old.display(),
            new.display()
        )),
    }
}

/// Check for deprecated `AMUX_*` env vars and return deprecation warnings.
pub fn check_deprecated_env_vars() -> Vec<String> {
    let mut warnings = Vec::new();
    for (old, new) in LEGACY_ENV_VARS {
        if std::env::var_os(old).is_some() {
            warnings.push(format!(
                "warning: {old} is deprecated; use {new} instead. \
                 {old} is no longer read."
            ));
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_dir_renames_when_old_exists_and_new_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".amux")).unwrap();
        std::fs::write(tmp.path().join(".amux/test.txt"), b"hello").unwrap();

        let msg = migrate_dir(tmp.path(), ".amux", ".awman");
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("Migrated"));
        assert!(tmp.path().join(".awman/test.txt").exists());
        assert!(!tmp.path().join(".amux").exists());
    }

    #[test]
    fn migrate_dir_warns_when_both_exist() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".amux")).unwrap();
        std::fs::create_dir(tmp.path().join(".awman")).unwrap();

        let msg = migrate_dir(tmp.path(), ".amux", ".awman");
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("was not migrated"));
        assert!(tmp.path().join(".amux").exists());
        assert!(tmp.path().join(".awman").exists());
    }

    #[test]
    fn migrate_dir_returns_none_when_old_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let msg = migrate_dir(tmp.path(), ".amux", ".awman");
        assert!(msg.is_none());
    }

    #[test]
    fn check_deprecated_env_vars_detects_legacy() {
        let _guard = crate::CWD_LOCK.lock().unwrap();
        std::env::set_var("AMUX_CONFIG_HOME", "/tmp/test");
        let warnings = check_deprecated_env_vars();
        std::env::remove_var("AMUX_CONFIG_HOME");
        assert!(!warnings.is_empty());
        assert!(warnings[0].contains("AWMAN_CONFIG_HOME"));
    }

    #[test]
    fn check_deprecated_env_vars_empty_when_none_set() {
        let _guard = crate::CWD_LOCK.lock().unwrap();
        // Clear any AMUX_* var that another test may have left behind in the
        // shared process env before this one runs.
        for (old, _) in LEGACY_ENV_VARS {
            std::env::remove_var(old);
        }
        let warnings = check_deprecated_env_vars();
        let amux_count = warnings.iter().filter(|w| w.contains("AMUX_")).count();
        assert_eq!(amux_count, 0);
    }
}
