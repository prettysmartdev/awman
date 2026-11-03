//! Direct tests of the Claude credential-scrubbing helpers in `claude.rs`.
//!
//! These call the helpers themselves (not only through `OverlayEngine`) and
//! check that secrets are actually absent from every staged byte, including
//! edge cases the engine-level tests in `tests.rs` do not reach: nested hard
//! links and symlinks, a symlinked host credential, special files, unparsable
//! input and non-object JSON.

use super::*;
use crate::data::session::AgentName;
use crate::engine::auth::keychain::AgentSecretFile;

const SENTINEL: &str = "SENTINEL-REFRESH-MUST-NOT-LEAK";

/// Every regular file under `dir` (recursively), refusing symlinks outright.
fn staged_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(
            !meta.file_type().is_symlink(),
            "staged tree must hold no symlinks: {}",
            path.display()
        );
        if meta.is_dir() {
            out.extend(staged_files(&path));
        } else {
            assert!(
                meta.is_file(),
                "only regular files may be staged: {}",
                path.display()
            );
            out.push(path);
        }
    }
    out
}

fn assert_sentinel_absent(dir: &Path) {
    for path in staged_files(dir) {
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(SENTINEL),
            "secret leaked into staged file {}",
            path.display()
        );
    }
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn host_claude_dir(root: &Path) -> PathBuf {
    let dir = root.join(".claude");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat-HOST","refreshToken":"{SENTINEL}"}}}}"#
        ),
    )
    .unwrap();
    dir
}

// ─── sanitize_claude_config ────────────────────────────────────────────────

#[test]
fn sanitize_config_removes_oauth_account_and_keeps_everything_else() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join(".claude.json");
    std::fs::write(
        &src,
        format!(
            r#"{{"oauthAccount":{{"emailAddress":"a@b","token":"{SENTINEL}"}},"model":"m","numStartups":3}}"#
        ),
    )
    .unwrap();
    let (dir, dest) = sanitize_claude_config(&src).unwrap();
    assert!(dest.starts_with(dir.path()));
    assert_eq!(dest.file_name().unwrap(), "claude.json");
    let raw = std::fs::read_to_string(&dest).unwrap();
    assert!(
        !raw.contains("oauthAccount") && !raw.contains(SENTINEL),
        "{raw}"
    );
    let v = read_json(&dest);
    assert_eq!(v["model"], "m");
    assert_eq!(v["numStartups"], 3);
    assert_eq!(v["projects"]["/workspace"]["hasTrustDialogAccepted"], true);
    // The host file is never modified.
    assert!(std::fs::read_to_string(&src).unwrap().contains(SENTINEL));
}

#[test]
fn sanitize_config_merges_trust_into_existing_projects_without_dropping_them() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join(".claude.json");
    std::fs::write(
        &src,
        r#"{"projects":{"/workspace":{"hasTrustDialogAccepted":false,"allowedTools":["x"]},"/other":{"k":1}}}"#,
    )
    .unwrap();
    let (_dir, dest) = sanitize_claude_config(&src).unwrap();
    let v = read_json(&dest);
    assert_eq!(v["projects"]["/workspace"]["hasTrustDialogAccepted"], true);
    assert_eq!(v["projects"]["/workspace"]["allowedTools"][0], "x");
    assert_eq!(v["projects"]["/other"]["k"], 1);
}

#[test]
fn sanitize_config_replaces_unparsable_input_instead_of_copying_it() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join(".claude.json");
    // Truncated JSON that still carries the secret text.
    std::fs::write(&src, format!(r#"{{"oauthAccount":{{"token":"{SENTINEL}""#)).unwrap();
    let (_dir, dest) = sanitize_claude_config(&src).unwrap();
    let raw = std::fs::read_to_string(&dest).unwrap();
    assert!(!raw.contains(SENTINEL), "{raw}");
    assert_eq!(
        read_json(&dest),
        serde_json::json!({"projects": {"/workspace": {"hasTrustDialogAccepted": true}}})
    );
}

#[test]
fn sanitize_config_ignores_non_object_projects_without_failing() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join(".claude.json");
    std::fs::write(
        &src,
        format!(r#"{{"projects":[1],"oauthAccount":"{SENTINEL}"}}"#),
    )
    .unwrap();
    let (_dir, dest) = sanitize_claude_config(&src).unwrap();
    let v = read_json(&dest);
    assert!(v.get("oauthAccount").is_none());
    assert_eq!(v["projects"], serde_json::json!([1]));
}

#[test]
fn sanitize_config_errors_on_a_missing_source() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(sanitize_claude_config(&tmp.path().join("absent.json")).is_err());
}

/// A non-UTF-8 `~/.claude.json` must still be sanitized (decoded lossily), and
/// the host file itself must never be mounted — otherwise `oauthAccount`
/// reaches the container and the agent can rewrite the host's config.
#[test]
fn non_utf8_claude_config_is_never_mounted_unsanitized() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join(".claude.json");
    let mut body = format!(r#"{{"oauthAccount":{{"token":"{SENTINEL}"}},"note":""#).into_bytes();
    body.push(0xff);
    body.extend_from_slice(br#""}"#);
    std::fs::write(&src, &body).unwrap();

    let engine = OverlayEngine::with_auth_resolver(
        crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
    )
    .with_secret_files_provider(std::sync::Arc::new(|_| Vec::new()));
    let agent = AgentName::new("claude").unwrap();
    let overlays = engine.agent_settings_overlays(&agent, tmp.path()).unwrap();
    let config = overlays
        .iter()
        .find(|o| {
            o.container_path
                .to_string_lossy()
                .ends_with("/.claude.json")
        })
        .expect(".claude.json overlay");
    assert_ne!(
        config.host_path, src,
        "the host file itself must never be mounted"
    );
    let staged = std::fs::read(&config.host_path).unwrap();
    assert!(!String::from_utf8_lossy(&staged).contains(SENTINEL));
}

// ─── sanitize_claude_settings_dir ──────────────────────────────────────────

#[test]
fn settings_dir_drops_every_denylisted_top_level_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let src = host_claude_dir(tmp.path());
    for name in CLAUDE_DENYLIST {
        let path = src.join(name);
        if !path.exists() {
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("x"), SENTINEL).unwrap();
        }
    }
    std::fs::write(src.join("CLAUDE.md"), "keep").unwrap();
    let (_tmp, root) = sanitize_claude_settings_dir(&src, false).unwrap();
    for name in CLAUDE_DENYLIST {
        assert!(!root.join(name).exists(), "{name} must not be staged");
    }
    assert_eq!(
        std::fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
        "keep"
    );
    assert_sentinel_absent(&root);
}

#[test]
fn settings_dir_denies_credential_aliases_at_every_depth() {
    let tmp = tempfile::tempdir().unwrap();
    let src = host_claude_dir(tmp.path());
    let host_credential = src.join(".credentials.json");
    let deep = src.join("plugins").join("cache").join("deeper");
    std::fs::create_dir_all(&deep).unwrap();
    // Case variants, nested.
    std::fs::write(src.join(".CREDENTIALS.JSON"), SENTINEL).unwrap();
    std::fs::write(deep.join(".Credentials.Json"), SENTINEL).unwrap();
    // Nested hard link and nested symlink to the host credential.
    std::fs::hard_link(&host_credential, deep.join("blob.bin")).unwrap();
    std::os::unix::fs::symlink(&host_credential, deep.join("alias.json")).unwrap();
    // A nested symlinked directory pointing back at the settings root.
    std::os::unix::fs::symlink(&src, src.join("plugins").join("loop")).unwrap();
    // Benign nested content must survive.
    std::fs::write(deep.join("plugin.json"), r#"{"ok":true}"#).unwrap();

    let (_tmp, root) = sanitize_claude_settings_dir(&src, false).unwrap();
    assert_sentinel_absent(&root);
    let staged = root.join("plugins").join("cache").join("deeper");
    assert!(staged.join("plugin.json").is_file());
    assert!(!staged.join("blob.bin").exists());
    assert!(!staged.join("alias.json").exists());
    assert!(!root.join("plugins").join("loop").exists());
}

#[test]
fn settings_dir_follows_a_symlinked_host_credential_to_its_inode() {
    // ~/.claude/.credentials.json may itself be a symlink to a file kept
    // elsewhere; a hard link to that target under any name is still refused.
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let real = vault.join("cred.json");
    std::fs::write(&real, SENTINEL).unwrap();
    let src = tmp.path().join(".claude");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::os::unix::fs::symlink(&real, src.join(".credentials.json")).unwrap();
    std::fs::hard_link(&real, src.join("copy.json")).unwrap();
    std::fs::hard_link(&real, src.join("sub").join("copy.json")).unwrap();

    let (_tmp, root) = sanitize_claude_settings_dir(&src, false).unwrap();
    assert_sentinel_absent(&root);
    assert!(!root.join("copy.json").exists());
    assert!(!root.join("sub").join("copy.json").exists());
}

#[test]
fn settings_dir_skips_fifos_without_blocking() {
    let tmp = tempfile::tempdir().unwrap();
    let src = host_claude_dir(tmp.path());
    std::fs::create_dir_all(src.join("sub")).unwrap();
    // Copying a FIFO would block forever; it must simply be skipped.
    nix::unistd::mkfifo(&src.join("pipe"), nix::sys::stat::Mode::S_IRWXU).unwrap();
    nix::unistd::mkfifo(&src.join("sub").join("pipe"), nix::sys::stat::Mode::S_IRWXU).unwrap();
    let (_tmp, root) = sanitize_claude_settings_dir(&src, false).unwrap();
    assert!(!root.join("pipe").exists());
    assert!(!root.join("sub").join("pipe").exists());
    assert!(root.join("sub").is_dir());
}

#[test]
fn settings_dir_merges_host_settings_and_injects_flags() {
    let tmp = tempfile::tempdir().unwrap();
    let src = host_claude_dir(tmp.path());
    std::fs::write(
        src.join("settings.json"),
        r#"{"theme":"dark","lspRecommendationDismissed":false}"#,
    )
    .unwrap();

    let (_a, plain) = sanitize_claude_settings_dir(&src, false).unwrap();
    let v = read_json(&plain.join("settings.json"));
    assert_eq!(v["theme"], "dark");
    assert_eq!(v["lspRecommendationDismissed"], true);
    assert_eq!(v["hasShownLspRecommendation"], true);
    assert!(v.get("permissionMode").is_none());
    assert!(v.get("skipDangerousModePermissionPrompt").is_none());

    let (_b, yolo) = sanitize_claude_settings_dir(&src, true).unwrap();
    let v = read_json(&yolo.join("settings.json"));
    assert_eq!(v["theme"], "dark");
    assert_eq!(v["permissionMode"], "bypassPermissions");
    assert_eq!(v["skipDangerousModePermissionPrompt"], true);
    // The host settings file is untouched.
    assert!(!std::fs::read_to_string(src.join("settings.json"))
        .unwrap()
        .contains("bypassPermissions"));
}

#[test]
fn settings_dir_replaces_unparsable_host_settings() {
    let tmp = tempfile::tempdir().unwrap();
    let src = host_claude_dir(tmp.path());
    std::fs::write(src.join("settings.json"), "{broken").unwrap();
    let (_tmp, root) = sanitize_claude_settings_dir(&src, false).unwrap();
    assert_eq!(
        read_json(&root.join("settings.json")),
        serde_json::json!({"hasShownLspRecommendation": true, "lspRecommendationDismissed": true})
    );
}

#[test]
fn settings_dir_of_a_missing_source_is_just_the_injected_settings() {
    let tmp = tempfile::tempdir().unwrap();
    let (_tmp, root) = sanitize_claude_settings_dir(&tmp.path().join("absent"), false).unwrap();
    let names: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("settings.json")]);
}

// ─── synthesised trees ─────────────────────────────────────────────────────

#[test]
fn minimal_config_is_trust_only() {
    let (_tmp, dest) = synthesize_minimal_claude_config().unwrap();
    assert_eq!(
        read_json(&dest),
        serde_json::json!({"projects": {"/workspace": {"hasTrustDialogAccepted": true}}})
    );
}

#[test]
fn minimal_settings_dir_matches_yolo_flag() {
    let (_a, plain) = synthesize_minimal_claude_settings_dir(false).unwrap();
    let v = read_json(&plain.join("settings.json"));
    assert_eq!(v["lspRecommendationDismissed"], true);
    assert!(v.get("permissionMode").is_none());
    assert_eq!(staged_files(&plain).len(), 1);

    let (_b, yolo) = synthesize_minimal_claude_settings_dir(true).unwrap();
    let v = read_json(&yolo.join("settings.json"));
    assert_eq!(v["permissionMode"], "bypassPermissions");
    assert_eq!(v["skipDangerousModePermissionPrompt"], true);
}

#[test]
fn staged_secret_files_land_with_their_mode_beside_the_host_copy() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join(".gemini");
    std::fs::create_dir_all(src.join("nested")).unwrap();
    std::fs::write(src.join("nested").join("settings.json"), "{}").unwrap();
    let secret = AgentSecretFile {
        relative_path: PathBuf::from("cli/token"),
        contents: b"tok".to_vec(),
        mode: 0o600,
    };
    let (_a, root) =
        stage_settings_dir_with_secrets(&src, std::slice::from_ref(&secret), "awman-t-").unwrap();
    assert!(root.join("nested").join("settings.json").is_file());
    assert_eq!(std::fs::read(root.join("cli/token")).unwrap(), b"tok");
    assert_eq!(
        std::fs::metadata(root.join("cli/token"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let (_b, fresh) = synthesize_settings_dir_with_secrets(&[secret], "awman-t-").unwrap();
    assert_eq!(staged_files(&fresh), vec![fresh.join("cli/token")]);
}
