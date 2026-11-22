//! General host CLI lookup is distinct from installed first-party host pings.
#[test]
fn general_host_cli_lookup_retains_explicit_snapshot_cwd_semantics() {
    let s = crate::support::sandbox(
        "engine::host_cli::general_host_cli_lookup_retains_explicit_snapshot_cwd_semantics",
    );
    let fake = s.install_fake(crate::support::fake_cli_exe(), "claude");
    let local = s.work().join("bin");
    std::fs::create_dir_all(&local).unwrap();
    let program = awman_test_support::exe::install_as(fake.exe(), &local, "claude");
    let mut env = s.env().clone();
    env.set("PATH", "bin");
    let env = awman::data::config::env::EnvSnapshot::from_instance_env(
        env.iter()
            .map(|(k, v)| (k.to_string_lossy().into(), v.to_string_lossy().into()))
            .collect(),
        s.work(),
        vec![],
    );
    assert_eq!(awman::engine::host_cli::program_in(&env, "claude"), program);
    assert!(
        fake.calls().is_empty(),
        "lookup never executes the candidate"
    );
    s.finish();
}
