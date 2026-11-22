//! Runner ownership and provenance contracts; no host registration or global files.
use super::*;
struct Runner {
    s: TestSandbox,
    gh: FakeCli,
    cargo: FakeCli,
    home: PathBuf,
}
impl Runner {
    fn new(id: &'static str) -> Self {
        let mut s = TestSandbox::for_test(TestCase::component(id));
        let home = s.root().join("runner");
        s.env_mut().set("AWMAN_RELEASE_RUNNER_HOME", &home);
        let work = s.root().join("jobs");
        s.env_mut().set("AWMAN_RELEASE_RUNNER_WORK", work);
        s.env_mut().set("AWMAN_RELEASE_RUNNER_GROUP_ID", "7");
        let exe = Path::new(awman_test_support::cargo_bin!("xtask-fake-cli"));
        let uname = s.install_fake(exe, "uname");
        uname.on(&["-s"]).stdout("Darwin\n");
        uname.on(&["-m"]).stdout("arm64\n");
        let sysctl = s.install_fake(exe, "sysctl");
        sysctl.on(&["-n", "kern.hv_support"]).stdout("1\n");
        let gh = s.install_fake(exe, "gh");
        let cargo = s.install_fake(exe, "cargo");
        Self { s, gh, cargo, home }
    }
    fn run(&self, args: &[&str]) -> awman_test_support::Output {
        self.s
            .command("bash")
            .arg(root().join("xtask/src/release/runner/runner.sh"))
            .args(args)
            .run()
    }
    fn group(&self) {
        self.gh
            .on(&["api", "orgs/example/actions/runner-groups/7"])
            .stdout("release-trusted\n");
    }
    fn archive(&self, valid: bool, config: &str) {
        let dir = self.s.work().join("archive");
        write(&dir.join("config.sh"), config);
        write(&dir.join("run.sh"), "#!/bin/bash\nexit 3\n");
        self.s
            .command("chmod")
            .args(["+x"])
            .arg(dir.join("config.sh"))
            .arg(dir.join("run.sh"))
            .run()
            .assert_success();
        let cache = self.home.join("cache");
        fs::create_dir_all(&cache).unwrap();
        let asset = "actions-runner-osx-arm64-9.9.9.tar.gz";
        self.s
            .command("tar")
            .arg("-czf")
            .arg(cache.join(asset))
            .arg("-C")
            .arg(dir)
            .arg(".")
            .run()
            .assert_success();
        use sha2::{Digest, Sha256};
        let hash = if valid {
            Sha256::digest(fs::read(cache.join(asset)).unwrap())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        } else {
            "0".repeat(64)
        };
        self.gh
            .on(&["api", "repos/actions/runner/releases/latest"])
            .stdout("v9.9.9\n");
        self.gh
            .on(&["api", "repos/actions/runner/releases/tags/v9.9.9"])
            .stdout(&format!("- {asset} {hash}\n"));
    }
}
#[test]
fn provisioning_is_manifest_verified_and_failure_stops_before_registration() {
    let r = Runner::new("release::runner::provision");
    r.cargo.on(&["xtask", "fixtures", "fetch"]);
    r.cargo.on(&["xtask", "fixtures", "verify"]);
    r.run(&["provision"]).assert_success();
    let calls = r.cargo.calls();
    assert_eq!(calls.len(), 2);
    for call in calls {
        assert!(call.argv.ends_with(&[
            "--store".into(),
            r.home.join("fixtures").display().to_string()
        ]));
    }
    r.cargo
        .on(&["xtask", "fixtures", "fetch"])
        .exit(1)
        .stderr("digest mismatch\n");
    let result = r.run(&["provision"]);
    assert!(!result.exit.success());
    assert!(result
        .stderr_text()
        .contains("could not fetch pinned fixtures"));
    assert!(r.gh.calls().is_empty());
}
#[test]
fn missing_group_or_bad_archive_cannot_obtain_a_registration_token() {
    let r = Runner::new("release::runner::trust");
    r.gh.on(&["api", "orgs/example/actions/runner-groups/7"])
        .stdout("");
    assert!(!r.run(&["up", "example/repo", "v1.2.3"]).exit.success());
    r.group();
    r.archive(false, "#!/bin/bash\nexit 0\n");
    let out = r.run(&["up", "example/repo", "v1.2.3"]);
    assert!(!out.exit.success());
    assert!(
        out.stderr_text().contains("checksum mismatch"),
        "{}",
        out.stderr_text()
    );
    assert!(!r
        .gh
        .calls()
        .iter()
        .any(|c| c.argv.iter().any(|a| a.contains("registration-token"))));
    assert!(!r.home.join("state").exists());
}
#[test]
fn failed_registration_releases_owned_state_and_keeps_existing_work() {
    let r = Runner::new("release::runner::rollback");
    r.group();
    r.archive(true, "#!/bin/bash\nprintf '%s\\n' \"$@\" >> \"$AWMAN_RELEASE_RUNNER_HOME/argv\"\n[ -n \"$ACTIONS_RUNNER_INPUT_TOKEN\" ] || exit 8\n[ \"$1\" = remove ]\n");
    r.gh.on(&[
        "api",
        "-X",
        "POST",
        "orgs/example/actions/runners/registration-token",
    ])
    .stdout("private-register\n");
    r.gh.on(&[
        "api",
        "-X",
        "POST",
        "orgs/example/actions/runners/remove-token",
    ])
    .stdout("private-remove\n");
    let work = r.s.root().join("jobs");
    for existing in [false, true] {
        if existing {
            write(&work.join("keep"), "owned by operator");
        }
        assert!(!r.run(&["up", "example/repo", "v1.2.3"]).exit.success());
        assert!(!r.home.join("state").exists());
        assert!(!r.home.join("run").exists());
        assert_eq!(work.exists(), existing);
        if existing {
            assert_eq!(
                fs::read_to_string(work.join("keep")).unwrap(),
                "owned by operator"
            );
        }
        r.run(&["down"]).assert_success();
    }
    let argv = fs::read_to_string(r.home.join("argv")).unwrap();
    assert!(argv.contains("--runnergroup\nrelease-trusted"));
    assert!(!argv.contains("private-") && !argv.contains("--token"));
}
#[test]
fn deregistration_failure_retains_repair_state() {
    let r = Runner::new("release::runner::repair");
    write(
        &r.home.join("state"),
        "repo=example/repo\nname=test-runner\nregistered=true\nowned_work=false\n",
    );
    r.gh.on(&["api", "-X", "POST"]).exit(1);
    r.gh.on(&["api", "repos/example/repo/actions/runners"])
        .stdout("42\n");
    r.gh.on(&["api", "-X", "DELETE"]).exit(1);
    let out = r.run(&["down"]);
    assert!(!out.exit.success());
    assert!(out.stderr_text().contains("state retained"));
    assert!(r.home.join("state").exists());
    r.gh.on(&["api", "-X", "DELETE"]).exit(0);
    r.run(&["down"]).assert_success();
    assert!(!r.home.join("state").exists());
}
