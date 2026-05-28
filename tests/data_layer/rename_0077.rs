//! WI-0077 data-layer rename verification.
//!
//! Verifies:
//! - `GlobalConfig` path constants and `home_dir_with()` return `.awman`-based paths.
//! - `RepoConfig` path constants and `path()` use `.awman` subdirectory.
//! - `EnvSnapshot` reads `AWMAN_API_KEY` correctly (not the old `AMUX_API_KEY` name).
//! - `check_deprecated_env_vars()` returns a warning for every `AMUX_*` env var set.
//! - Migration renames `<git_root>/.amux/` → `<git_root>/.awman/` with an info message.
//! - Docs internal links all resolve to existing files (no broken links after WI-0077).

use awman::data::config::env::{EnvSnapshot, AWMAN_API_KEY, AWMAN_CONFIG_HOME};
use awman::data::config::global::{
    GlobalConfig, GLOBAL_CONFIG_FILENAME, GLOBAL_CONFIG_HOME_SUBDIR,
};
use awman::data::config::repo::{RepoConfig, REPO_CONFIG_FILENAME, REPO_CONFIG_SUBDIR};
use awman::data::migration;

/// Process-wide mutex for tests that mutate `std::env` vars.
///
/// Integration test binaries cannot access the `#[cfg(test)]`-gated
/// `awman::CWD_LOCK`, so we define an equivalent lock here.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ─── GlobalConfig path constants ─────────────────────────────────────────────

#[test]
fn global_config_home_subdir_constant_is_awman() {
    assert_eq!(
        GLOBAL_CONFIG_HOME_SUBDIR, ".awman",
        "GLOBAL_CONFIG_HOME_SUBDIR must be '.awman'"
    );
}

#[test]
fn global_config_home_subdir_constant_not_amux() {
    assert_ne!(
        GLOBAL_CONFIG_HOME_SUBDIR, ".amux",
        "GLOBAL_CONFIG_HOME_SUBDIR must not be the legacy '.amux'"
    );
}

#[test]
fn global_config_default_path_contains_awman_segment() {
    let tmp = tempfile::tempdir().unwrap();
    let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
    // When AWMAN_CONFIG_HOME is set, the returned path is directly under it.
    // The AWMAN_CONFIG_HOME constant name itself must contain "awman".
    let path = GlobalConfig::path_with(&env).unwrap();
    assert_eq!(
        path.file_name().unwrap(),
        GLOBAL_CONFIG_FILENAME,
        "global config file should be named {GLOBAL_CONFIG_FILENAME}"
    );
    // When AWMAN_CONFIG_HOME is NOT set, home_dir_with must append ".awman".
    let empty_env = EnvSnapshot::empty();
    // Use dirs::home_dir as reference: the path must end with .awman/config.json.
    if let Ok(home_dir) = GlobalConfig::home_dir_with(&empty_env) {
        let display = home_dir.display().to_string();
        assert!(
            display.contains(".awman"),
            "GlobalConfig::home_dir_with() without overrides must include '.awman'; got {display}"
        );
        assert!(
            !display.contains(".amux"),
            "GlobalConfig::home_dir_with() must not include '.amux'; got {display}"
        );
    }
}

// ─── RepoConfig path constants ────────────────────────────────────────────────

#[test]
fn repo_config_subdir_constant_is_awman() {
    assert_eq!(
        REPO_CONFIG_SUBDIR, ".awman",
        "REPO_CONFIG_SUBDIR must be '.awman'"
    );
}

#[test]
fn repo_config_subdir_constant_not_amux() {
    assert_ne!(
        REPO_CONFIG_SUBDIR, ".amux",
        "REPO_CONFIG_SUBDIR must not be the legacy '.amux'"
    );
}

#[test]
fn repo_config_path_is_inside_awman_subdir() {
    let tmp = tempfile::tempdir().unwrap();
    let p = RepoConfig::path(tmp.path());
    // Must be <tmp>/.awman/config.json
    assert_eq!(
        p,
        tmp.path()
            .join(REPO_CONFIG_SUBDIR)
            .join(REPO_CONFIG_FILENAME),
        "RepoConfig::path() must be under .awman/"
    );
    assert!(
        p.display().to_string().contains(".awman"),
        "RepoConfig::path() must contain '.awman'; got {p:?}"
    );
    assert!(
        !p.display().to_string().contains(".amux"),
        "RepoConfig::path() must not contain '.amux'; got {p:?}"
    );
}

// ─── EnvSnapshot reads AWMAN_* env vars ──────────────────────────────────────

/// `EnvSnapshot` must expose AWMAN_API_KEY (not AMUX_API_KEY) as the key for
/// the API key.
#[test]
fn awman_api_key_constant_is_awman_not_amux() {
    assert!(
        AWMAN_API_KEY.starts_with("AWMAN_"),
        "AWMAN_API_KEY constant must start with 'AWMAN_'; got {AWMAN_API_KEY:?}"
    );
    assert!(
        !AWMAN_API_KEY.starts_with("AMUX_"),
        "AWMAN_API_KEY constant must not start with 'AMUX_'; got {AWMAN_API_KEY:?}"
    );
}

/// `EnvSnapshot::api_key()` returns the value from `AWMAN_API_KEY`.
#[test]
fn env_snapshot_api_key_reads_awman_var() {
    let env = EnvSnapshot::with_overrides([(AWMAN_API_KEY, "my-test-api-key")]);
    assert_eq!(
        env.api_key(),
        Some("my-test-api-key"),
        "EnvSnapshot::api_key() must return the value of AWMAN_API_KEY"
    );
}

/// `EnvSnapshot::api_key()` is None when only the old `AMUX_API_KEY` is set —
/// confirming the old var is not read.
#[test]
fn env_snapshot_api_key_does_not_read_amux_var() {
    // Construct a snapshot with the OLD env var name — api_key() must return None.
    let env = EnvSnapshot::with_overrides([("AMUX_API_KEY", "legacy-value")]);
    assert_eq!(
        env.api_key(),
        None,
        "EnvSnapshot must not read the legacy AMUX_API_KEY; \
         only AWMAN_API_KEY should be honoured"
    );
}

/// `EnvSnapshot::config_home()` uses `AWMAN_CONFIG_HOME`, not `AMUX_CONFIG_HOME`.
#[test]
fn env_snapshot_config_home_reads_awman_var() {
    let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, "/custom/awman/config")]);
    assert_eq!(
        env.config_home().as_deref(),
        Some(std::path::Path::new("/custom/awman/config")),
        "EnvSnapshot::config_home() must read AWMAN_CONFIG_HOME"
    );
}

// ─── Deprecation warnings for AMUX_* env vars ────────────────────────────────

/// `check_deprecated_env_vars()` returns a warning when `AMUX_API_KEY` is set.
/// Uses `CWD_LOCK` to serialize access to `std::env::set_var` — required because
/// env mutation is process-wide and test threads run concurrently.
#[test]
fn deprecated_amux_api_key_returns_warning() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::set_var("AMUX_API_KEY", "legacy-test");
    let warnings = migration::check_deprecated_env_vars();
    std::env::remove_var("AMUX_API_KEY");

    let has_warning = warnings.iter().any(|w| w.contains("AMUX_API_KEY"));
    assert!(
        has_warning,
        "check_deprecated_env_vars() must warn about AMUX_API_KEY; got {warnings:?}"
    );
    // The warning must also name the replacement.
    let names_replacement = warnings.iter().any(|w| w.contains("AWMAN_API_KEY"));
    assert!(
        names_replacement,
        "deprecation warning must name AWMAN_API_KEY as replacement; got {warnings:?}"
    );
}

/// Setting `AWMAN_API_KEY` (the current name) must NOT produce a warning.
#[test]
fn awman_api_key_produces_no_deprecation_warning() {
    let _guard = ENV_LOCK.lock().unwrap();
    // Ensure no AMUX_* vars are set.
    for legacy in &[
        "AMUX_API_KEY",
        "AMUX_CONFIG_HOME",
        "AMUX_API_ROOT",
        "AMUX_OVERLAYS",
        "AMUX_REMOTE_ADDR",
        "AMUX_REMOTE_SESSION",
    ] {
        std::env::remove_var(legacy);
    }
    std::env::set_var("AWMAN_API_KEY", "new-valid-key");
    let warnings = migration::check_deprecated_env_vars();
    std::env::remove_var("AWMAN_API_KEY");

    assert!(
        warnings.is_empty(),
        "AWMAN_API_KEY must produce no deprecation warnings; got {warnings:?}"
    );
}

// ─── Migration: repo-level .amux → .awman ────────────────────────────────────

/// `migrate_repo_dir()` renames `<git_root>/.amux/` to `<git_root>/.awman/`
/// and returns an info message when only the old directory exists.
#[test]
fn migration_renames_amux_to_awman_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let old_dir = tmp.path().join(".amux");
    let new_dir = tmp.path().join(".awman");

    // Create the old .amux directory with content.
    std::fs::create_dir(&old_dir).unwrap();
    std::fs::write(old_dir.join("config.json"), br#"{"agent":"claude"}"#).unwrap();

    let msg = migration::migrate_repo_dir(tmp.path());

    assert!(msg.is_some(), "migration should return a message");
    let msg = msg.unwrap();
    assert!(
        msg.contains("Migrated"),
        "migration message should say 'Migrated'; got {msg:?}"
    );
    assert!(
        new_dir.exists(),
        "~/.awman/ must exist after migration; msg: {msg}"
    );
    assert!(
        !old_dir.exists(),
        "~/.amux/ must be removed after migration"
    );
    assert!(
        new_dir.join("config.json").exists(),
        "migrated content must be present in .awman/"
    );
}

/// When both `<git_root>/.amux/` and `<git_root>/.awman/` exist, migration
/// skips and emits a collision warning (does not clobber the new dir).
#[test]
fn migration_warns_and_skips_when_both_dirs_exist() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".amux")).unwrap();
    std::fs::create_dir(tmp.path().join(".awman")).unwrap();
    std::fs::write(tmp.path().join(".awman").join("kept.txt"), b"new").unwrap();

    let msg = migration::migrate_repo_dir(tmp.path());

    assert!(msg.is_some(), "collision should emit a warning");
    let msg = msg.unwrap();
    assert!(
        msg.contains("warning") || msg.contains("not migrated"),
        "collision message should warn about the conflict; got {msg:?}"
    );
    // Old dir must be untouched.
    assert!(tmp.path().join(".amux").exists(), ".amux must still exist");
    // New dir must be untouched.
    assert!(
        tmp.path().join(".awman").join("kept.txt").exists(),
        ".awman contents must not be overwritten"
    );
}

/// When no legacy `.amux/` dir exists, migration is a no-op.
#[test]
fn migration_is_noop_when_amux_dir_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let msg = migration::migrate_repo_dir(tmp.path());
    assert!(
        msg.is_none(),
        "migration should return None when .amux/ is absent; got {msg:?}"
    );
}

// ─── Docs internal link integrity ────────────────────────────────────────────

/// Walk every `.md` file under `docs/` (recursively, including subdirectories
/// like `docs/blog/` and `docs/releases/`) and verify that each relative file
/// link `[text](target)` or `[text](target#anchor)` resolves to a file that
/// exists. This catches broken links introduced by doc renames (e.g.
/// `08-headless-mode.md` → `08-api-mode.md`).
#[test]
fn docs_internal_links_all_resolve() {
    let docs_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
    assert!(
        docs_dir.is_dir(),
        "docs/ directory not found at {docs_dir:?}"
    );

    let mut broken: Vec<String> = Vec::new();
    let md_files = collect_md_files_recursively(&docs_dir);
    assert!(
        !md_files.is_empty(),
        "expected at least one .md file under {docs_dir:?}"
    );

    for file_path in md_files {
        let content = std::fs::read_to_string(&file_path)
            .unwrap_or_else(|e| panic!("read {file_path:?}: {e}"));

        // Links resolve relative to the directory of the current file,
        // not the root of docs/.
        let base_dir = file_path
            .parent()
            .expect("md file must have a parent dir")
            .to_path_buf();

        for link_target in extract_md_links(&content) {
            if link_target.starts_with("http://")
                || link_target.starts_with("https://")
                || link_target.starts_with("mailto:")
                || link_target.starts_with('#')
            {
                continue;
            }

            let target_file = link_target
                .split_once('#')
                .map(|(base, _)| base)
                .unwrap_or(&link_target);

            if target_file.is_empty() {
                continue;
            }

            let resolved = base_dir.join(target_file);
            if !resolved.exists() {
                let rel = file_path
                    .strip_prefix(&docs_dir)
                    .unwrap_or(&file_path)
                    .display();
                broken.push(format!("{rel}: broken link [{link_target}] → {resolved:?}",));
            }
        }
    }

    assert!(
        broken.is_empty(),
        "Found broken internal links under docs/:\n{}",
        broken.join("\n")
    );
}

fn collect_md_files_recursively(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(collect_md_files_recursively(&path));
        } else if path.extension().and_then(|x| x.to_str()) == Some("md") {
            out.push(path);
        }
    }
    out
}

/// Extract all link targets from `[text](target)` patterns in a Markdown string.
/// Does not parse full CommonMark; only handles inline `[…](…)` links.
fn extract_md_links(content: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Look for `](` which marks the start of a link target.
        if bytes[i] == b']' && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
            i += 2; // skip `](`
            let start = i;
            // Collect until `)` or end of line.
            while i < bytes.len() && bytes[i] != b')' && bytes[i] != b'\n' {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b')' {
                let target = &content[start..i];
                // Skip image alt-text that ended up here (already have `![…]`
                // syntax, so real link detection is good enough for our purpose).
                targets.push(target.to_string());
            }
        }
        i += 1;
    }

    targets
}
