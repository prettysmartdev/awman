use crate::support;
const OPT_INS: &[&str] = &[
    "AWMAN_TEST_DOCKER",
    "AWMAN_TEST_APPLE_CONTAINER",
    "AWMAN_TEST_SBX",
    "AWMAN_TEST_BUILTIN",
];
fn observe(
    s: &awman_test_support::TestSandbox,
    a: &awman_test_support::capability::ReleaseArtifact,
    isolation: &str,
    opt: Option<&str>,
) -> awman_test_support::process::Output {
    let mut command = support::run_artifact(s, a)
        .env("AWMAN_TEST_ISOLATION", isolation)
        .args(["agents", "list", "--json"]);
    for switch in OPT_INS {
        command = command.env_remove(switch);
    }
    if let Some(switch) = opt {
        command = command.env(switch, "1");
    }
    command.run()
}
/// Guards: shipped switches never widen the non-isolated release's host access.
#[test]
fn runtime_opt_ins_have_no_effect_without_isolation() {
    let mut s = support::sandbox("switches::runtime_opt_ins_have_no_effect_without_isolation");
    let a = support::artifact(&mut s);
    // All observations use a private HOME, repo and network namespace even when
    // the release's own isolation switch is off. No real runtime is installed.
    let bin = s.bin().to_path_buf();
    s.env_mut().set("PATH", bin);
    let baseline = observe(&s, &a, "0", None);
    for switch in OPT_INS {
        let out = observe(&s, &a, "0", Some(switch));
        assert_eq!(out.exit, baseline.exit, "{switch}: {out:?}");
        assert_eq!(out.stdout, baseline.stdout, "{switch}");
        assert_eq!(out.stderr, baseline.stderr, "{switch}");
    }
    support::no_helpers(&s);
}
#[test]
fn test_isolation_only_reduces_access() {
    let mut s = support::sandbox("switches::test_isolation_only_reduces_access");
    let a = support::artifact(&mut s);
    // `ready` would drive the host runtime when isolation is off. An installed
    // fake records attempts; its path is outside TMPDIR so the release correctly
    // treats it as a real runtime rather than its temp-tree fake exception.
    let fake_exe = support::fake_cli_exe();
    s.register_exe("fake-cli", fake_exe);
    let fake = s.install_fake(fake_exe, "docker");
    fake.on(&["version"]).stdout("27.5.1\n");
    fake.on(&["info"]).stdout("{}\n");
    fake.on(&["ps"]).stdout("");
    let repo = awman_test_support::fixtures::RepoBuilder::new(&s, "app")
        .file("Dockerfile.dev", "FROM scratch\n")
        .init();
    let observe_ready = |isolated: &str, opt: Option<&str>| {
        let mut cmd = support::run_artifact(&s, &a)
            .env("AWMAN_TEST_ISOLATION", isolated)
            .current_dir(repo.path())
            .args(["ready", "--json"]);
        for name in OPT_INS {
            cmd = cmd.env_remove(name);
        }
        if let Some(name) = opt {
            cmd = cmd.env(name, "1");
        }
        cmd.run()
    };
    let before = fake.calls().len();
    let isolated = observe_ready("1", None);
    assert_eq!(
        fake.calls().len(),
        before,
        "isolated release reached the host Docker CLI: {isolated:?}"
    );
    let _normal = observe_ready("0", None);
    let normal_calls = fake.calls().len() - before;
    assert!(
        normal_calls > 0,
        "positive control never reached the instrumented runtime"
    );
    for switch in OPT_INS {
        let start = fake.calls().len();
        let _ = observe_ready("1", Some(switch));
        let added = fake.calls().len() - start;
        assert!(
            added <= normal_calls,
            "{switch} added host calls beyond normal execution"
        );
    }
    support::no_helpers(&s);
}
/// Fault-injection hooks must not remain reachable in the distributed executable.
#[test]
fn release_has_no_countdown_fault_injection_hooks() {
    let mut s = support::sandbox("switches::release_has_no_countdown_fault_injection_hooks");
    let a = support::artifact(&mut s);
    let bytes = std::fs::read(&a.exe).unwrap();
    for hook in [
        "AWMAN_TEST_YOLO_COUNTDOWN_MS",
        "AWMAN_TEST_AGENT_DONE_COUNTDOWN_MS",
    ] {
        assert!(!bytes.windows(hook.len()).any(|window| window == hook.as_bytes()), "release-reachable fault injection hook remains: {hook}; owner layer0/engine must remove it");
    }
}
