//! `awman config prerequisite-grants list|revoke` through the real binary.
//!
//! Each test gets a throwaway `HOME` (and so `~/.awman`) and an empty git
//! repository; records are seeded through the public store API, never by
//! hand-written JSON, except where a hostile file is the point. No Docker,
//! network or prompt is involved.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use awman::data::fs::prerequisite_grants::{
    GrantKey, PrerequisiteGrantStore, RememberedGrant, RememberedKind,
};

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("home/.awman")).unwrap();
        std::fs::create_dir_all(dir.path().join("repo")).unwrap();
        crate::git_init(&dir.path().join("repo"));
        Self { dir }
    }

    fn data_home(&self) -> PathBuf {
        self.dir.path().join("home/.awman")
    }

    fn grants_file(&self) -> PathBuf {
        PrerequisiteGrantStore::path_for(&self.data_home())
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo").canonicalize().unwrap()
    }

    fn seed(&self, records: &[(RememberedKind, RememberedGrant)]) {
        let mut store = PrerequisiteGrantStore::load(&self.data_home());
        for (i, (kind, grant)) in records.iter().enumerate() {
            store
                .remember(
                    GrantKey {
                        git_root: self.repo(),
                        remote_url: format!("https://github.com/example/repo{i}.git"),
                        prerequisite: *kind,
                    },
                    grant.clone(),
                )
                .unwrap();
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let home = self.dir.path().join("home");
        Command::new(crate::awman_bin())
            .args(args)
            .current_dir(self.repo())
            .env_clear()
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )
            .env("HOME", &home)
            .env("AWMAN_CONFIG_HOME", self.data_home())
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("TMPDIR", std::env::temp_dir())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("AWMAN_TEST_ISOLATION", "1")
            .envs(extra.iter().copied())
            .stdin(Stdio::null())
            .output()
            .expect("run awman")
    }

    fn records(&self) -> usize {
        PrerequisiteGrantStore::load(&self.data_home())
            .records()
            .len()
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn three_records() -> Vec<(RememberedKind, RememberedGrant)> {
    vec![
        (
            RememberedKind::GithubToken,
            RememberedGrant::TokenEnv {
                var: "GH_TOKEN".into(),
            },
        ),
        (RememberedKind::Ssh, RememberedGrant::SshDir),
        (
            RememberedKind::GitIdentity,
            RememberedGrant::Identity {
                name: "Ada Lovelace".into(),
                email: "ada@example.test".into(),
            },
        ),
    ]
}

#[test]
fn list_with_no_file_says_so_and_succeeds() {
    let f = Fixture::new();
    let out = f.run(&["config", "prerequisite-grants", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains("No remembered prerequisite decisions"),
        "{}",
        text(&out)
    );
    assert!(!f.grants_file().exists(), "listing never creates the file");
}

#[test]
fn list_numbers_every_kind_of_decision_in_file_order() {
    let f = Fixture::new();
    f.seed(&three_records());
    let out = f.run(&["config", "prerequisite-grants", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let body = text(&out);
    for needle in [
        "github token",
        "token from GH_TOKEN",
        "whole ~/.ssh",
        "identity Ada Lovelace <ada@example.test>",
        "https://github.com/example/repo0.git",
    ] {
        assert!(body.contains(needle), "missing {needle:?} in:\n{body}");
    }
    let token_at = body.find("token from GH_TOKEN").unwrap();
    let ssh_at = body.find("whole ~/.ssh").unwrap();
    let identity_at = body.find("identity Ada").unwrap();
    assert!(
        token_at < ssh_at && ssh_at < identity_at,
        "file order:\n{body}"
    );
}

#[test]
fn list_without_a_subcommand_is_not_a_silent_success() {
    let f = Fixture::new();
    f.seed(&three_records());
    let out = f.run(&["config", "prerequisite-grants"]);
    // Either the group lists by default or it asks for a subcommand; it must
    // never modify the store.
    assert!(
        out.status.code() == Some(0) || out.status.code() == Some(2),
        "{}",
        text(&out)
    );
    assert_eq!(f.records(), 3);
}

#[test]
fn revoking_one_renumbers_the_rest_and_persists() {
    let f = Fixture::new();
    f.seed(&three_records());
    let out = f.run(&["config", "prerequisite-grants", "revoke", "2"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let body = text(&out);
    assert!(body.contains("Revoked 1"), "{body}");
    assert!(!body.contains("whole ~/.ssh"), "{body}");
    assert!(body.contains("token from GH_TOKEN"), "{body}");
    assert!(body.contains("identity Ada Lovelace"), "{body}");
    assert_eq!(f.records(), 2, "the revoke is persisted");

    // The survivors are renumbered: the identity is now record 2.
    let out = f.run(&["config", "prerequisite-grants", "revoke", "2"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(!text(&out).contains("identity Ada"), "{}", text(&out));
    let store = PrerequisiteGrantStore::load(&f.data_home());
    assert_eq!(store.records().len(), 1);
    assert_eq!(
        store.records()[0].grant,
        RememberedGrant::TokenEnv {
            var: "GH_TOKEN".into()
        }
    );
}

#[test]
fn revoking_all_empties_the_store() {
    let f = Fixture::new();
    f.seed(&three_records());
    let out = f.run(&["config", "prerequisite-grants", "revoke", "all"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("Revoked 3"), "{}", text(&out));
    assert_eq!(f.records(), 0);
    let out = f.run(&["config", "prerequisite-grants", "list"]);
    assert!(text(&out).contains("No remembered prerequisite decisions"));
}

#[test]
fn an_out_of_range_record_number_fails_and_changes_nothing() {
    let f = Fixture::new();
    f.seed(&three_records());
    let before = std::fs::read(f.grants_file()).unwrap();
    let out = f.run(&["config", "prerequisite-grants", "revoke", "4"]);
    assert_ne!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains("no remembered prerequisite decision number 4"),
        "{}",
        text(&out)
    );
    assert_eq!(std::fs::read(f.grants_file()).unwrap(), before);
}

#[test]
fn a_malformed_selector_is_a_usage_error() {
    let f = Fixture::new();
    f.seed(&three_records());
    let plain = f.run(&["config", "prerequisite-grants", "revoke", "0"]);
    assert_eq!(plain.status.code(), Some(2), "{}", text(&plain));
    assert!(
        text(&plain).contains("expected a record number"),
        "{}",
        text(&plain)
    );
    for bad in ["one", "-1", "1.5", "ALL"] {
        let out = f.run(&["config", "prerequisite-grants", "revoke", bad]);
        assert_eq!(
            out.status.code(),
            plain.status.code(),
            "{bad}: {}",
            text(&out)
        );
    }
    assert_eq!(f.records(), 3);
}

#[test]
fn revoke_without_a_selector_is_a_usage_error() {
    let f = Fixture::new();
    f.seed(&three_records());
    let out = f.run(&["config", "prerequisite-grants", "revoke"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert_eq!(f.records(), 3);
}

#[cfg(unix)]
#[test]
fn a_file_open_to_other_users_is_ignored_with_a_warning() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.seed(&three_records());
    std::fs::set_permissions(f.grants_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = f.run(&["config", "prerequisite-grants", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let body = text(&out);
    assert!(
        body.contains("Warning:"),
        "an unsafe file is reported:\n{body}"
    );
    assert!(
        body.contains("No remembered prerequisite decisions"),
        "and never trusted:\n{body}"
    );
    assert!(!body.contains("token from GH_TOKEN"), "{body}");
}

#[test]
fn the_file_and_listing_hold_decisions_never_values() {
    const SECRET: &str = "ghp_prerequisite_grants_cli_never_shown";
    let f = Fixture::new();
    f.seed(&three_records());
    // The remembered variable holds a value in this run's environment; only
    // its name may ever be shown.
    let out = f.run_with_env(
        &["config", "prerequisite-grants", "list"],
        &[("GH_TOKEN", SECRET)],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("token from GH_TOKEN"), "{}", text(&out));
    assert!(!text(&out).contains(SECRET));
    assert!(!std::fs::read_to_string(f.grants_file())
        .unwrap()
        .contains(SECRET));
}
