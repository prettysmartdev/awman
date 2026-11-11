//! The worked example of the WI 0144 support handoff, kept compiling and
//! passing: sandbox + fake CLI + supervisor + require + wait_for.

use crate::sandbox;
use awman_test_support::capability::{FakeCliExe, LocalGit};
use awman_test_support::fixtures::RepoBuilder;
use awman_test_support::{wait_for, Exit, Ready};
use std::time::Duration;

#[test]
fn a_held_tool_is_observed_released_and_reaped() {
    // 1. Explicit identity and tier; nothing is read from the process env.
    let sandbox = sandbox("example::a_held_tool_is_observed_released_and_reaped");

    // 2. Capabilities the component tier guarantees (absent = BLOCKED, not skipped).
    sandbox.require::<LocalGit>();
    let fake_cli = sandbox.require::<FakeCliExe>();

    // 3. Deterministic fixtures and a scripted fake on the sandbox PATH.
    let repo = RepoBuilder::new(&sandbox, "app")
        .file("README.md", "hi\n")
        .init();
    let docker = sandbox.install_fake(&fake_cli.exe, "docker");
    let started = sandbox.tmp().join("started");
    docker
        .on(&["run"])
        .touch(&started)
        .hold_until_released()
        .stdout("done\n");

    // 4. A supervised child: own process group, sandbox env only, deadline,
    //    returns once the readiness condition holds.
    let mut child = sandbox
        .command("docker")
        .args(["run", "img"])
        .current_dir(repo.path())
        .label("docker run")
        .deadline(Duration::from_secs(10))
        .ready(Ready::FileExists(started))
        .spawn();

    // 5. Event-based wait with a deadline (never sleep).
    let call = wait_for("docker run to be recorded", Duration::from_secs(5), || {
        docker.calls().into_iter().find(|c| c.held)
    });
    assert_eq!(call.argv, ["run", "img"]);
    assert_eq!(call.cwd, repo.path());
    assert_eq!(
        call.env.get("HOME").map(String::as_str),
        sandbox.home().to_str()
    );

    docker.release();
    assert_eq!(child.wait(Duration::from_secs(10)), Exit::Code(0));
    assert_eq!(child.stdout_so_far(), "done\n");

    // 6. Explicit finish: reap, leak check, evidence on failure, cleanup.
    sandbox.finish();
}
