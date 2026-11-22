//! Durable profile mechanics through the production filesystem API.
use awman::data::{
    fs::{agent_profiles::*, private_files::*, DataPaths},
    ids::ExecutionId,
    session::AgentName,
    settings_profile::{ProfileState, SettingsProfileName},
};
use awman_test_support::TestSandbox;
use std::time::Duration;
fn fixture(s: &TestSandbox) -> (ProfileLayout, ProfileManifest) {
    let paths = DataPaths::from_env(&crate::support::env_snapshot(s)).unwrap();
    let repo = ProfileRepoBinding::new(&s.work()).unwrap();
    let agent = AgentName::new("claude").unwrap();
    let name = SettingsProfileName::parse("review").unwrap();
    (
        ProfileLayout::new(&paths, &repo, &agent, &name),
        ProfileManifest::new(&repo, &agent, &name),
    )
}
fn stage(lease: &ProfileLease, generation: u64) -> std::path::PathBuf {
    let p = lease.begin_staging().unwrap();
    write_private_json_atomic(
        &p.join("tree/settings.json"),
        &serde_json::json!({"generation":generation}),
        || Ok(()),
    )
    .unwrap();
    write_private_json_atomic(
        &p.join("snapshot.json"),
        &serde_json::json!({"generation":generation}),
        || Ok(()),
    )
    .unwrap();
    p
}
#[test]
#[cfg(unix)] // settings profiles are unix-only
fn layout_private_publication_and_retention() {
    let s = TestSandbox::new();
    let (layout, mut m) = fixture(&s);
    assert!(layout.root().starts_with(s.awman_home()));
    let lease = layout.try_lock(Duration::ZERO).unwrap().unwrap();
    lease.write_manifest(&m).unwrap();
    for n in 1..=4 {
        let p = stage(&lease, n);
        m.current_generation = n;
        lease.publish(p, &m).unwrap();
        lease.prune_generations(&[n, n - 1]).unwrap();
    }
    assert!(lease.generation_tree(4).is_dir());
    assert!(lease.generation_tree(3).is_dir());
    assert!(!lease.generation_tree(2).exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(layout.root())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(layout.root().join("manifest.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let original = std::fs::metadata(s.awman_home()).unwrap().permissions();
        std::fs::set_permissions(s.awman_home(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(layout.try_lock(Duration::ZERO).is_err());
        std::fs::set_permissions(s.awman_home(), original).unwrap();
    }
    let id = ExecutionId::new();
    assert!(lease
        .create_run_dir(&id)
        .unwrap()
        .starts_with(layout.root().join("runs")));
    lease.remove_run_dir(&id).unwrap();
}
#[test]
fn lease_contention_and_reopen_preserves_durable_state() {
    let s = crate::support::sandbox(
        "data::agent_profiles::lease_contention_and_reopen_preserves_durable_state",
    );
    let (layout, m) = fixture(&s);
    let lease = layout.try_lock(Duration::ZERO).unwrap().unwrap();
    lease.write_manifest(&m).unwrap();
    let other = layout.clone();
    let thread = std::thread::spawn(move || other.try_lock(Duration::ZERO).unwrap().is_none());
    assert!(thread.join().unwrap());
    drop(lease);
    let next = layout.try_lock(Duration::ZERO).unwrap().unwrap();
    assert_eq!(
        next.read_manifest().unwrap().unwrap().state,
        ProfileState::Inactive
    );
}
#[test]
#[cfg(unix)] // settings profiles are unix-only
fn orphan_generation_never_becomes_current_and_next_publication_recovers() {
    let s = TestSandbox::new();
    let (layout, mut m) = fixture(&s);
    let lease = layout.try_lock(Duration::ZERO).unwrap().unwrap();
    lease.write_manifest(&m).unwrap();
    let p = stage(&lease, 1);
    m.current_generation = 1;
    lease.publish(p, &m).unwrap();
    let p = stage(&lease, 2);
    std::fs::rename(&p, layout.root().join("generations/g-00000002")).unwrap();
    drop(lease);
    let lease = layout.try_lock(Duration::ZERO).unwrap().unwrap();
    assert_eq!(
        lease.read_manifest().unwrap().unwrap().current_generation,
        1
    );
    let p = stage(&lease, 2);
    m.current_generation = 2;
    lease.publish(p, &m).unwrap();
    assert_eq!(
        lease.read_manifest().unwrap().unwrap().current_generation,
        2
    );
}
#[test]
fn identity_schema_links_and_portable_names_fail_closed() {
    let s = crate::support::sandbox(
        "data::agent_profiles::identity_schema_links_and_portable_names_fail_closed",
    );
    let (layout, mut m) = fixture(&s);
    let lease = layout.try_lock(Duration::ZERO).unwrap().unwrap();
    m.provider = "codex".into();
    assert!(lease.write_manifest(&m).is_err());
    m.provider = "claude".into();
    m.schema = 2;
    assert!(lease.write_manifest(&m).is_err());
    for name in ["nul", "con", "com1", "lpt9", "../review"] {
        assert!(SettingsProfileName::parse(name).is_err());
    }
    #[cfg(unix)]
    {
        let manifest = layout.root().join("manifest.json");
        std::os::unix::fs::symlink(s.work().join("elsewhere"), manifest).unwrap();
        assert!(lease.read_manifest().is_err());
    }
}
#[test]
#[cfg(unix)] // settings profiles are unix-only
fn inactive_deletion_and_delete_launch_race_use_one_namespace() {
    let s = TestSandbox::new();
    let (layout, m) = fixture(&s);
    let lease = layout.try_lock(Duration::ZERO).unwrap().unwrap();
    lease.write_manifest(&m).unwrap();
    let other = layout.clone();
    assert!(other.try_lock(Duration::ZERO).unwrap().is_none());
    let namespace = lock_private_timeout(
        &layout
            .root()
            .parent()
            .unwrap()
            .join(".review.namespace.lock"),
        LockMode::Exclusive,
        Duration::ZERO,
        || Ok(()),
    )
    .unwrap()
    .unwrap();
    assert!(matches!(
        lease.remove_profile(),
        Err(awman::data::error::DataError::ProfileLockBusy)
    ));
    assert!(layout.root().join("manifest.json").exists());
    drop(namespace);
    let lease = layout.try_lock(Duration::ZERO).unwrap().unwrap();
    lease.remove_profile().unwrap();
    assert!(!layout.root().exists());
    let next = layout.try_lock(Duration::ZERO).unwrap().unwrap();
    assert!(next.read_manifest().unwrap().is_none());
}

#[test]
fn xdg_and_default_home_layout_have_one_datapaths_source() {
    use awman::data::config::env::EnvSnapshot;
    let s = TestSandbox::new();
    let home = s.home().to_string_lossy().into_owned();
    let xdg = s.tmp().join("xdg").to_string_lossy().into_owned();
    let custom = s.tmp().join("custom").to_string_lossy().into_owned();
    for (env, base) in [
        (
            EnvSnapshot::with_overrides([
                ("HOME", home.as_str()),
                ("XDG_DATA_HOME", xdg.as_str()),
                ("AWMAN_HOME", custom.as_str()),
            ]),
            std::path::PathBuf::from(&custom),
        ),
        (
            EnvSnapshot::with_overrides([("HOME", home.as_str()), ("XDG_DATA_HOME", xdg.as_str())]),
            std::path::PathBuf::from(&xdg).join("awman"),
        ),
        (
            EnvSnapshot::with_overrides([("HOME", home.as_str())]),
            std::path::PathBuf::from(&home).join(".awman"),
        ),
    ] {
        let paths = DataPaths::from_env(&env).unwrap();
        assert!(paths
            .agent_profiles_dir(&s.work())
            .starts_with(base.join("data/repos")));
    }
}

#[cfg(unix)]
#[test]
fn symlinked_home_ancestor_is_allowed_above_the_private_store_boundary() {
    use awman::data::config::env::EnvSnapshot;
    let s = TestSandbox::new();
    let alias = s.tmp().join("home-alias");
    std::os::unix::fs::symlink(s.home(), &alias).unwrap();
    let home = alias.to_string_lossy().into_owned();
    let env = EnvSnapshot::with_overrides([("HOME", home.as_str())]);
    let paths = DataPaths::from_env(&env).unwrap();
    let repo = ProfileRepoBinding::new(&s.work()).unwrap();
    let layout = ProfileLayout::new(
        &paths,
        &repo,
        &AgentName::new("claude").unwrap(),
        &SettingsProfileName::parse("review").unwrap(),
    );
    let lease = layout.try_lock(Duration::ZERO).unwrap().unwrap();
    lease
        .write_manifest(&ProfileManifest::new(
            &repo,
            &AgentName::new("claude").unwrap(),
            &SettingsProfileName::parse("review").unwrap(),
        ))
        .unwrap();
}
