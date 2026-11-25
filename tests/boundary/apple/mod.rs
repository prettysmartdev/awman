//! Boundary tests: Apple Containers (`container` CLI with its service
//! running). macOS only; on other platforms the tests are listed and
//! counted as `not-applicable`, never silently absent.

use crate::support;
use awman::engine::container::ContainerRuntime;
use awman_test_support::capability::AppleContainers;

struct OwnedFixture<'a> {
    sandbox: &'a awman_test_support::TestSandbox,
    tag: String,
}

impl OwnedFixture<'_> {
    fn load<'a>(
        sandbox: &'a awman_test_support::TestSandbox,
        archive: &std::path::Path,
    ) -> OwnedFixture<'a> {
        let (tag, private_archive) =
            awman_test_support::apple_image::private_fixture(sandbox, archive);
        awman_test_support::apple_image::assert_absent(sandbox, "container", &tag);
        let owned = OwnedFixture { sandbox, tag };
        sandbox
            .command("container")
            .args(["image", "load", "--input"])
            .arg(private_archive)
            .run()
            .assert_success();
        owned
    }
}

impl Drop for OwnedFixture<'_> {
    fn drop(&mut self) {
        awman_test_support::apple_image::delete_and_verify(self.sandbox, "container", &self.tag);
    }
}

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
    use awman_test_support::capability::GuestFixtures;
    use std::{path::Path, sync::Arc};
    struct Space;
    impl DiskSpace for Space {
        fn available(&self, _: &Path) -> Option<u64> {
            None
        }
    }
    let mut s =
        support::sandbox("apple::native_store_export_validates_content_and_reuses_the_cache");
    let apple = s.require::<AppleContainers>();
    s.grant(&apple);
    let fixture = s.require::<GuestFixtures>();
    let runtime = ContainerRuntime::apple_in(support::env_snapshot(&s));
    // Own cleanup before import: a partially successful load must not leak either.
    let image = OwnedFixture::load(&s, &fixture.paths["guest-oci-base"]);
    let acquirer = CachingAcquirer::new(
        &s.root().join("cache"),
        AcquireLimits::DEFAULT,
        support::env_snapshot(&s),
        Arc::new(Space),
    )
    .with_retry(RetryPolicy::NONE)
    .with_apple_transport(Some(Arc::new(awman_cli::NativeXpc)));
    let request = AcquireRequest {
        tag: image.tag.clone(),
        source: ImageSourceSpec::AppleStore { reference: None },
        platform: OciPlatform::host_linux(),
        policy: AcquirePolicy::IfMissing,
        registries: Default::default(),
    };
    let first = acquirer
        .acquire(&request, &mut |_| {})
        .expect("export through native XPC");
    let metadata = runtime
        .inspect_image(&image.tag)
        .expect("read the installed Apple version's inspection format")
        .expect("loaded fixture is present");
    assert!(!metadata.id.is_empty());
    if let Some(manifest) = metadata.manifest_digest {
        assert_eq!(manifest, first.identity.manifest_digest);
    }
    assert_eq!(first.identity.source, ImageSourceKind::AppleStore);
    assert!(first.bytes > 0 && first.archive.is_file());
    let tag = image.tag.clone();
    drop(image);
    assert!(!runtime.image_exists(&tag));
    let cached = acquirer
        .acquire(&request, &mut |_| {})
        .expect("validated cached bytes remain usable after source removal");
    assert_eq!(cached.identity, first.identity);
    assert_eq!(cached.archive, first.archive);
    s.finish();
}

/// WI 0152: the same read-only proof against the Apple container service:
/// workspace writes fail inside the container; `/tmp`, `/awman/output`, a
/// mounted context directory and the agent home stay writable.
#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "not-applicable: Apple Containers exist only on macOS"
)]
fn a_read_only_workspace_rejects_writes_while_scratch_output_context_and_home_stay_writable() {
    use awman_test_support::capability::GuestFixtures;
    let mut s = support::sandbox(
        "apple::a_read_only_workspace_rejects_writes_while_scratch_output_context_and_home_stay_writable",
    );
    let apple = s.require::<AppleContainers>();
    s.grant(&apple);
    let fixture = s.require::<GuestFixtures>();
    let runtime = ContainerRuntime::apple_in(support::env_snapshot(&s));
    let image = OwnedFixture::load(&s, &fixture.paths["guest-oci-base"]);
    support::read_only::prove(&support::read_only::Proof {
        runtime: std::sync::Arc::new(runtime),
        image: &image.tag,
        root: &s.root().join("read-only-proof"),
        builtin: false,
    });
    drop(image);
    s.finish();
}
