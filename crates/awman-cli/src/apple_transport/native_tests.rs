use super::*;
use awman::engine::oci::apple_store::{check_release, XpcTransport, CONTRACT};
use std::time::{Duration, Instant};

fn native_enabled() -> bool {
    std::env::var("AWMAN_TEST_APPLE_CONTAINER").is_ok_and(|v| v == "1")
}
fn wait(secs: u64) -> (Instant, fn() -> bool) {
    (Instant::now() + Duration::from_secs(secs), || false)
}

/// Requires the real, running Apple Containers service
/// (`AWMAN_TEST_APPLE_CONTAINER=1`); fails closed when it is absent.
#[test]
fn apple_native_ping_reports_a_supported_release() {
    if !native_enabled() {
        return;
    }
    let (deadline, stop) = wait(60);
    let reply = NativeXpc
        .send(
            CONTRACT.api_service,
            &[(
                CONTRACT.route_key,
                XpcValue::String(CONTRACT.ping_route.into()),
            )],
            &[
                (CONTRACT.version_key, XpcKind::String),
                (CONTRACT.commit_key, XpcKind::String),
                (CONTRACT.build_key, XpcKind::String),
            ],
            &WaitControl {
                deadline,
                stop: &stop,
            },
        )
        .expect("the Apple Containers API server must answer");
    let release = check_release(&reply).expect("installed release must be supported");
    eprintln!("apple container release {release:?}");
}

#[test]
fn apple_native_list_returns_image_descriptions() {
    if !native_enabled() {
        return;
    }
    let (deadline, stop) = wait(60);
    let reply = NativeXpc
        .send(
            CONTRACT.images_service,
            &[(
                CONTRACT.route_key,
                XpcValue::String(CONTRACT.list_route.into()),
            )],
            &[(CONTRACT.descriptions_key, XpcKind::Data)],
            &WaitControl {
                deadline,
                stop: &stop,
            },
        )
        .expect("the image helper must answer");
    let Some(XpcValue::Data(listing)) = reply.get(CONTRACT.descriptions_key) else {
        panic!("no descriptions in {reply:?}");
    };
    let parsed: Vec<serde_json::Value> = serde_json::from_slice(listing).unwrap();
    eprintln!("apple store lists {} images", parsed.len());
}

#[test]
fn apple_native_stop_cancels_an_outstanding_request() {
    if !native_enabled() {
        return;
    }
    let stop = || true;
    let err = NativeXpc.send(
        CONTRACT.images_service,
        &[(
            CONTRACT.route_key,
            XpcValue::String(CONTRACT.list_route.into()),
        )],
        &[],
        &WaitControl {
            deadline: Instant::now() + Duration::from_secs(60),
            stop: &stop,
        },
    );
    // Either the reply beat the first poll or the stop was honoured.
    assert!(matches!(err, Ok(_) | Err(XpcFailure::Stopped)), "{err:?}");
}

/// Exports a real store image through the production acquirer and
/// validates it. `AWMAN_TEST_APPLE_IMAGE` names the image (default
/// `awman-amux:latest`).
#[test]
fn apple_native_export_round_trip() {
    if !native_enabled() {
        return;
    }
    use awman::data::config::env::EnvSnapshot;
    use awman::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
    use awman::data::oci_identity::OciPlatform;
    use awman::engine::oci::{
        AcquireLimits, AcquirePolicy, AcquireRequest, CachingAcquirer, DiskSpace, ImageAcquirer,
        RetryPolicy,
    };
    struct Unknown;
    impl DiskSpace for Unknown {
        fn available(&self, _: &std::path::Path) -> Option<u64> {
            None
        }
    }
    let image =
        std::env::var("AWMAN_TEST_APPLE_IMAGE").unwrap_or_else(|_| "awman-amux:latest".into());
    let state = tempfile::tempdir().unwrap();
    let acq = CachingAcquirer::new(
        state.path(),
        AcquireLimits::DEFAULT,
        EnvSnapshot::empty(),
        std::sync::Arc::new(Unknown),
    )
    .with_retry(RetryPolicy::NONE)
    .with_apple_transport(Some(std::sync::Arc::new(NativeXpc)));
    let request = AcquireRequest {
        tag: image.clone(),
        source: ImageSourceSpec::AppleStore { reference: None },
        platform: OciPlatform::host_linux(),
        policy: AcquirePolicy::IfMissing,
        registries: Default::default(),
    };
    let started = Instant::now();
    let got = acq.acquire(&request, &mut |_| {}).expect("native export");
    assert_eq!(got.identity.source, ImageSourceKind::AppleStore);
    // Keep the validated archive for guest tests that follow.
    if let Some(keep) = std::env::var_os("AWMAN_TEST_APPLE_KEEP_ARCHIVE") {
        std::fs::copy(&got.archive, keep).expect("keep the exported archive");
    }
    eprintln!(
        "exported {image} in {:?}: manifest {} config {} platform {} ({} bytes)",
        started.elapsed(),
        got.identity.manifest_digest,
        got.identity.config_digest,
        got.identity.platform,
        got.bytes
    );
}

fn native_acquirer(state: &std::path::Path) -> awman::engine::oci::CachingAcquirer {
    use awman::data::config::env::EnvSnapshot;
    use awman::engine::oci::{AcquireLimits, CachingAcquirer, DiskSpace, RetryPolicy};
    struct Unknown;
    impl DiskSpace for Unknown {
        fn available(&self, _: &std::path::Path) -> Option<u64> {
            None
        }
    }
    CachingAcquirer::new(
        state,
        AcquireLimits::DEFAULT,
        EnvSnapshot::empty(),
        std::sync::Arc::new(Unknown),
    )
    .with_retry(RetryPolicy::NONE)
    .with_apple_transport(Some(std::sync::Arc::new(NativeXpc)))
}

fn native_request(policy: awman::engine::oci::AcquirePolicy) -> awman::engine::oci::AcquireRequest {
    use awman::data::config::image_source::ImageSourceSpec;
    use awman::data::oci_identity::OciPlatform;
    awman::engine::oci::AcquireRequest {
        tag: std::env::var("AWMAN_TEST_APPLE_IMAGE").unwrap_or_else(|_| "awman-amux:latest".into()),
        source: ImageSourceSpec::AppleStore { reference: None },
        platform: OciPlatform::host_linux(),
        policy,
        registries: Default::default(),
    }
}

fn staging_entries(state: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(state.join("oci-cache").join("tmp"))
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default()
}

/// Cancels a live export mid-flight; the helper's late write must not
/// leave anything in awman's staging area or cache.
#[test]
fn apple_native_cancelled_export_leaves_nothing() {
    if !native_enabled() {
        return;
    }
    use awman::engine::oci::{AcquirePolicy, ImageAcquirer};
    let state = tempfile::tempdir().unwrap();
    let acq = native_acquirer(state.path());
    let token = acq.cancel_token();
    // Cancel while `imageSave` is outstanding: as soon as the bridge's
    // private export directory exists.
    let watch = state.path().to_path_buf();
    let canceller = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            if staging_entries(&watch)
                .iter()
                .any(|d| d.join("apple-export").exists())
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        token.cancel();
    });
    let started = Instant::now();
    let err = acq
        .acquire(&native_request(AcquirePolicy::IfMissing), &mut |_| {})
        .expect_err("the export must be cancelled");
    canceller.join().unwrap();
    assert!(awman::engine::oci::retry::is_cancelled(&err), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    // Give the helper time to finish (or fail) its abandoned write.
    std::thread::sleep(Duration::from_secs(20));
    assert!(
        staging_entries(state.path()).is_empty(),
        "{:?}",
        staging_entries(state.path())
    );
    let images = std::fs::read_dir(state.path().join("oci-cache").join("images"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(images, 0);
}

/// Two consumers importing the same image at once both get the one
/// committed image.
#[test]
fn apple_native_concurrent_imports_share_one_commit() {
    if !native_enabled() {
        return;
    }
    use awman::engine::oci::{AcquirePolicy, ImageAcquirer};
    let state = tempfile::tempdir().unwrap();
    let path = state.path().to_path_buf();
    let run = |p: std::path::PathBuf| {
        std::thread::spawn(move || {
            native_acquirer(&p)
                .acquire(&native_request(AcquirePolicy::IfMissing), &mut |_| {})
                .map(|i| i.identity.clone())
        })
    };
    let (a, b) = (run(path.clone()), run(path.clone()));
    let (a, b) = (a.join().unwrap().unwrap(), b.join().unwrap().unwrap());
    assert_eq!(a, b);
    assert!(staging_entries(&path).is_empty());
}

/// Kills the image helper while `imageSave` is outstanding. The bridge
/// must report a retryable interruption, and the next attempt must
/// succeed once launchd restarts the helper on demand. Opt-in with
/// `AWMAN_TEST_APPLE_KILL_HELPER=1`: it disrupts the running service.
/// The helper's save takes only a second or two, so name a large image
/// with `AWMAN_TEST_APPLE_IMAGE` for the kill to land mid-save.
#[test]
fn apple_native_helper_crash_is_retried() {
    if !native_enabled() || std::env::var("AWMAN_TEST_APPLE_KILL_HELPER").as_deref() != Ok("1") {
        return;
    }
    use awman::engine::oci::{AcquirePolicy, ImageAcquirer, RetryPolicy};
    use std::sync::{Arc, Mutex};
    /// Records every native outcome as `(route, failure)`.
    struct Recording(Mutex<Vec<(String, Option<XpcFailure>)>>);
    impl XpcTransport for Recording {
        fn send(
            &self,
            service: &str,
            request: &[(&str, XpcValue)],
            keys: &[(&str, XpcKind)],
            wait: &WaitControl<'_>,
        ) -> Result<XpcReply, XpcFailure> {
            let route = request
                .iter()
                .find_map(|(_, v)| match v {
                    XpcValue::String(r) if r.starts_with("image") || r == "ping" => Some(r.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let result = NativeXpc.send(service, request, keys, wait);
            self.0
                .lock()
                .unwrap()
                .push((route, result.as_ref().err().cloned()));
            result
        }
    }
    let state = tempfile::tempdir().unwrap();
    let recording = Arc::new(Recording(Mutex::new(Vec::new())));
    let acq = native_acquirer(state.path())
        .with_apple_transport(Some(recording.clone()))
        .with_retry(RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_secs(2),
            ..RetryPolicy::DEFAULT
        });
    let watch = state.path().to_path_buf();
    let killer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            if staging_entries(&watch)
                .iter()
                .any(|d| d.join("apple-export").exists())
            {
                std::thread::sleep(Duration::from_millis(100));
                return std::process::Command::new("/usr/bin/pkill")
                    .args([
                        "-9",
                        "-f",
                        "^/.*/container-core-images/bin/container-core-images start",
                    ])
                    .status()
                    .is_ok_and(|s| s.success());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    });
    let got = acq
        .acquire(&native_request(AcquirePolicy::IfMissing), &mut |_| {})
        .expect("the export is retried after the helper restarts");
    assert!(killer.join().unwrap(), "the helper was not killed");
    let calls = recording.0.lock().unwrap().clone();
    eprintln!("helper-crash calls: {calls:?}");
    assert!(
        calls
            .iter()
            .any(|(r, f)| r == "imageSave" && *f == Some(XpcFailure::Interrupted)),
        "{calls:?}"
    );
    assert!(
        matches!(calls.last(), Some((r, None)) if r == "imageSave"),
        "{calls:?}"
    );
    assert!(staging_entries(state.path()).is_empty());
    eprintln!("helper crash retried: {}", got.identity.manifest_digest);
}
