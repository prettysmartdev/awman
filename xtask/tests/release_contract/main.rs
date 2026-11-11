//! WI 0144 §5.3: executable contracts for every row, using private HOME, real
//! scratch Git objects, fake transports, FakeCli tools and controlling PTYs.
#![cfg(unix)]
mod runner;
use awman_test_support::{fakes::FakeCli, Exit, PtyChild, TestCase, TestSandbox};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
const LIMIT: Duration = Duration::from_secs(20);
fn root() -> PathBuf {
    // Runtime value first: a test binary reused from a target/ shared with a
    // checkout at another path still carries that path at compile time.
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    manifest_dir.parent().unwrap().to_path_buf()
}
fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}
struct Fixture {
    s: TestSandbox,
    make: FakeCli,
    awman: FakeCli,
    clip: FakeCli,
    cargo: FakeCli,
}
impl Fixture {
    fn new(id: &'static str) -> Self {
        let mut s = TestSandbox::for_test(TestCase::component(id));
        s.register_exe(
            "fake-cli",
            Path::new(awman_test_support::cargo_bin!("xtask-fake-cli")),
        );
        let git = s
            .command("sh")
            .args(["-c", "command -v git"])
            .run()
            .stdout_text()
            .trim()
            .to_owned();
        let real_xtask = awman_test_support::cargo_bin!("xtask");
        s.register_exe("xtask", Path::new(real_xtask));
        let state = s.root().join("github");
        fs::create_dir_all(&state).unwrap();
        s.env_mut().set("REAL_GIT", &git);
        s.env_mut().set("REAL_XTASK", real_xtask);
        s.env_mut().set("FAKE_STATE", &state);
        let calls_path = s.root().join("calls");
        s.env_mut().set("FAKE_CALLS", calls_path);
        let adapter = s.bin().join("operator-adapter");
        s.env_mut().set("AWMAN_RELEASE_XTASK", adapter);
        s.env_mut().set("RELEASE_RUNNER", "local");
        s.command(&git)
            .args(["init", "-b", "main"])
            .run()
            .assert_success();
        s.command(&git)
            .args(["config", "user.name", "Release Contract"])
            .run()
            .assert_success();
        s.command(&git)
            .args(["config", "user.email", "release@invalid"])
            .run()
            .assert_success();
        write(&s.work().join("Cargo.toml"), "[package]\nname = \"awman\"\nversion = \"1.2.3\"\n[dependencies.dep]\nversion = \"9.8.7\"\n");
        write(
            &s.work().join("Cargo.lock"),
            "version = 4\n[[package]]\nname = \"awman\"\nversion = \"1.2.3\"\n",
        );
        write(
            &s.work().join("docs/releases/v1.2.3.md"),
            "# Release v1.2.3\n\nReal notes.\n",
        );
        s.command(&git).args(["add", "."]).run().assert_success();
        s.command(&git)
            .args(["commit", "-m", "initial"])
            .run()
            .assert_success();
        s.command(&git)
            .args(["update-ref", "refs/remotes/origin/main", "HEAD"])
            .run()
            .assert_success();
        for persona in ["git", "gh", "operator-adapter"] {
            s.install_exe(
                &root().join("xtask/tests/release_contract/fakes.sh"),
                persona,
            );
        }
        let make = s.install_fake(
            Path::new(awman_test_support::cargo_bin!("xtask-fake-cli")),
            "make",
        );
        make.on(&["test"]).stdout("suite output\n");
        let awman = s.install_fake(
            Path::new(awman_test_support::cargo_bin!("xtask-fake-cli")),
            "awman",
        );
        awman.on(&["--version"]).stdout("awman 1.2.3\n");
        awman.on(&["chat"]);
        let clip = s.install_fake(
            Path::new(awman_test_support::cargo_bin!("xtask-fake-cli")),
            "pbcopy",
        );
        clip.on(&[]).read_stdin();
        let mut cargo_fake = None;
        for tool in [
            "cargo",
            "rustc",
            "sudo",
            "caffeinate",
            "xclip",
            "xsel",
            "sleep",
        ] {
            let f = s.install_fake(
                Path::new(awman_test_support::cargo_bin!("xtask-fake-cli")),
                tool,
            );
            f.on(&[]).stdout("fake tool 1.0\n");
            if tool == "cargo" {
                cargo_fake = Some(f.clone());
            }
            if tool == "xclip" || tool == "xsel" {
                f.on(&[]).read_stdin();
            }
        }
        Self {
            s,
            make,
            awman,
            clip,
            cargo: cargo_fake.unwrap(),
        }
    }
    fn state(&self) -> PathBuf {
        self.s.root().join("github")
    }
    fn put(&self, file: &str, data: &str) {
        if file == "state" && data == "none" {
            let _ = fs::remove_file(self.state().join("remote-tag"));
        }
        write(&self.state().join(file), data);
    }
    fn env(&mut self, key: &str, value: &str) {
        self.s.env_mut().set(key, value);
    }
    fn calls(&self) -> String {
        fs::read_to_string(self.s.root().join("calls")).unwrap_or_default()
    }
    fn read(&self, file: &str) -> String {
        fs::read_to_string(self.s.work().join(file)).unwrap()
    }
    fn record(&self) -> serde_json::Value {
        serde_json::from_str(&self.read(".git/awman-release/local-tests.json")).unwrap()
    }
    fn internal(&self, args: &[&str]) -> String {
        let out = self
            .s
            .command(awman_test_support::cargo_bin!("xtask"))
            .args(["release", "run"])
            .args(args)
            .run();
        out.assert_success();
        out.stdout_text().trim().to_owned()
    }
    fn start(&self, script: &Path, args: &[&str]) -> PtyChild {
        self.s
            .pty_command("bash")
            .arg(script)
            .args(args)
            .deadline(LIMIT)
            .spawn()
    }
    fn run(&self) -> String {
        let mut child = self.start(
            &root().join("xtask/src/release/operator/flow.sh"),
            &["v1.2.3"],
        );
        assert_eq!(
            child.wait(LIMIT),
            Exit::Code(0),
            "{}",
            child.output_so_far()
        );
        child.output_so_far()
    }
    fn prompt(&self, needle: &str, answer: &str, exit: i32) -> String {
        let mut child = self.start(
            &root().join("xtask/src/release/operator/flow.sh"),
            &["v1.2.3"],
        );
        child.wait_for_output(needle, LIMIT);
        child.write_all(answer.as_bytes());
        assert_eq!(
            child.wait(LIMIT),
            Exit::Code(exit),
            "{}",
            child.output_so_far()
        );
        child.output_so_far()
    }
    fn git(&self, args: &[&str]) -> String {
        self.s
            .command(self.s.env().get_str("REAL_GIT").unwrap())
            .args(args)
            .run()
            .stdout_text()
            .trim()
            .to_owned()
    }
    fn candidate(&self, state: &str) {
        self.put("state", state);
        self.put("commit", &self.git(&["rev-parse", "HEAD"]));
    }
}
fn contains(text: &str, needle: &str) {
    assert!(text.contains(needle), "missing {needle:?}\n{text}");
}
#[test]
fn row_01_usage_and_runner_mode() {
    let mut f = Fixture::new("release::row_01");
    let mut missing = f.start(&root().join("xtask/src/release/operator/flow.sh"), &[]);
    assert_eq!(missing.wait(LIMIT), Exit::Code(1));
    contains(&missing.output_so_far(), "Usage:");
    let out = f.s.command("make").run();
    assert!(!out.exit.success()); // FakeCli refuses unknown commands.
                                  // Exercise the actual Make recipe, using only fake cargo.
    let out =
        f.s.command("/usr/bin/make")
            .args(["-f", root().join("Makefile").to_str().unwrap(), "release"])
            .run();
    contains(
        &out.stdout_text(),
        "Usage: make release VERSION=vx.y.z [RELEASE_RUNNER=auto|local|none]",
    );
    assert!(!out.exit.success());
    // These recipes exercise the public dispatch names without calling GitHub.
    for (target, subcommand) in [
        ("release-status", "status"),
        ("release-abandon", "abandon"),
        ("apple-silicon-prerelease", "validate"),
    ] {
        f.s.command("/usr/bin/make")
            .args([
                "-f",
                root().join("Makefile").to_str().unwrap(),
                target,
                "VERSION=v1.2.3",
            ])
            .run()
            .assert_success();
        let calls = f.cargo.calls();
        let call = calls.last().unwrap();
        let mut expected = vec!["xtask", "release", subcommand];
        if subcommand != "validate" {
            expected.push("v1.2.3");
        }
        assert_eq!(call.argv, expected);
    }
    f.env("RELEASE_RUNNER", "wat");
    let out = f.prompt("Pre-checks", "", 1);
    contains(&out, "RELEASE_RUNNER must be auto, local or none");
    f.s.env_mut().remove("RELEASE_RUNNER");
    f.put("online", "studio\n");
    contains(&f.run(), "online runner(s) will take");
}
#[test]
fn row_02_version_validation() {
    let f = Fixture::new("release::row_02");
    for v in ["1.2.3", "v1.2", "v1.2.3-rc1", "v1.2.3;no", "v1.x.3"] {
        let mut c = f.start(&root().join("xtask/src/release/operator/flow.sh"), &[v]);
        assert_eq!(c.wait(LIMIT), Exit::Code(1));
        contains(&c.output_so_far(), "Version must match vX.Y.Z");
    }
    f.run();
}
#[test]
fn row_03_steps_and_markers() {
    let f = Fixture::new("release::row_03");
    let out = f.run();
    let plain = strip(&out);
    let steps: Vec<_> = plain.lines().filter(|l| l.starts_with("==>")).collect();
    assert_eq!(
        steps,
        [
            "==> Pre-checks",
            "==> Version in Cargo.toml",
            "==> Release notes",
            "==> Tests",
            "==> Commit",
            "==> Tag",
            "==> Release runner",
            "==> Push",
            "==> Release workflow"
        ]
    );
    contains(&out, "\x1b[0;34m");
    contains(&out, "✓");
}
#[test]
fn row_04_changed_remote_source_is_prepared_again() {
    let changed = Fixture::new("release::row_04_changed_remote_source");
    changed.candidate("prepared");
    write(
        &changed.s.work().join("docs/releases/v1.2.3.md"),
        "updated source notes\n",
    );
    changed.run();
    assert_eq!(changed.make.calls().len(), 1);
    contains(&changed.calls(), "gh <workflow> <run> <candidate.yml>");
}

fn resume_state(state: &str) {
    let f = Fixture::new("release::row_04");
    f.candidate(state);
    let out = f.run();
    contains(&out, "resuming");
    assert!(f.make.calls().is_empty());
    assert!(!f.calls().contains("gh <workflow> <run>"));
}

fn resume_git_failure(stage: &str) {
    let mut f = Fixture::new("release::row_04_failure");
    f.env("FAIL_GIT", stage);
    write(&f.s.work().join("docs/releases/v1.2.3.md"), "new notes\n");
    let mut c = f.start(
        &root().join("xtask/src/release/operator/flow.sh"),
        &["v1.2.3"],
    );
    assert_ne!(c.wait(LIMIT), Exit::Code(0));
    f.env("FAIL_GIT", "");
    if f.git(&["rev-parse", "HEAD"]) != f.git(&["rev-parse", "origin/main"]) {
        f.prompt(
            "Proceed anyway (you may be ahead, or behind)? [y/N]",
            "y\n",
            0,
        );
    } else {
        f.run();
    }
    assert_eq!(f.make.calls().len(), 1);
}

macro_rules! resume_case {
    ($name:ident, $run:ident, $value:literal) => {
        #[test]
        fn $name() {
            $run($value);
        }
    };
}
resume_case!(row_04_resume_prepared, resume_state, "prepared");
resume_case!(row_04_resume_built, resume_state, "candidate-built");
resume_case!(row_04_resume_validated, resume_state, "validated");
resume_case!(row_04_resume_promoted, resume_state, "promoted");
resume_case!(row_04_resume_commit_failure, resume_git_failure, "commit");
resume_case!(row_04_resume_tag_failure, resume_git_failure, "tag");
resume_case!(row_04_resume_push_failure, resume_git_failure, "push");
#[test]
fn row_05_tty_defaults_and_no_tty() {
    let f = Fixture::new("release::row_05");
    f.git(&["checkout", "-b", "topic"]);
    let mut redirected =
        f.s.pty_command("bash")
            .args(["-c", "exec bash \"$1\" v1.2.3 < /dev/null", "release-stdin"])
            .arg(root().join("xtask/src/release/operator/flow.sh"))
            .deadline(LIMIT)
            .spawn();
    redirected.wait_for_output("Proceed with release from 'topic'? [y/N]", LIMIT);
    redirected.write_all(b"n\n");
    assert_eq!(redirected.wait(LIMIT), Exit::Code(1));
    contains(&redirected.output_so_far(), "Aborting.");
    let out = f.prompt("Proceed with release from 'topic'? [y/N]", "\n", 1);
    contains(&out, "Aborting.");
    let out =
        f.s.command("bash")
            .arg(root().join("xtask/src/release/operator/flow.sh"))
            .arg("v1.2.3")
            .run();
    assert!(!out.exit.success());
    contains(&out.stderr_text(), "Cannot open /dev/tty");
    let mut c = f.start(
        &root().join("xtask/src/release/operator/flow.sh"),
        &["v1.2.3"],
    );
    c.wait_for_output("[y/N]", LIMIT);
    c.write_all(&[4]);
    assert_eq!(c.wait(LIMIT), Exit::Code(1));
    contains(&c.output_so_far(), "EOF");
}
#[test]
fn row_06_authentication() {
    let mut f = Fixture::new("release::row_06");
    f.env("AUTH_FAIL", "1");
    let out = f.prompt("Pre-checks", "", 1);
    contains(&out, "Not logged in to GitHub. Run: gh auth login");
    assert!(!f.calls().contains("<fetch>"));
}
#[test]
fn row_07_off_main() {
    let f = Fixture::new("release::row_07");
    f.git(&["checkout", "-b", "topic"]);
    write(
        &f.s.work().join("docs/releases/v1.2.3.md"),
        "topic release notes\n",
    );
    let out = f.prompt("Proceed with release from 'topic'? [y/N]", "y\n", 0);
    contains(&out, "Currently on branch 'topic'");
    assert_eq!(f.record()["answers"][0]["answer"], "y");
    contains(&f.calls(), "git <push> <origin> <HEAD:main>");
    assert_eq!(
        f.git(&["rev-parse", "HEAD"]),
        f.git(&["rev-parse", "origin/main"])
    );
}
#[test]
fn row_08_divergence() {
    let f = Fixture::new("release::row_08");
    f.git(&["commit", "--allow-empty", "-m", "ahead"]);
    let out = f.prompt(
        "Proceed anyway (you may be ahead, or behind)? [y/N]",
        "\n",
        1,
    );
    contains(&out, "not in sync");
    contains(&f.calls(), "git <fetch> <origin> <main> <--quiet>");
    f.prompt(
        "Proceed anyway (you may be ahead, or behind)? [y/N]",
        "y\n",
        0,
    );
}
#[test]
fn row_09_dirty_files_and_evidence_mismatch() {
    let f = Fixture::new("release::row_09");
    write(&f.s.work().join("unrelated"), "influences tests");
    f.git(&["add", "unrelated"]);
    write(
        &f.s.work().join("docs/releases/v1.2.3.md"),
        "changed notes\n",
    );
    f.prompt(
        "Proceed anyway? (consider committing or stashing these first) [y/N]",
        "\n",
        1,
    );
    f.prompt(
        "Proceed anyway? (consider committing or stashing these first) [y/N]",
        "y\n",
        0,
    );
    assert_eq!(f.git(&["ls-tree", "--name-only", "HEAD", "unrelated"]), "");
    assert_eq!(f.git(&["diff", "--cached", "--name-only"]), "unrelated");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(f.state().join("operator.json")).unwrap())
            .unwrap();
    assert_eq!(payload["local_tests"], "not run");
    assert_eq!(payload["tested_checkout_outcome"], "passed");
}
#[test]
fn row_10_remote_tag_and_published_resume() {
    let f = Fixture::new("release::row_10");
    f.put("remote-tag", "");
    f.prompt(
        "its Release workflow run will be checked, and re-run if it failed) [Y/n]",
        "\n",
        0,
    );
    let f = Fixture::new("release::row_10_verified");
    f.candidate("verified");
    f.put("remote-tag", "");
    let out = f.prompt("[Y/n]", "\n", 0);
    contains(&out, "Release v1.2.3 is already published");
    assert!(!f.calls().contains("<push>"));
}
#[test]
fn row_11_runner_selection() {
    for mode in ["auto", "none", "local"] {
        let mut f = Fixture::new("release::row_11");
        f.env("RELEASE_RUNNER", mode);
        f.run();
        if mode == "none" {
            assert!(!f.calls().contains("<runner> <up>"));
        }
    }
    let mut f = Fixture::new("release::row_11_unavailable");
    f.env("HARDWARE", "0");
    f.env("RELEASE_RUNNER", "auto");
    f.prompt("Continue? (the Apple Silicon job will wait for a runner, and nothing publishes until it passes) [y/N]","\n",1);
    f.env("RELEASE_RUNNER", "local");
    contains(&f.prompt("Pre-checks", "", 1), "This host cannot run");
    for key in ["PROVISION_FAIL", "FULL_CHECK_FAIL"] {
        let mut f = Fixture::new("release::row_11_provision");
        f.env(key, "1");
        assert!(f.prompt("Pre-checks", "", 1).contains("Error:"));
        assert!(!f.calls().contains("<push>"));
    }
}
#[test]
fn row_12_portable_version_bump() {
    let f = Fixture::new("release::row_12");
    let p = f.s.work().join("Cargo.toml");
    write(&p, &f.read("Cargo.toml").replacen("1.2.3", "1.0.0", 1));
    let out = f.run();
    contains(
        &out,
        "Installed awman reports version 1.2.3 but Cargo.toml says 1.0.0",
    );
    contains(&out, "Bumped Cargo.toml: 1.0.0 → 1.2.3");
    contains(&f.read("Cargo.toml"), "version = \"9.8.7\"");
}
#[test]
fn row_13_notes_template_and_chat() {
    for answer in ["n\n", "\n"] {
        let f = Fixture::new("release::row_13");
        fs::remove_file(f.s.work().join("docs/releases/v1.2.3.md")).unwrap();
        let out = f.prompt(
            "Launch 'awman chat' to write release notes now? [Y/n]",
            answer,
            1,
        );
        contains(
            &f.read("docs/releases/v1.2.3.md"),
            "_Write release notes here._",
        );
        contains(&out, "re-run");
        assert_eq!(f.awman.calls().len(), if answer == "\n" { 1 } else { 0 });
    }
    let f = Fixture::new("release::row_13_edit");
    fs::remove_file(f.s.work().join("docs/releases/v1.2.3.md")).unwrap();
    let mut c = f.start(
        &root().join("xtask/src/release/operator/flow.sh"),
        &["v1.2.3"],
    );
    c.wait_for_output("[Y/n]", LIMIT);
    write(
        &f.s.work().join("docs/releases/v1.2.3.md"),
        "Written in chat\n",
    );
    c.write_all(b"\n");
    assert_eq!(c.wait(LIMIT), Exit::Code(0));
}
#[test]
fn row_14_local_pass_hash_and_reuse() {
    source_change_during_tests_is_not_a_pass();
    let tools = Fixture::new("release::row_14_tool_identity");
    tools.internal(&["--internal-local", "begin"]);
    tools.internal(&["--internal-local", "passed"]);
    tools.cargo.on(&["--version"]).stdout("changed cargo 2.0\n");
    tools.internal(&["--internal-commit"]);
    assert_eq!(tools.internal(&["--internal-local", "query"]), "not run");
    let commit = tools.git(&["rev-parse", "HEAD"]);
    let payload: serde_json::Value =
        serde_json::from_str(&tools.internal(&["--internal-payload", &commit, "o/r", "v1.2.3"]))
            .unwrap();
    assert_eq!(payload["local_tests"], "not run");
    assert_eq!(payload["tested_checkout_outcome"], "passed");
    let journal = Fixture::new("release::row_14_journal_commit");
    let old_commit = journal.git(&["rev-parse", "HEAD"]);
    write(
        &journal.s.work().join("docs/releases/v1.2.3.md"),
        "new notes\n",
    );
    journal.git(&["add", "docs/releases/v1.2.3.md"]);
    journal.git(&["commit", "-m", "updated candidate inputs"]);
    journal.internal(&["--internal-local", "begin"]);
    journal.internal(&["--internal-local", "passed"]);
    let payload: serde_json::Value = serde_json::from_str(&journal.internal(&[
        "--internal-journal-payload",
        &old_commit,
        "o/r",
        "v1.2.3",
    ]))
    .unwrap();
    assert_eq!(payload["local_tests"], "not run");
    assert_eq!(payload["tested_checkout_outcome"], "passed");
    assert_eq!(payload["commit"], old_commit);
    assert_eq!(journal.internal(&["--internal-local", "query"]), "passed");
    let missing_commit = "0".repeat(40);
    let payload: serde_json::Value = serde_json::from_str(&journal.internal(&[
        "--internal-journal-payload",
        &missing_commit,
        "o/r",
        "v1.2.3",
    ]))
    .unwrap();
    assert_eq!(payload["local_tests"], "not run");
    assert_eq!(journal.internal(&["--internal-local", "query"]), "passed");
    let f = Fixture::new("release::row_14");
    write(
        &f.s.work().join(".git/.release-tests-passed-v1.2.3"),
        "obsolete",
    );
    f.run();
    assert_eq!(f.record()["local_tests"], "passed");
    f.put("state", "none");
    contains(&f.run(), "Tests already passed for this source tree");
    assert_eq!(f.make.calls().len(), 1);
    write(
        &f.s.work().join("Cargo.lock"),
        &(f.read("Cargo.lock") + "\n# changed\n"),
    );
    f.put("state", "none");
    f.git(&["tag", "-d", "v1.2.3"]);
    f.run();
    assert_eq!(f.make.calls().len(), 2);
    assert!(!f
        .s
        .work()
        .join(".git/.release-tests-passed-v1.2.3")
        .exists());
}
#[test]
fn row_15_fail_skip_clipboard_and_remember_choice() {
    let f = Fixture::new("release::row_15");
    f.make.on(&["test"]).stdout("full failure\n").exit(1);
    let out = f.prompt(
        "Skip tests and continue the release anyway? [y/N]",
        "y\n",
        0,
    );
    contains(&out, "copied to clipboard");
    assert_eq!(f.clip.calls().len(), 1);
    assert_eq!(f.clip.calls()[0].stdin, b"full failure\n");
    assert_eq!(f.record()["local_tests"], "skipped by operator");
    f.put("state", "none");
    f.run();
    assert_eq!(f.make.calls().len(), 1);
}
#[test]
fn row_15_decline_fix() {
    failure_fix_prompt("\nn\n");
}
#[test]
fn row_15_accept_fix_by_default() {
    failure_fix_prompt("\n\n");
}
fn failure_fix_prompt(answer: &str) {
    let f = Fixture::new("release::row_15_fix");
    f.make.on(&["test"]).exit(1);
    let out = f.prompt(
        "Skip tests and continue the release anyway? [y/N]",
        answer,
        1,
    );
    contains(&out, "Launch 'awman chat' to fix the failing tests? [Y/n]");
    contains(&out.to_lowercase(), "re-run");
}
#[test]
fn row_16_commit_only_release_paths() {
    let f = Fixture::new("release::row_16");
    write(
        &f.s.work().join("docs/releases/v1.2.3.md"),
        "Updated notes\n",
    );
    f.run();
    assert_eq!(
        f.git(&["log", "-1", "--format=%s"]),
        "Add release notes for v1.2.3"
    );
    contains(
        &f.calls(),
        "git <add> <docs/releases/v1.2.3.md> <Cargo.toml> <Cargo.lock>",
    );
}
#[test]
fn row_17_local_tag_create_reuse_and_mismatch() {
    let f = Fixture::new("release::row_17");
    f.run();
    assert_eq!(
        f.git(&["rev-parse", "v1.2.3"]),
        f.git(&["rev-parse", "HEAD"])
    );
    f.put("state", "none");
    contains(&f.run(), "Tag v1.2.3 already exists locally");
    let mut f = Fixture::new("release::row_17_mismatch");
    f.git(&["tag", "v1.2.3"]);
    f.git(&["commit", "--allow-empty", "-m", "new"]);
    f.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    f.env("RELEASE_RUNNER", "none");
    let out = f.prompt("Pre-checks", "", 1);
    contains(&out, "points to a different commit");
    assert!(!f.calls().contains("<workflow> <run>"));
}
#[test]
fn row_18_queued_cancel_default_and_decline() {
    let own = Fixture::new("release::row_18_own_candidate");
    own.candidate("prepared");
    own.put("queued", "99\n");
    own.run();
    assert!(!own.calls().contains("gh <run> <cancel>"));
    let ci = Fixture::new("release::row_18_ci_labels");
    ci.put("queued", "89\n");
    ci.run();
    assert!(!ci.calls().contains("gh <run> <cancel>"));
    let progress = Fixture::new("release::row_18_in_progress");
    progress.put("in-progress", "88\n");
    progress.prompt("Cancel them? [Y/n]", "\n", 0);
    assert!(progress.state().join("cancelled").exists());
    let duplicate = Fixture::new("release::row_18_duplicate");
    duplicate.put("queued", "88\n");
    duplicate.put("in-progress", "88\n");
    let out = duplicate.prompt("Cancel them? [Y/n]", "\n", 0);
    contains(&out, "1 Tests run(s) waiting");
    assert_eq!(duplicate.calls().matches("gh <run> <cancel>").count(), 1);
    for answer in ["\n", "n\n"] {
        let f = Fixture::new("release::row_18");
        f.put("queued", "");
        f.prompt("Cancel them? [Y/n]", answer, 0);
        assert_eq!(f.state().join("cancelled").exists(), answer == "\n");
        contains(&f.calls(), "awman-release");
    }
}
#[test]
fn row_20_push_then_candidate_never_tag() {
    let f = Fixture::new("release::row_20");
    write(&f.s.work().join("docs/releases/v1.2.3.md"), "new notes\n");
    f.run();
    let calls = f.calls();
    let push = calls.find("git <push> <origin> <main>").unwrap();
    let dispatch = calls.find("gh <workflow> <run> <candidate.yml>").unwrap();
    assert!(push < dispatch);
    assert!(!calls.contains("git <push> <origin> <v1.2.3>"));
    contains(&calls, "<--ref> <main>");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(f.state().join("operator.json")).unwrap())
            .unwrap();
    assert_eq!(payload["request_id"].as_str().unwrap().len(), 64);
}
#[test]
fn row_21_unmanaged_follow_line() {
    let mut f = Fixture::new("release::row_21");
    f.env("RELEASE_RUNNER", "none");
    let out = f.run();
    contains(
        &out,
        "Candidate started; the Release workflow will publish v1.2.3 after the build gates pass",
    );
    contains(&out, "gh run watch $(gh run list --workflow candidate.yml");
    contains(&out, "Release v1.2.3 tagged!");
    assert!(!f.calls().contains("gh <run> <watch>"));
}
#[test]
fn row_22_watch_rerun_and_verified_success() {
    let mut missing = Fixture::new("release::row_22_missing");
    missing.env("FORCE_NO_STATE", "1");
    let out = missing.prompt("Pre-checks", "", 1);
    contains(
        &out,
        "No Release workflow run appeared for v1.2.3 within 5 minutes.",
    );
    assert_eq!(
        fs::read_dir(missing.s.bin().join(".fake/sleep/calls"))
            .unwrap()
            .count(),
        60
    );
    assert!(!missing.state().join("runner").exists());
    let decline = Fixture::new("release::row_22_decline");
    decline.candidate("validated");
    decline.put("conclusion", "failure");
    decline.prompt("Re-run its failed jobs on this Mac? [Y/n]", "n\n", 1);
    assert!(!decline.calls().contains("gh <run> <rerun>"));
    let unverified = Fixture::new("release::row_22_unverified");
    unverified.candidate("validated");
    unverified.put("conclusion", "success");
    let out = unverified.prompt("Pre-checks", "", 1);
    contains(&out, "already succeeded");
    contains(&out, "has not verified the published release");
    let f = Fixture::new("release::row_22");
    f.candidate("validated");
    f.put("conclusion", "failure");
    f.prompt("Re-run its failed jobs on this Mac? [Y/n]", "\n", 0);
    contains(
        &f.calls(),
        "gh <run> <rerun> <99> <--repo> <o/r> <--failed>",
    );
    contains(
        &fs::read_to_string(f.state().join("journals")).unwrap(),
        "Re-run its failed jobs on this Mac?",
    );
    contains(&f.calls(), "<--exit-status> <--interval> <60>");
    let mut f = Fixture::new("release::row_22_fail");
    f.env("WATCH_FAIL", "1");
    let out = f.prompt("Pre-checks", "", 1);
    contains(&out, "gh run view 99 --repo o/r --log-failed");
    contains(&out, "Re-run this script to retry the failed jobs.");
    assert!(!f.state().join("runner").exists());
}
#[test]
fn row_23_teardown_success_failure_and_double_interrupt() {
    for sig in [
        nix::sys::signal::Signal::SIGINT,
        nix::sys::signal::Signal::SIGTERM,
    ] {
        let mut f = Fixture::new("release::row_23");
        f.env("WATCH_HOLD", "1");
        f.env("DOWN_HOLD", "1");
        let mut c = f.start(
            &root().join("xtask/src/release/operator/flow.sh"),
            &["v1.2.3"],
        );
        c.wait_for_output("WATCH_READY", LIMIT);
        nix::sys::signal::killpg(nix::unistd::Pid::from_raw(c.pid() as i32), sig).unwrap();
        c.wait_for_output("TEARDOWN_READY", LIMIT);
        nix::sys::signal::killpg(nix::unistd::Pid::from_raw(c.pid() as i32), sig).unwrap();
        f.put("release-down", "");
        assert_eq!(c.wait(LIMIT), Exit::Code(130), "{}", c.output_so_far());
        assert!(!f.state().join("runner").exists());
    }
}
#[test]
fn row_24_end_state_matches_operator_contract() {
    let f = Fixture::new("release::row_24");
    let out = f.run();
    contains(&out, "Release v1.2.3 published!");
    assert_eq!(
        f.git(&["rev-parse", "HEAD"]),
        f.git(&["rev-parse", "origin/main"])
    );
    assert_eq!(
        f.git(&["rev-parse", "v1.2.3"]),
        f.git(&["rev-parse", "HEAD"])
    );
    assert!(f.state().join("remote-tag").exists());
    assert_eq!(
        fs::read_to_string(f.state().join("remote-tag"))
            .unwrap()
            .trim(),
        f.git(&["rev-parse", "HEAD"])
    );
    assert_eq!(
        fs::read_to_string(f.state().join("body")).unwrap(),
        f.read("docs/releases/v1.2.3.md")
    );
    let mut assets: Vec<_> = fs::read_dir(f.state().join("assets"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assets.sort();
    assert_eq!(
        assets,
        [
            "SHA256SUMS",
            "awman-linux-amd64",
            "awman-linux-arm64",
            "awman-macos-arm64",
            "awman-windows-amd64.exe",
        ]
    );
    let checksums = fs::read_to_string(f.state().join("assets/SHA256SUMS")).unwrap();
    let expected = assets[1..]
        .iter()
        .map(|asset| {
            let bytes = fs::read(f.state().join("assets").join(asset)).unwrap();
            use sha2::{Digest, Sha256};
            let digest: String = Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("{digest}  {asset}\n")
        })
        .collect::<String>();
    assert_eq!(checksums, expected);
    assert!(!f.state().join("runner").exists());
}
fn strip(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

// Each interrupted phase is its own deadline and failure report.
fn resume_after_interrupt_at_step(phase: usize) {
    let mut f = Fixture::new("release::row_04_interrupt");
    let mut held_tool = None;
    match phase {
        0 => f.env("HOLD_AT", "gh/auth"),
        1 => {
            write(
                &f.s.work().join("Cargo.toml"),
                &f.read("Cargo.toml").replacen("1.2.3", "1.0.0", 1),
            );
            let cargo = f.cargo.clone();
            cargo.on(&[]).stdout("fake tool 1.0\n");
            cargo.on(&["update"]).hold_until_released();
            held_tool = Some(cargo);
        }
        2 => {
            write(
                &f.s.work().join("docs/releases/v1.2.3.md"),
                "_Write release notes here._\n",
            );
            f.awman.on(&["chat"]).hold_until_released();
            held_tool = Some(f.awman.clone());
        }
        3 => {
            f.make.on(&["test"]).hold_until_released();
            held_tool = Some(f.make.clone());
        }
        4 => {
            write(&f.s.work().join("docs/releases/v1.2.3.md"), "new notes\n");
            f.env("HOLD_AT", "git/commit");
        }
        5 => f.env("HOLD_AT", "git/tag"),
        6 => f.env("HOLD_AT", "runner/up"),
        7 => {
            write(&f.s.work().join("docs/releases/v1.2.3.md"), "new notes\n");
            f.env("HOLD_AT", "git/push");
        }
        8 => f.env("WATCH_HOLD", "1"),
        _ => unreachable!(),
    }
    let mut child = f.start(
        &root().join("xtask/src/release/operator/flow.sh"),
        &["v1.2.3"],
    );
    if phase == 2 {
        child.wait_for_output("[Y/n]", LIMIT);
        child.write_all(b"\n");
    }
    if let Some(fake) = &held_tool {
        fake.wait_until_held(LIMIT);
    } else {
        if phase == 8 {
            child.wait_for_output("WATCH_READY", LIMIT);
        } else {
            awman_test_support::wait_for_with(
                "held step",
                LIMIT,
                || f.state().join("held").exists().then_some(()),
                || child.output_so_far(),
            );
        }
    }
    nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(child.pid() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    assert_eq!(
        child.wait(LIMIT),
        Exit::Code(130),
        "step {phase}: {}",
        child.output_so_far()
    );
    assert!(!f.state().join("runner").exists());
    f.env("HOLD_AT", "");
    f.env("WATCH_HOLD", "0");
    if let Some(fake) = held_tool {
        fake.on(&[]).stdout("fake tool 1.0\n");
        fake.on(&["update"]);
        fake.on(&["chat"]);
        fake.on(&["test"]).stdout("suite output\n");
    }
    if phase == 2 {
        write(
            &f.s.work().join("docs/releases/v1.2.3.md"),
            "written notes\n",
        );
    }
    if f.git(&["rev-parse", "HEAD"]) != f.git(&["rev-parse", "origin/main"]) {
        f.prompt(
            "Proceed anyway (you may be ahead, or behind)? [y/N]",
            "y\n",
            0,
        );
    } else {
        f.run();
    }
    if phase >= 4 {
        assert_eq!(
            f.make.calls().len(),
            1,
            "step {phase} repeated finished local tests"
        );
    }
}

// Each independent operator flow gets the normal component deadline. Bundling
// all seven row-15 flows in one test exceeded it on slower macOS runners.
#[test]
fn row_15_clipboard_xclip() {
    clipboard_fallback("xclip");
}
#[test]
fn row_15_clipboard_xsel() {
    clipboard_fallback("xsel");
}
#[test]
fn row_15_clipboard_unavailable() {
    clipboard_fallback("none");
}
fn clipboard_fallback(persona: &str) {
    let mut f = Fixture::new("release::row_15_clipboard");
    // A private tool PATH prevents macOS from falling through to the real
    // /usr/bin/pbcopy when a fake clipboard executable is deliberately absent.
    for tool in [
        "bash", "awk", "sed", "grep", "tee", "mktemp", "rm", "mkdir", "head", "seq", "basename",
        "shasum", "mv", "cp", "cat", "wc", "tr", "touch",
    ] {
        let path = f.s.env().resolve(std::ffi::OsStr::new(tool)).unwrap();
        std::os::unix::fs::symlink(path, f.s.bin().join(tool)).unwrap();
    }
    fs::remove_file(f.s.bin().join("pbcopy")).unwrap();
    if persona != "xclip" {
        fs::remove_file(f.s.bin().join("xclip")).unwrap();
    }
    if persona == "none" {
        fs::remove_file(f.s.bin().join("xsel")).unwrap();
    }
    let bin = f.s.bin();
    f.s.env_mut().set("PATH", bin);
    f.make
        .on(&["test"])
        .stdout("entire failure output\n")
        .exit(1);
    let out = f.prompt(
        "Skip tests and continue the release anyway? [y/N]",
        "y\n",
        0,
    );
    if persona == "none" {
        contains(
            &out,
            "Could not copy to clipboard (install pbcopy, xclip, or xsel).",
        );
    } else {
        let calls = f.s.bin().join(".fake").join(persona).join("calls");
        let call = fs::read_dir(calls).unwrap().next().unwrap().unwrap().path();
        assert_eq!(
            fs::read(call.join("stdin")).unwrap(),
            b"entire failure output\n"
        );
    }
}

fn source_change_during_tests_is_not_a_pass() {
    let f = Fixture::new("release::row_14_changed_during_tests");
    f.make.on(&["test"]).hold_until_released();
    let mut child = f.start(
        &root().join("xtask/src/release/operator/flow.sh"),
        &["v1.2.3"],
    );
    f.make.wait_until_held(LIMIT);
    write(
        &f.s.work().join("Cargo.lock"),
        &(f.read("Cargo.lock") + "\n# changed during tests\n"),
    );
    f.make.release();
    assert_eq!(child.wait(LIMIT), Exit::Code(1));
    contains(
        &child.output_so_far(),
        "source changed while local tests were running",
    );
    assert_eq!(f.record()["local_tests"], "not run");
    assert!(!f.calls().contains("gh <workflow> <run>"));
}

#[test]
fn resume_after_auth_interrupt() {
    resume_after_interrupt_at_step(0);
}
#[test]
fn resume_after_version_interrupt() {
    resume_after_interrupt_at_step(1);
}
#[test]
fn resume_after_notes_interrupt() {
    resume_after_interrupt_at_step(2);
}
#[test]
fn resume_after_tests_interrupt() {
    resume_after_interrupt_at_step(3);
}
#[test]
fn resume_after_commit_interrupt() {
    resume_after_interrupt_at_step(4);
}
#[test]
fn resume_after_tag_interrupt() {
    resume_after_interrupt_at_step(5);
}
#[test]
fn resume_after_runner_interrupt() {
    resume_after_interrupt_at_step(6);
}
#[test]
fn resume_after_push_interrupt() {
    resume_after_interrupt_at_step(7);
}
#[test]
fn resume_after_watch_interrupt() {
    resume_after_interrupt_at_step(8);
}
