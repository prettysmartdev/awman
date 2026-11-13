//! Boundary conformance: the shared runtime conformance suite
//! (`awman::engine::conformance`, owned by the Layer 1 slice) run against
//! the real implementations. The same assertions run against the verified
//! fakes in the `component` tier, so a fake that passes there and the real
//! thing passing here agree on everything the suite checks (manifest
//! `[fakes.*]` pairs).
//!
//! The registry and git transport suites run in their own domains
//! (`registry::`, `git::`) because they need different capabilities.

mod argv_watch;
mod container_world;
mod process_world;
mod waiter;

use crate::docker::engine::{Cleanup, Engine};
use crate::support;
use awman::engine::conformance::{container, process};
use awman_test_support::capability::Subprocess;
use awman_test_support::conformance::{docker_cli, DockerCliUnderTest};

/// awman's container runtime, driving the real docker CLI against the real
/// Docker Engine, conforms: availability, image identity and HOME, builds
/// (streamed output, typed failure), background containers (exec relay,
/// values by name only, kill and drop cleanup), owned-container listing.
/// Also the plain docker CLI contract the verified fake mirrors.
#[test]
fn real_docker_cli_conforms() {
    let mut sandbox = support::sandbox("conformance::real_docker_cli_conforms");
    let engine = Engine::grant(&mut sandbox);
    let cleanup = Cleanup::new(&engine);
    cleanup
        .image(container::PRESENT_IMAGE)
        .image(container::BUILT_IMAGE);

    docker_cli(&DockerCliUnderTest {
        sandbox: engine.sandbox,
        cli: engine.docker.cli.clone(),
    })
    .assert_conforms();

    let world = container_world::RealDockerWorld::new(&engine);
    let report = container::container_runtime(&world);
    report.assert_conforms();
    assert!(
        report.not_applicable.is_empty(),
        "a real engine declares every container check applicable: {:?}",
        report.not_applicable
    );
    drop(cleanup);
    sandbox.finish();
}

/// awman's host process runner conforms against real child processes: exit
/// status, both streams in order, cancellation before and during a run, a
/// deadline, a missing program, and a descendant holding the pipes open.
#[test]
fn real_host_command_runner_conforms() {
    let sandbox = support::sandbox("conformance::real_host_command_runner_conforms");
    let _ = sandbox.require::<Subprocess>();
    let env = support::env_snapshot(&sandbox);
    let world = process_world::ShellProcessWorld::new(&sandbox, env);
    let report = process::host_command_runner(&world);
    report.assert_conforms();
    drop(world);
    sandbox.finish();
}

/// A run that names no real host resources is blocked, never passed and
/// never skipped: each real-boundary test, started in a child that names no
/// `AWMAN_TEST_HOST_RESOURCES`, fails with `BLOCKED[<capability>]`, reports
/// zero passed tests and does not touch the host's Docker, registries or
/// SSH. This is what a runner without Docker reports for the tier.
#[test]
fn a_run_naming_no_host_resources_is_blocked_not_passed_or_skipped() {
    let mut sandbox = support::sandbox(
        "conformance::a_run_naming_no_host_resources_is_blocked_not_passed_or_skipped",
    );
    let exe = std::env::current_exe().expect("this test binary");
    sandbox.allow_tool(&exe);
    for (test, capability) in [
        (
            "docker::the_engine_is_reachable_and_unknown_images_are_absent",
            "docker",
        ),
        ("conformance::real_docker_cli_conforms", "docker"),
        ("templates::the_claude_template_builds", "docker"),
        (
            "registry::a_real_registry_conforms_to_the_acquirer_contract",
            "local-registry",
        ),
        (
            "git::ssh_conforms_to_the_transport_contract",
            "git-transports",
        ),
    ] {
        let out = sandbox
            .command(&exe)
            // Reuse the run-owned parent, rather than recursively nesting the
            // child's socket directory under this test's already long root.
            .env("AWMAN_TEST_TMPROOT", sandbox.root().parent().unwrap())
            .args(["--exact", test, "--test-threads=1", "--nocapture"])
            .label("boundary tests without host resources")
            .run();
        let text = format!("{}\n{}", out.stdout_text(), out.stderr_text());
        assert!(
            !out.exit.success(),
            "{test} passed without {capability}:\n{text}"
        );
        assert!(
            text.contains(&format!("BLOCKED[{capability}]")),
            "{test} was not reported BLOCKED[{capability}]:\n{text}"
        );
        assert!(
            text.contains("0 passed") && !text.contains("1 passed"),
            "{test} counted as passed:\n{text}"
        );
        assert!(
            !text.contains("ignored,") || text.contains("0 ignored"),
            "{test} was skipped:\n{text}"
        );
    }
    sandbox.finish();
}
