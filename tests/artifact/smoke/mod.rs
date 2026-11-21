use crate::support;
#[test]
fn exact_executable_runs_a_command_in_a_private_repository() {
    let mut s = support::sandbox("smoke::exact_executable_runs_a_command_in_a_private_repository");
    let a = support::artifact(&mut s);
    let repo = awman_test_support::fixtures::RepoBuilder::new(&s, "app")
        .file("README.md", "artifact smoke\n")
        .init();
    let out = support::run_artifact(&s, &a)
        .current_dir(repo.path())
        .args(["config", "show", "--json"])
        .run();
    out.assert_success();
    let config: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("config show --json");
    assert!(config.is_object(), "{config}");
    support::no_helpers(&s);
}
