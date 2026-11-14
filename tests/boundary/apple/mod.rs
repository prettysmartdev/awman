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

// The important native path is a real store export, validated by the production
// acquirer, followed by cache reuse. Recovery faults belong in the component
// transport contract; native validation never kills the operator's service.
#[cfg(target_os = "macos")]
#[test]
fn native_store_export_validates_content_and_reuses_the_cache() {
    use awman::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
    use awman::data::oci_identity::OciPlatform;
    use awman::engine::oci::{
        AcquireLimits, AcquirePolicy, AcquireRequest, CachingAcquirer, DiskSpace, ImageAcquirer,
        RetryPolicy,
    };
    use awman_test_support::{capability::GuestFixtures, TestSandbox};
    use std::{path::Path, sync::Arc};
    struct Space;
    impl DiskSpace for Space {
        fn available(&self, _: &Path) -> Option<u64> {
            None
        }
    }
    struct Image<'a>(&'a TestSandbox);
    impl Drop for Image<'_> {
        fn drop(&mut self) {
            let output = self
                .0
                .command("container")
                .args(["image", "delete", "awman-test/guest:fixture"])
                .run();
            if !std::thread::panicking() {
                output.assert_success();
            }
        }
    }
    let mut s =
        support::sandbox("apple::native_store_export_validates_content_and_reuses_the_cache");
    let apple = s.require::<AppleContainers>();
    s.grant(&apple);
    let fixture = s.require::<GuestFixtures>();
    let runtime = ContainerRuntime::apple_in(support::env_snapshot(&s));
    assert!(!runtime.image_exists("awman-test/guest:fixture"), "BLOCKED[fixture-ownership]: remove or rename an existing awman-test/guest:fixture before this test");
    // Own cleanup before import: a partially successful load must not leak either.
    let image = Image(&s);
    s.command("container")
        .args(["image", "load", "--input"])
        .arg(&fixture.paths["guest-oci-base"])
        .run()
        .assert_success();
    let acquirer = CachingAcquirer::new(
        &s.root().join("cache"),
        AcquireLimits::DEFAULT,
        support::env_snapshot(&s),
        Arc::new(Space),
    )
    .with_retry(RetryPolicy::NONE)
    .with_apple_transport(Some(Arc::new(awman_cli::NativeXpc)));
    let request = AcquireRequest {
        tag: "awman-test/guest:fixture".into(),
        source: ImageSourceSpec::AppleStore { reference: None },
        platform: OciPlatform::host_linux(),
        policy: AcquirePolicy::IfMissing,
        registries: Default::default(),
    };
    let first = acquirer
        .acquire(&request, &mut |_| {})
        .expect("export through native XPC");
    assert_eq!(first.identity.source, ImageSourceKind::AppleStore);
    assert!(first.bytes > 0 && first.archive.is_file());
    drop(image);
    assert!(!runtime.image_exists("awman-test/guest:fixture"));
    let cached = acquirer
        .acquire(&request, &mut |_| {})
        .expect("validated cached bytes remain usable after source removal");
    assert_eq!(cached.identity, first.identity);
    assert_eq!(cached.archive, first.archive);
    s.finish();
}
