//! Boundary tests: Apple Containers (`container` CLI with its service
//! running). macOS only; on other platforms the tests are listed and
//! counted as `not-applicable`, never silently absent.

use crate::support;
use awman::engine::container::ContainerRuntime;
use awman_test_support::capability::AppleContainers;

/// awman's Apple runtime reaches the service, reports an unknown image as
/// absent and lists running awman containers without error.
#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "not-applicable: Apple Containers exist only on macOS"
)]
fn the_service_is_reachable_and_unknown_images_are_absent() {
    let mut sandbox =
        support::sandbox("apple::the_service_is_reachable_and_unknown_images_are_absent");
    let apple = sandbox.require::<AppleContainers>();
    sandbox.grant(&apple);
    let runtime = ContainerRuntime::apple_in(support::env_snapshot(&sandbox));

    runtime.availability().unwrap_or_else(|e| {
        panic!("BLOCKED[apple-containers]: the container service is not running: {e}")
    });
    assert!(
        runtime.display_name().contains("Apple"),
        "{}",
        runtime.display_name()
    );
    assert!(!runtime.image_exists("awman-boundary/unknown:none"));
    runtime
        .list_running_all()
        .unwrap_or_else(|e| panic!("listing awman containers failed: {e}"));
    sandbox.finish();
}
