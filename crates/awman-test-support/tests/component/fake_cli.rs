//! FakeCli protocol v1.

use crate::{fake_cli_exe, sandbox};
use awman_test_support::Exit;
use std::time::Duration;

#[test]
fn without_a_scenario_the_fake_exits_127() {
    let sandbox = sandbox("fake_cli::without_a_scenario_the_fake_exits_127");
    let fake = sandbox.install_fake(fake_cli_exe(), "docker");
    std::fs::remove_file(fake.dir().join("rules")).unwrap();
    let out = sandbox.command("docker").arg("ps").run();
    assert_eq!(out.exit, Exit::Code(127));
    assert!(out
        .stderr_text()
        .contains("fake docker: no scenario installed"));
}

#[test]
fn unmatched_invocations_exit_127_and_are_recorded() {
    let sandbox = sandbox("fake_cli::unmatched_invocations_exit_127_and_are_recorded");
    let fake = sandbox.install_fake(fake_cli_exe(), "gh");
    fake.on(&["auth", "status"]).stdout("ok\n");
    let out = sandbox.command("gh").args(["pr", "list"]).run();
    assert_eq!(out.exit, Exit::Code(127));
    assert!(out.stderr_text().contains("no rule matches: pr list"));
    assert_eq!(fake.calls()[0].argv, vec!["pr", "list"]);
    assert_eq!(fake.calls()[0].exit, Some(127));
}

#[test]
fn the_longest_matching_prefix_wins() {
    let sandbox = sandbox("fake_cli::the_longest_matching_prefix_wins");
    let docker = sandbox.install_fake(fake_cli_exe(), "docker");
    docker.on(&[]).stdout("generic\n");
    docker
        .on(&["image", "inspect"])
        .stderr("No such image\n")
        .exit(1);
    docker.on(&["image"]).stdout("image\n");
    let generic = sandbox.command("docker").arg("ps").run();
    assert_eq!(generic.stdout_text(), "generic\n");
    let inspect = sandbox
        .command("docker")
        .args(["image", "inspect", "x"])
        .run();
    assert_eq!(inspect.exit, Exit::Code(1));
    assert_eq!(inspect.stderr_text(), "No such image\n");
    assert_eq!(
        sandbox
            .command("docker")
            .args(["image", "ls"])
            .run()
            .stdout_text(),
        "image\n"
    );
}

#[test]
fn values_with_newlines_tabs_and_percent_round_trip() {
    let sandbox = sandbox("fake_cli::values_with_newlines_tabs_and_percent_round_trip");
    let fake = sandbox.install_fake(fake_cli_exe(), "echoer");
    fake.on(&["a\tb"]).stdout("line1\nline2\t100%\n");
    let out = sandbox.command("echoer").arg("a\tb").arg("x\ny").run();
    assert_eq!(out.stdout_text(), "line1\nline2\t100%\n");
    assert_eq!(fake.calls()[0].argv, vec!["a\tb", "x\ny"]);
}

#[test]
fn hold_blocks_until_released() {
    let sandbox = sandbox("fake_cli::hold_blocks_until_released");
    let fake = sandbox.install_fake(fake_cli_exe(), "slow");
    fake.on(&[]).hold_until_released().stdout("after\n");
    let mut child = sandbox.command("slow").spawn();
    let held = fake.wait_until_held(Duration::from_secs(10));
    assert!(held.exit.is_none());
    assert!(child.try_exit().is_none());
    fake.release();
    assert_eq!(child.wait(Duration::from_secs(10)), Exit::Code(0));
    assert_eq!(child.stdout_so_far(), "after\n");
    assert_eq!(
        fake.wait_for_calls(1, Duration::from_secs(5))[0].exit,
        Some(0)
    );
}

#[test]
fn touch_creates_files() {
    let sandbox = sandbox("fake_cli::touch_creates_files");
    let fake = sandbox.install_fake(fake_cli_exe(), "maker");
    let marker = sandbox.tmp().join("made/it");
    fake.on(&[]).touch(&marker);
    sandbox.command("maker").run().assert_success();
    assert!(marker.exists());
}

#[test]
fn the_persona_comes_from_the_installed_name_not_the_environment() {
    let sandbox =
        sandbox("fake_cli::the_persona_comes_from_the_installed_name_not_the_environment");
    let a = sandbox.install_fake(fake_cli_exe(), "alpha");
    let b = sandbox.install_fake(fake_cli_exe(), "beta");
    a.on(&[]).stdout("alpha\n");
    b.on(&[]).stdout("beta\n");
    // A completely empty environment still selects the right scenario.
    let mut cmd = sandbox.command(b.exe());
    for (k, _) in sandbox.env().iter() {
        cmd = cmd.env_remove(k);
    }
    assert_eq!(cmd.run().stdout_text(), "beta\n");
    assert_eq!(sandbox.command("alpha").run().stdout_text(), "alpha\n");
    assert!(b.calls()[0].env.is_empty());
}

#[test]
fn the_protocol_version_is_checked() {
    let sandbox = sandbox("fake_cli::the_protocol_version_is_checked");
    let out = sandbox.command(fake_cli_exe()).arg("--fake-protocol").run();
    assert_eq!(
        out.stdout_text().trim(),
        awman_test_support::fakes::FAKE_PROTOCOL.to_string()
    );
}
