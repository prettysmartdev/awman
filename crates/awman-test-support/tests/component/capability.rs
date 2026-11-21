//! `require::<T>()` in the component tier.

use crate::{expect_panic, fake_cli_exe, sandbox};
use awman_test_support::capability::{
    AwmanBinary, Docker, FakeCliExe, Hypervisor, LocalGit, Loopback, Subprocess, TempDir,
};
use awman_test_support::{TestCase, TestSandbox, Tier};

#[test]
fn guaranteed_capabilities_are_granted() {
    let sandbox = sandbox("capability::guaranteed_capabilities_are_granted");
    sandbox.require::<TempDir>();
    sandbox.require::<Loopback>();
    sandbox.require::<Subprocess>();
    let fake = sandbox.require::<FakeCliExe>();
    assert_eq!(fake.exe, fake_cli_exe());
    let git = sandbox.require::<LocalGit>();
    assert!(git.exe.is_file());
}

#[test]
fn a_guaranteed_but_absent_capability_blocks() {
    // No awman binary is registered in this package's tests.
    let sandbox = sandbox("capability::a_guaranteed_but_absent_capability_blocks");
    let text = expect_panic(|| {
        sandbox.require::<AwmanBinary>();
    });
    assert!(text.starts_with("BLOCKED[awman-binary]: "), "{text}");
}

#[test]
fn a_capability_outside_the_tier_is_misplaced() {
    let sandbox = sandbox("capability::a_capability_outside_the_tier_is_misplaced");
    let text = expect_panic(|| {
        sandbox.require::<Docker>();
    });
    assert!(text.starts_with("MISPLACED[docker]: "), "{text}");
    let text = expect_panic(|| {
        sandbox.require::<Hypervisor>();
    });
    assert!(text.starts_with("MISPLACED[hypervisor]: "), "{text}");
}

#[test]
fn boundary_sandboxes_state_their_tier() {
    // Whether a real resource is granted is covered with an injected
    // runtime snapshot in the unit tests (capability::tests).
    let case = || TestCase::boundary("capability::boundary_sandboxes_state_their_tier");
    let selected = awman_test_support::runtime_env::get().tier.clone();
    if selected.as_deref().is_some_and(|tier| tier != "boundary") {
        // A run that selected another tier may only validate the placement.
        let text = expect_panic(|| {
            TestSandbox::for_test(case());
        });
        assert!(text.starts_with("MISPLACED[tier]: "), "{text}");
    } else {
        let sandbox = TestSandbox::for_test(case());
        assert_eq!(sandbox.tier(), Tier::Boundary);
        let text = expect_panic(|| {
            sandbox.require::<Hypervisor>();
        });
        assert!(text.starts_with("MISPLACED[hypervisor]: "), "{text}");
    }
}

#[test]
fn granting_adds_the_capability_environment() {
    let mut sandbox = sandbox("capability::granting_adds_the_capability_environment");
    let git = sandbox.require::<LocalGit>();
    let before = sandbox.env().clone();
    sandbox.grant(&git);
    assert_eq!(sandbox.env(), &before, "local git needs nothing extra");
}
