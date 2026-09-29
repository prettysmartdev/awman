use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::*;
use crate::data::config::env::EnvSnapshot;
use crate::data::config::image_source::ImageSourceSpec;
use crate::engine::oci::archive::tests::{arm64, layer_tar, limits, oci_archive, L};
use crate::engine::oci::verify::tests::FixedDisk;
use crate::engine::oci::{
    AcquirePolicy, AcquireRequest, CachingAcquirer, ImageAcquirer, RetryPolicy,
};

const V012: &str = "container-apiserver version 0.12.0 (build: release, commit: 651811c)";
const C012: &str = "651811cc090937457956643dd2c454df77eb141b";

/// What the fake helper does when `imageSave` arrives.
#[derive(Clone)]
enum Save {
    /// Write these bytes to `filePath`.
    Write(Vec<u8>),
    /// Reply with success without writing anything.
    Nothing,
    /// Put a symlink at `filePath`.
    Symlink,
    /// Reply with a protocol error.
    Error(&'static str, String),
    /// Report a connection failure.
    Fail(XpcFailure),
    /// Wait until the caller stops or the deadline passes.
    Hang,
}

struct Fake {
    version: String,
    commit: String,
    build: String,
    ping: Option<XpcFailure>,
    listing: Vec<u8>,
    save: Save,
    /// Every `(service, route)` sent, in order.
    calls: Mutex<Vec<(String, String)>>,
    /// The last save request's fields.
    saved: Mutex<BTreeMap<String, XpcValue>>,
}

impl Fake {
    fn new(listing: serde_json::Value, save: Save) -> Self {
        Self {
            version: V012.into(),
            commit: C012.into(),
            build: "release".into(),
            ping: None,
            listing: listing.to_string().into_bytes(),
            save,
            calls: Mutex::new(Vec::new()),
            saved: Mutex::new(BTreeMap::new()),
        }
    }

    fn routes(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, r)| r.clone())
            .collect()
    }
}

fn string(v: &str) -> XpcValue {
    XpcValue::String(v.into())
}

impl XpcTransport for Fake {
    fn send(
        &self,
        service: &str,
        request: &[(&str, XpcValue)],
        _reply_keys: &[(&str, XpcKind)],
        wait: &WaitControl<'_>,
    ) -> Result<XpcReply, XpcFailure> {
        let fields: BTreeMap<String, XpcValue> = request
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        let Some(XpcValue::String(route)) = fields.get(CONTRACT.route_key) else {
            panic!("no route");
        };
        self.calls
            .lock()
            .unwrap()
            .push((service.to_string(), route.clone()));
        let mut reply = XpcReply::new();
        match (service, route.as_str()) {
            (s, "ping") if s == CONTRACT.api_service => {
                if let Some(f) = &self.ping {
                    return Err(f.clone());
                }
                reply.insert(CONTRACT.version_key.into(), string(&self.version));
                reply.insert(CONTRACT.commit_key.into(), string(&self.commit));
                reply.insert(CONTRACT.build_key.into(), string(&self.build));
            }
            (s, "imageList") if s == CONTRACT.images_service => {
                reply.insert(
                    CONTRACT.descriptions_key.into(),
                    XpcValue::Data(self.listing.clone()),
                );
            }
            (s, "imageSave") if s == CONTRACT.images_service => {
                *self.saved.lock().unwrap() = fields.clone();
                let Some(XpcValue::String(out)) = fields.get(CONTRACT.file_path_key) else {
                    panic!("no filePath");
                };
                match &self.save {
                    Save::Write(bytes) => std::fs::write(out, bytes).unwrap(),
                    Save::Nothing => {}
                    Save::Symlink => {
                        let target = Path::new(out).with_file_name("elsewhere");
                        std::fs::write(&target, b"x").unwrap();
                        std::os::unix::fs::symlink(&target, out).unwrap();
                    }
                    Save::Error(code, message) => {
                        let body = serde_json::json!({"code": code, "message": message});
                        reply.insert(
                            CONTRACT.error_key.into(),
                            XpcValue::Data(body.to_string().into_bytes()),
                        );
                    }
                    Save::Fail(f) => return Err(f.clone()),
                    Save::Hang => loop {
                        if (wait.stop)() {
                            return Err(XpcFailure::Stopped);
                        }
                        if Instant::now() >= wait.deadline {
                            return Err(XpcFailure::TimedOut);
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    },
                }
            }
            other => panic!("unexpected request {other:?}"),
        }
        Ok(reply)
    }
}

fn entry(reference: &str, digest: &str, media_type: &str) -> serde_json::Value {
    serde_json::json!({
        "reference": reference,
        "descriptor": {"digest": digest, "mediaType": media_type, "size": 375}
    })
}

const INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

fn image() -> (Vec<u8>, String) {
    oci_archive(&arm64(), &[layer_tar(&[L::File("bin/tool", b"x", 0o755)])])
}

fn request() -> AcquireRequest {
    AcquireRequest {
        tag: "awman-x-claude:latest".into(),
        source: ImageSourceSpec::AppleStore { reference: None },
        platform: arm64(),
        policy: AcquirePolicy::IfMissing,
        registries: BTreeMap::new(),
    }
}

fn acquirer(state: &Path, fake: Arc<Fake>) -> CachingAcquirer {
    CachingAcquirer::new(
        state,
        limits(),
        EnvSnapshot::empty(),
        Arc::new(FixedDisk(None)),
    )
    .with_retry(RetryPolicy::NONE)
    .with_apple_transport(Some(fake))
}

fn tmp_entries(state: &Path) -> usize {
    std::fs::read_dir(state.join(crate::engine::oci::cache::CACHE_DIR).join("tmp"))
        .map(|d| d.count())
        .unwrap_or(0)
}

// ── Release check ──────────────────────────────────────────────────────────

fn ping(version: &str, commit: &str, build: &str) -> XpcReply {
    let mut r = XpcReply::new();
    r.insert(CONTRACT.version_key.into(), string(version));
    r.insert(CONTRACT.commit_key.into(), string(commit));
    r.insert(CONTRACT.build_key.into(), string(build));
    r
}

#[test]
fn both_version_spellings_of_supported_releases_pass() {
    let r = check_release(&ping(V012, C012, "release")).unwrap();
    assert_eq!(r.version, "0.12.0");
    let r = check_release(&ping(
        "1.5.0",
        "d265d669ecae041bf338cb3b39c4118316d138f0",
        "release",
    ))
    .unwrap();
    assert_eq!(r.version, "1.5.0");
}

#[test]
fn unknown_versions_commits_and_debug_builds_are_refused() {
    for (v, c, b) in [
        ("1.6.0", C012, "release"),
        (
            "0.12.0",
            "0000000000000000000000000000000000000000",
            "release",
        ),
        ("1.5.0", C012, "release"),
        (V012, C012, "debug"),
        ("garbage", C012, "release"),
    ] {
        match check_release(&ping(v, c, b)) {
            Err(EngineError::UnsupportedImageSource { reason, .. }) => {
                assert!(reason.contains("0.12.0, 1.4.1, 1.5.0"), "{reason}")
            }
            other => panic!("{v}/{c}/{b} accepted: {other:?}"),
        }
    }
    let mut missing = ping(V012, C012, "release");
    missing.remove(CONTRACT.commit_key);
    assert!(check_release(&missing).is_err());
}

#[test]
fn the_supported_table_pins_full_commits_and_names_its_evidence() {
    for r in SUPPORTED_RELEASES {
        assert_eq!(r.commit.len(), 40);
        assert!(parse_version(r.version).is_some());
    }
    assert!(SUPPORTED_RELEASES
        .iter()
        .any(|r| r.evidence == ReleaseEvidence::Native));
    const { assert!(!CONTRACT.protocol_versioned) };
}

// ── Selection ──────────────────────────────────────────────────────────────

#[test]
fn references_normalize_like_the_store() {
    assert_eq!(
        normalize_reference("awman-x"),
        "docker.io/library/awman-x:latest"
    );
    assert_eq!(
        normalize_reference("docker.io/library/awman-x:latest"),
        "docker.io/library/awman-x:latest"
    );
    assert_eq!(normalize_reference("org/app:1"), "docker.io/org/app:1");
    assert_eq!(
        normalize_reference("localhost:5000/app"),
        "localhost:5000/app:latest"
    );
    assert_eq!(
        normalize_reference("ghcr.io/a/b@sha256:x"),
        "ghcr.io/a/b@sha256:x"
    );
}

#[test]
fn selection_requires_exactly_one_named_image() {
    let a = entry("awman-x-claude:latest", &digest('a'), INDEX);
    let b = entry(
        "docker.io/library/awman-x-claude:latest",
        &digest('b'),
        INDEX,
    );
    let other = entry("awman-y:latest", &digest('c'), INDEX);
    let listing = serde_json::json!([a, other]).to_string();
    let sel = select_image(listing.as_bytes(), "awman-x-claude:latest").unwrap();
    assert_eq!(sel.digest, digest('a'));

    let dup = serde_json::json!([a, a]).to_string();
    assert!(select_image(dup.as_bytes(), "awman-x-claude").is_ok());

    let ambiguous = serde_json::json!([a, b]).to_string();
    match select_image(ambiguous.as_bytes(), "awman-x-claude:latest") {
        Err(EngineError::Config(m)) => assert!(m.contains("2 different"), "{m}"),
        other => panic!("{other:?}"),
    }
    match select_image(listing.as_bytes(), "absent") {
        Err(EngineError::Config(m)) => assert!(m.contains("not in the Apple"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(select_image(b"{}", "a").is_err());
    let bad = serde_json::json!([entry("a", "sha256:zz", INDEX)]).to_string();
    assert!(select_image(bad.as_bytes(), "a").is_err());
}

#[test]
fn malicious_names_are_sanitized_in_errors() {
    let name = format!("evil\u{1b}[31m{}", "x".repeat(400));
    match select_image(b"[]", &name) {
        Err(EngineError::Config(m)) => {
            assert!(!m.contains('\u{1b}'));
            assert!(m.len() < 600);
        }
        other => panic!("{other:?}"),
    }
}

// ── Export through the acquirer ────────────────────────────────────────────

#[test]
fn a_store_image_is_exported_validated_and_cached() {
    let state = tempfile::tempdir().unwrap();
    let (bytes, manifest) = image();
    let fake = Arc::new(Fake::new(
        serde_json::json!([entry("awman-x-claude:latest", &digest('a'), INDEX)]),
        Save::Write(bytes),
    ));
    let acq = acquirer(state.path(), fake.clone());
    let got = acq.acquire(&request(), &mut |_| {}).unwrap();
    assert_eq!(got.identity.source, ImageSourceKind::AppleStore);
    assert_eq!(
        got.identity.manifest_digest.as_str(),
        format!("sha256:{manifest}")
    );
    assert_eq!(fake.routes(), ["ping", "imageList", "imageSave"]);

    // The request carried only the selected entry and awman's platform.
    let saved = fake.saved.lock().unwrap().clone();
    let Some(XpcValue::Data(d)) = saved.get(CONTRACT.descriptions_key) else {
        panic!()
    };
    let sent: Vec<serde_json::Value> = serde_json::from_slice(d).unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["reference"], "awman-x-claude:latest");
    let Some(XpcValue::Data(p)) = saved.get(CONTRACT.platform_key) else {
        panic!()
    };
    let p: serde_json::Value = serde_json::from_slice(p).unwrap();
    assert_eq!(p["architecture"], "arm64");
    assert_eq!(p["os"], "linux");
    let Some(XpcValue::String(out)) = saved.get(CONTRACT.file_path_key) else {
        panic!()
    };
    assert!(Path::new(out).starts_with(state.path()));
    assert_eq!(tmp_entries(state.path()), 0);

    // A second ready is served from the cache without contacting the store.
    let again = acq.acquire(&request(), &mut |_| {}).unwrap();
    assert_eq!(again.identity, got.identity);
    assert_eq!(fake.routes().len(), 3);
}

#[test]
fn an_unknown_release_sends_no_image_route() {
    let state = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(serde_json::json!([]), Save::Nothing);
    fake.version = "9.9.9".into();
    let fake = Arc::new(fake);
    let err = acquirer(state.path(), fake.clone())
        .acquire(&request(), &mut |_| {})
        .unwrap_err();
    assert!(
        matches!(err, EngineError::UnsupportedImageSource { .. }),
        "{err}"
    );
    assert_eq!(fake.routes(), ["ping"]);
}

#[test]
fn a_stopped_service_is_blocked_with_the_start_command() {
    let state = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(serde_json::json!([]), Save::Nothing);
    fake.ping = Some(XpcFailure::Unavailable);
    let err = acquirer(state.path(), Arc::new(fake))
        .acquire(&request(), &mut |_| {})
        .unwrap_err();
    match err {
        EngineError::ImageSourceBlocked { reason, .. } => {
            assert!(reason.contains("container system start"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
}

fn export_error(save: Save) -> EngineError {
    let state = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::new(
        serde_json::json!([entry("awman-x-claude:latest", &digest('a'), INDEX)]),
        save,
    ));
    let err = acquirer(state.path(), fake)
        .acquire(&request(), &mut |_| {})
        .unwrap_err();
    assert_eq!(
        tmp_entries(state.path()),
        0,
        "staging left behind for {err}"
    );
    err
}

#[test]
fn missing_symlinked_malformed_and_oversized_exports_are_rejected() {
    assert!(export_error(Save::Nothing)
        .to_string()
        .contains("wrote no file"));
    assert!(export_error(Save::Symlink)
        .to_string()
        .contains("not a regular file"));
    assert!(matches!(
        export_error(Save::Write(b"not a tar at all".to_vec())),
        EngineError::ImageArchiveRejected { .. }
    ));
    let (bytes, _) = image();
    let truncated = bytes[..bytes.len() / 2].to_vec();
    assert!(matches!(
        export_error(Save::Write(truncated)),
        EngineError::ImageArchiveRejected { .. }
    ));
    let huge = vec![0u8; (limits().max_archive_bytes + 1) as usize];
    assert!(export_error(Save::Write(huge))
        .to_string()
        .contains("limit"));
}

#[test]
fn a_wrong_platform_export_is_rejected() {
    let amd = crate::data::oci_identity::OciPlatform {
        os: "linux".into(),
        architecture: "amd64".into(),
        variant: None,
    };
    let (bytes, _) = oci_archive(&amd, &[layer_tar(&[L::File("a", b"x", 0o644)])]);
    assert!(matches!(
        export_error(Save::Write(bytes)),
        EngineError::ImagePlatformMismatch { .. }
    ));
}

#[test]
fn a_single_manifest_descriptor_must_match_the_exported_manifest() {
    let state = tempfile::tempdir().unwrap();
    let (bytes, _) = image();
    let fake = Arc::new(Fake::new(
        serde_json::json!([entry("awman-x-claude:latest", &digest('a'), MANIFEST)]),
        Save::Write(bytes),
    ));
    let err = acquirer(state.path(), fake)
        .acquire(&request(), &mut |_| {})
        .unwrap_err();
    assert!(
        matches!(err, EngineError::ImageDigestMismatch { .. }),
        "{err}"
    );
}

#[test]
fn peer_errors_are_classified_and_sanitized() {
    let err = export_error(Save::Error(
        "notFound",
        "no image at /Users/x/private\u{7}".into(),
    ));
    match err {
        EngineError::Config(m) => assert!(!m.contains('\u{7}'), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        export_error(Save::Error("internalError", "boom".into())),
        EngineError::Container(_)
    ));
    assert!(matches!(
        export_error(Save::Error("interrupted", "restart".into())),
        EngineError::Network(_)
    ));
}

#[test]
fn staging_paths_never_reach_error_text() {
    let state = tempfile::tempdir().unwrap();
    // The helper echoes the path it was given in its error message.
    let echo = Fake::new(
        serde_json::json!([entry("awman-x-claude:latest", &digest('a'), INDEX)]),
        Save::Nothing,
    );
    struct Echo(Fake);
    impl XpcTransport for Echo {
        fn send(
            &self,
            service: &str,
            request: &[(&str, XpcValue)],
            keys: &[(&str, XpcKind)],
            wait: &WaitControl<'_>,
        ) -> Result<XpcReply, XpcFailure> {
            let out = request.iter().find_map(|(k, v)| match v {
                XpcValue::String(s) if *k == CONTRACT.file_path_key => Some(s.clone()),
                _ => None,
            });
            match out {
                Some(out) => {
                    let body = serde_json::json!({"code": "internalError",
                        "message": format!("cannot write {out}")});
                    let mut r = XpcReply::new();
                    r.insert(
                        CONTRACT.error_key.into(),
                        XpcValue::Data(body.to_string().into_bytes()),
                    );
                    Ok(r)
                }
                None => self.0.send(service, request, keys, wait),
            }
        }
    }
    let acq = CachingAcquirer::new(
        state.path(),
        limits(),
        EnvSnapshot::empty(),
        Arc::new(FixedDisk(None)),
    )
    .with_retry(RetryPolicy::NONE)
    .with_apple_transport(Some(Arc::new(Echo(echo))));
    let err = acq
        .acquire(&request(), &mut |_| {})
        .unwrap_err()
        .to_string();
    assert!(err.contains("<staging>"), "{err}");
    assert!(!err.contains(&state.path().display().to_string()), "{err}");
}

#[test]
fn an_interrupted_helper_is_retried_then_succeeds() {
    struct Flaky {
        inner: Fake,
        failures: Mutex<u32>,
    }
    impl XpcTransport for Flaky {
        fn send(
            &self,
            service: &str,
            request: &[(&str, XpcValue)],
            keys: &[(&str, XpcKind)],
            wait: &WaitControl<'_>,
        ) -> Result<XpcReply, XpcFailure> {
            let is_save = request
                .iter()
                .any(|(_, v)| *v == XpcValue::String("imageSave".into()));
            let mut left = self.failures.lock().unwrap();
            if is_save && *left > 0 {
                *left -= 1;
                return Err(XpcFailure::Interrupted);
            }
            drop(left);
            self.inner.send(service, request, keys, wait)
        }
    }
    let state = tempfile::tempdir().unwrap();
    let (bytes, _) = image();
    let flaky = Arc::new(Flaky {
        inner: Fake::new(
            serde_json::json!([entry("awman-x-claude:latest", &digest('a'), INDEX)]),
            Save::Write(bytes),
        ),
        failures: Mutex::new(1),
    });
    let acq = CachingAcquirer::new(
        state.path(),
        limits(),
        EnvSnapshot::empty(),
        Arc::new(FixedDisk(None)),
    )
    .with_retry(RetryPolicy {
        max_attempts: 2,
        initial_backoff: Duration::ZERO,
        ..RetryPolicy::DEFAULT
    })
    .with_sleeper(Arc::new(|_| {}))
    .with_apple_transport(Some(flaky.clone()));
    acq.acquire(&request(), &mut |_| {}).unwrap();
    assert_eq!(*flaky.failures.lock().unwrap(), 0);
}

#[test]
fn cancellation_reaches_an_outstanding_export() {
    let state = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::new(
        serde_json::json!([entry("awman-x-claude:latest", &digest('a'), INDEX)]),
        Save::Hang,
    ));
    let acq = acquirer(state.path(), fake);
    let token = acq.cancel_token();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        token.cancel();
    });
    let err = acq.acquire(&request(), &mut |_| {}).unwrap_err();
    canceller.join().unwrap();
    assert!(crate::engine::oci::retry::is_cancelled(&err), "{err}");
    assert_eq!(tmp_entries(state.path()), 0);
}

#[test]
fn a_deadline_bounds_an_outstanding_export() {
    let state = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::new(
        serde_json::json!([entry("awman-x-claude:latest", &digest('a'), INDEX)]),
        Save::Hang,
    ));
    let err = acquirer(state.path(), fake)
        .with_retry(RetryPolicy {
            deadline: Duration::from_millis(100),
            ..RetryPolicy::NONE
        })
        .acquire(&request(), &mut |_| {})
        .unwrap_err();
    assert!(err.to_string().contains("deadline"), "{err}");
}

#[test]
fn a_refresh_failure_keeps_the_committed_image() {
    let state = tempfile::tempdir().unwrap();
    let (bytes, _) = image();
    let good = Arc::new(Fake::new(
        serde_json::json!([entry("awman-x-claude:latest", &digest('a'), INDEX)]),
        Save::Write(bytes),
    ));
    let first = acquirer(state.path(), good)
        .acquire(&request(), &mut |_| {})
        .unwrap();
    let broken = Arc::new(Fake::new(
        serde_json::json!([entry("awman-x-claude:latest", &digest('a'), INDEX)]),
        Save::Fail(XpcFailure::Interrupted),
    ));
    let refresh = AcquireRequest {
        policy: AcquirePolicy::Refresh,
        ..request()
    };
    assert!(acquirer(state.path(), broken.clone())
        .acquire(&refresh, &mut |_| {})
        .is_err());
    let cached = acquirer(state.path(), broken)
        .acquire(
            &AcquireRequest {
                policy: AcquirePolicy::CachedOnly,
                ..request()
            },
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(cached.identity, first.identity);
    assert!(first.archive.exists());
}

#[test]
fn without_a_transport_the_source_is_blocked() {
    match no_transport() {
        EngineError::ImageSourceBlocked { reason, .. } => {
            assert!(
                reason.contains("does not run the `container` CLI"),
                "{reason}"
            )
        }
        other => panic!("{other:?}"),
    }
}
