//! `GitHubTokenResolver` coverage beyond the inline tests: the `gh` status
//! mapping, the fixed argv of every `gh` command, host validation at the
//! command boundary, the test-isolation rule for `gh` on `PATH`, the
//! repository boundary, and the trait defaults. Every `gh` here is a fake
//! shell script inside a temp dir; nothing reaches the network.

use super::*;
use crate::engine::test_path::PathGuard;
use std::os::unix::fs::PermissionsExt;

/// A fake `gh` that succeeds for `auth status --hostname github.com`, fails
/// for any other status host, and prints a token for `auth token`.
const FAKE_GH: &str = r#"#!/bin/sh
case "$*" in
  "auth status --hostname github.com") exit 0 ;;
  "auth status --hostname "*) echo "not logged in" >&2; exit 1 ;;
  "auth token --hostname "*) echo "gho_fake_for_$4"; exit 0 ;;
esac
exit 2
"#;

fn write_exe(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn resolver_for(host: &str) -> GitHubTokenResolver {
    GitHubTokenResolver::new(GitHubTokenResolverOptions {
        host: host.into(),
        env_vars: vec![],
        verify: true,
        timeout: Duration::from_secs(10),
    })
    .with_env_snapshot(EnvSnapshot::empty())
}

/// Hold the process-wide PATH lock with `PATH` set to exactly `dir`.
fn path_only(dir: &Path) -> PathGuard {
    let guard = PathGuard::acquire();
    std::env::set_var("PATH", dir);
    guard
}

#[test]
fn gh_status_maps_exit_codes_and_missing_gh() {
    let dir = tempfile::tempdir().unwrap();
    let gh = write_exe(dir.path(), "gh", FAKE_GH);
    let gh = gh.canonicalize().unwrap();
    let _path = path_only(dir.path());

    let found = resolver_for("github.com").gh_on_path().unwrap();
    assert_eq!(found.canonicalize().unwrap(), gh);
    assert!(matches!(
        resolver_for("github.com").host_gh_status(),
        GhAuthStatus::LoggedIn { .. }
    ));
    assert!(matches!(
        resolver_for("ghe.example.com").host_gh_status(),
        GhAuthStatus::NotLoggedIn { .. }
    ));
    assert!(GitHubTokenResolver::host_gh_authenticated("github.com"));
    assert!(!GitHubTokenResolver::host_gh_authenticated(
        "ghe.example.com"
    ));

    // Through the trait, a different host is probed with that host.
    let r = resolver_for("github.com");
    assert!(matches!(
        TokenProbeSource::host_gh_status(&r, "ghe.example.com"),
        GhAuthStatus::NotLoggedIn { .. }
    ));
    assert!(matches!(
        TokenProbeSource::host_gh_status(&r, "github.com"),
        GhAuthStatus::LoggedIn { .. }
    ));

    // An invalid host never reaches `gh` as an argument.
    assert!(matches!(
        resolver_for("--help").host_gh_status(),
        GhAuthStatus::Unknown { .. }
    ));

    let empty = tempfile::tempdir().unwrap();
    std::env::set_var("PATH", empty.path());
    assert_eq!(
        resolver_for("github.com").host_gh_status(),
        GhAuthStatus::NotInstalled
    );
    assert!(matches!(
        resolver_for("github.com").gh_auth_token_command(HostStdin::Null),
        Err(HostSecretCommandError::NotFound { .. })
    ));
    assert!(matches!(
        resolver_for("github.com").gh_auth_login_command(),
        Err(HostSecretCommandError::NotFound { .. })
    ));
}

#[test]
fn gh_outside_the_temp_dir_is_invisible_under_test_isolation() {
    // A directory under the build tree, not the system temp dir, stands in
    // for a real install location.
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
    std::fs::create_dir_all(&base).unwrap();
    let dir = tempfile::tempdir_in(&base).unwrap();
    if inside_temp_dir(dir.path()) {
        eprintln!("SKIP: build tree lies inside the temp dir");
        return;
    }
    write_exe(dir.path(), "gh", FAKE_GH);
    let _path = path_only(dir.path());
    assert!(resolver_for("github.com").gh_on_path().is_none());
    assert_eq!(
        resolver_for("github.com").host_gh_status(),
        GhAuthStatus::NotInstalled
    );
}

#[test]
fn gh_token_and_login_commands_have_fixed_argv() {
    let dir = tempfile::tempdir().unwrap();
    write_exe(dir.path(), "gh", FAKE_GH);
    let _path = path_only(dir.path());
    let r = resolver_for("ghe.example.com:8443");

    let token = r.gh_auth_token_command(HostStdin::Null).unwrap();
    let review = token.review();
    assert_eq!(review.label, "gh auth token");
    assert_eq!(
        review.argv[1..],
        ["auth", "token", "--hostname", "ghe.example.com:8443"].map(String::from)
    );
    assert_eq!(review.timeout_secs, 10);
    let out = token.run().unwrap();
    assert_eq!(
        out.secret.unwrap().expose(),
        "gho_fake_for_ghe.example.com:8443"
    );

    let login = r.gh_auth_login_command().unwrap().review();
    assert_eq!(login.label, "gh auth login");
    assert_eq!(
        login.argv[1..],
        [
            "auth",
            "login",
            "--hostname",
            "ghe.example.com:8443",
            "--web",
            "--git-protocol",
            "https"
        ]
        .map(String::from)
    );
    assert_eq!(login.timeout_secs, HOST_SECRET_TIMEOUT_MAX.as_secs());

    for bad in ["evil.com/x", "-x", "a b", "h:"] {
        assert!(
            resolver_for(bad)
                .gh_auth_token_command(HostStdin::Null)
                .is_err(),
            "{bad}"
        );
        assert!(resolver_for(bad).gh_auth_login_command().is_err(), "{bad}");
    }
}

#[test]
fn host_tools_inside_the_repository_are_refused() {
    let repo = tempfile::tempdir().unwrap();
    let tool = write_exe(repo.path(), "op", "#!/bin/sh\necho tok\n");
    let policy = AcquirePolicy {
        argv: vec![tool.to_string_lossy().into(), "read".into()],
        timeout_secs: 5,
        refresh: Default::default(),
        scope: crate::data::workflow_prerequisites::AcquireScope::Global,
        sha256: String::new(),
    };
    let r = resolver_for("github.com").with_repository_root(repo.path().to_path_buf());
    let err = r
        .acquire_command(&policy, HostStdin::Null, &[])
        .unwrap_err();
    assert!(
        err.to_string().contains("outside the workflow repository"),
        "{err}"
    );

    // A missing repository root fails closed rather than skipping the check.
    let gone = repo.path().join("gone");
    let r = resolver_for("github.com").with_repository_root(gone);
    assert!(r.acquire_command(&policy, HostStdin::Null, &[]).is_err());

    // Outside the repository the same command builds and runs.
    let other = tempfile::tempdir().unwrap();
    let r = resolver_for("github.com").with_repository_root(other.path().to_path_buf());
    let out = TokenProbeSource::acquire_token(&r, &policy, &[]).unwrap();
    assert_eq!(out.secret.unwrap().expose(), "tok");
}

#[test]
fn acquire_labels_never_show_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let tool = write_exe(dir.path(), "vault", "#!/bin/sh\necho tok\n");
    let program = tool.to_string_lossy().to_string();
    let policy = |argv: Vec<String>| AcquirePolicy {
        argv,
        timeout_secs: 7,
        refresh: Default::default(),
        scope: crate::data::workflow_prerequisites::AcquireScope::Global,
        sha256: String::new(),
    };
    let r = resolver_for("github.com");
    let one = r
        .acquire_command(&policy(vec![program.clone()]), HostStdin::Null, &[])
        .unwrap()
        .review();
    assert_eq!(one.label, format!("configured token command ({program})"));
    let many = r
        .acquire_command(
            &policy(vec![program.clone(), "--secret-path".into(), "kv/x".into()]),
            HostStdin::Null,
            &[],
        )
        .unwrap()
        .review();
    assert_eq!(
        many.label,
        format!("configured token command ({program} …)")
    );
    assert!(!many.label.contains("kv/x"));
    assert_eq!(many.timeout_secs, 7);

    assert!(matches!(
        r.acquire_command(&policy(vec![]), HostStdin::Null, &[]),
        Err(HostSecretCommandError::EmptyArgv)
    ));
    // A refused binary is never built into a command.
    assert!(matches!(
        r.acquire_command(&policy(vec![program.clone()]), HostStdin::Null, &[program]),
        Err(HostSecretCommandError::RefusedAgentBinary { .. })
    ));
}

#[test]
fn verification_disabled_short_circuits_both_entry_points() {
    let r = GitHubTokenResolver::new(GitHubTokenResolverOptions {
        verify: false,
        ..Default::default()
    });
    let disabled = TokenVerification::Unknown {
        reason: "verification disabled".into(),
    };
    let token = SecretString::new("ghp_secret");
    assert_eq!(r.verify(&token), disabled);
    assert_eq!(
        TokenProbeSource::verify(&r, &token, "ghe.example.com"),
        disabled
    );
    // Enabled, the trait entry point is still inert under test isolation,
    // even for an invalid host.
    let r = resolver_for("github.com");
    assert_eq!(
        TokenProbeSource::verify(&r, &token, "evil.com/x"),
        TokenVerification::Unknown {
            reason: "test isolation".into()
        }
    );
    assert_eq!(verify_url("GitHub.COM"), "https://api.github.com/user");
}

#[test]
fn resolver_accessors_defaults_and_debug_redaction() {
    let defaults = GitHubTokenResolverOptions::default();
    assert_eq!(defaults.host, "github.com");
    assert!(defaults.verify);
    assert_eq!(defaults.timeout, GITHUB_TOKEN_PROBE_TIMEOUT);
    assert_eq!(
        defaults.env_vars,
        crate::data::config::workflows::PREREQUISITES_DEFAULT_ENV_VARS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );

    let r = GitHubTokenResolver::new(GitHubTokenResolverOptions {
        host: "ghe.example.com".into(),
        env_vars: vec!["A".into(), "B".into()],
        ..Default::default()
    })
    .with_env_snapshot(EnvSnapshot::with_overrides([
        ("A", ""),
        ("B", "ghp_value_b"),
    ]));
    assert_eq!(r.host(), "ghe.example.com");
    assert_eq!(r.options().env_vars, ["A", "B"]);
    assert_eq!(r.env_token_name().as_deref(), Some("B"));
    assert!(r.env_token("A").is_none());
    assert!(r.env_token("MISSING").is_none());
    assert!(TokenProbeSource::env_token_name(&r, &["A".into(), "MISSING".into()]).is_none());
    assert_eq!(
        TokenProbeSource::env_token(&r, "B").unwrap().expose(),
        "ghp_value_b"
    );
    let debug = format!("{r:?}");
    assert!(!debug.contains("ghp_value_b"), "{debug}");
    assert!(debug.contains("env_snapshot: true"), "{debug}");
}

#[test]
fn trait_defaults_are_inert() {
    struct NameOnly;
    impl TokenProbeSource for NameOnly {
        fn env_token_name(&self, _: &[String]) -> Option<String> {
            Some("GH_TOKEN".into())
        }
        fn gh_on_path(&self) -> Option<PathBuf> {
            None
        }
        fn host_gh_status(&self, _: &str) -> GhAuthStatus {
            GhAuthStatus::NotInstalled
        }
        fn verify(&self, _: &SecretString, _: &str) -> TokenVerification {
            TokenVerification::Unknown { reason: "x".into() }
        }
    }
    assert!(NameOnly.env_token("GH_TOKEN").is_none());
    let policy = AcquirePolicy {
        argv: vec!["/bin/true".into()],
        timeout_secs: 1,
        refresh: Default::default(),
        scope: crate::data::workflow_prerequisites::AcquireScope::Global,
        sha256: String::new(),
    };
    assert!(matches!(
        NameOnly.acquire_token(&policy, &[]),
        Err(HostSecretCommandError::Spawn { .. })
    ));
}

#[test]
fn host_validation_edge_cases() {
    for good in ["a", "github.com", "x-y.example.com", "h:1", "h:65535"] {
        assert!(valid_host(good), "{good}");
    }
    for bad in [
        ".github.com",
        "h:123456",
        "a:b:1",
        "[::1]",
        "h:-1",
        "gïthub.com",
        "h\n",
        "h#frag",
        ":443",
    ] {
        assert!(!valid_host(bad), "{bad:?}");
    }
}
