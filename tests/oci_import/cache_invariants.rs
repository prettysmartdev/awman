//! Cached-execution invariants of the verified archive cache, through the
//! public acquirer only:
//!
//! * a changed endpoint, path, reference or platform is a different cache
//!   key and is never served from another entry;
//! * source deletion leaves cached use intact, a replaced archive is
//!   re-imported, shared bytes keep one archive with per-reference identity;
//! * a corrupted archive is a miss (actionable error under `CachedOnly`,
//!   silent re-acquisition under `IfMissing`), and a record from before the
//!   config metadata existed is migrated by exactly one re-acquisition;
//! * the SDK projection of a multi-image archive holds only the selected
//!   image and tag.

use std::path::{Path, PathBuf};

use awman::data::config::image_source::ImageSourceSpec;
use awman::data::oci_identity::OciPlatform;
use awman::engine::error::EngineError;
use awman::engine::oci::archive::prepare_runtime_archive;
use awman::engine::oci::{
    cached_image_config, prune_cache, validate_archive, AcquirePolicy, AcquireRequest,
    ImageAcquirer,
};

use crate::support::*;

fn image(name: &str) -> Image {
    Image::new(OciPlatform::host_linux(), realistic_layers(), name)
}

fn archive_request(tag: &str, path: &Path, policy: AcquirePolicy) -> AcquireRequest {
    AcquireRequest {
        tag: tag.into(),
        source: ImageSourceSpec::Archive {
            path: path.to_path_buf(),
        },
        platform: OciPlatform::host_linux(),
        policy,
        registries: Default::default(),
    }
}

fn docker_request(
    tag: &str,
    host: &str,
    reference: Option<&str>,
    policy: AcquirePolicy,
) -> AcquireRequest {
    AcquireRequest {
        tag: tag.into(),
        source: ImageSourceSpec::DockerStore {
            host: Some(host.into()),
            tls: None,
            reference: reference.map(str::to_string),
        },
        platform: OciPlatform::host_linux(),
        policy,
        registries: Default::default(),
    }
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn host_os_arch() -> (&'static str, &'static str) {
    match OciPlatform::host_linux().architecture.as_str() {
        "amd64" => ("linux", "amd64"),
        _ => ("linux", "arm64"),
    }
}

fn is_cache_miss(err: &EngineError) -> bool {
    matches!(err, EngineError::Container(m) if m.contains("not in the builtin image cache") && m.contains("awman ready"))
}

#[test]
fn changed_path_reference_endpoint_or_platform_is_never_served_from_another_entry() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let bytes = docker_save(&[image("agent:1")]);
    let a = write(temp.path(), "a.tar", &bytes);
    let acq = acquirer(&state);
    let got = acq
        .acquire(
            &archive_request("agent:1", &a, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();

    // Same bytes at another path: not cached under that path.
    let b = write(temp.path(), "b.tar", &bytes);
    let err = acq
        .acquire(
            &archive_request("agent:1", &b, AcquirePolicy::CachedOnly),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(is_cache_miss(&err), "{err:?}");
    assert!(
        err.to_string().contains("b.tar"),
        "the error names the configured locator"
    );

    // Another platform for the same path: a miss, not the cached image.
    let mut foreign = archive_request("agent:1", &a, AcquirePolicy::CachedOnly);
    foreign.platform = other_platform(&OciPlatform::host_linux());
    assert!(is_cache_miss(
        &acq.acquire(&foreign, &mut |_| {}).unwrap_err()
    ));

    // The same bytes from a Docker endpoint are a different entry; a second
    // endpoint or reference is different again.
    let engine = FakeEngine::start(
        temp.path(),
        engine_script("1.45", host_os_arch(), "agent:1", bytes.clone(), |_, a| {
            Reply::ok(a)
        }),
    );
    let miss = acq
        .acquire(
            &docker_request("agent:1", &engine.host(), None, AcquirePolicy::CachedOnly),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(is_cache_miss(&miss));
    let from_engine = acq
        .acquire(
            &docker_request("agent:1", &engine.host(), None, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(
        from_engine.identity.manifest_digest,
        got.identity.manifest_digest
    );
    assert_ne!(from_engine.identity.source, got.identity.source);
    assert_eq!(
        cached_archives(&state),
        1,
        "identical bytes are stored once"
    );

    let other_dir = temp.path().join("other");
    std::fs::create_dir(&other_dir).unwrap();
    let other_engine = FakeEngine::start(
        &other_dir,
        engine_script("1.45", host_os_arch(), "agent:1", bytes.clone(), |_, a| {
            Reply::ok(a)
        }),
    );
    assert!(is_cache_miss(
        &acq.acquire(
            &docker_request(
                "agent:1",
                &other_engine.host(),
                None,
                AcquirePolicy::CachedOnly
            ),
            &mut |_| {},
        )
        .unwrap_err()
    ));
    assert!(is_cache_miss(
        &acq.acquire(
            &docker_request(
                "agent:1",
                &engine.host(),
                Some("agent:2"),
                AcquirePolicy::CachedOnly
            ),
            &mut |_| {},
        )
        .unwrap_err()
    ));
    assert_eq!(engine.exports(), 1, "cached-only never contacts the daemon");
    assert_eq!(other_engine.exports(), 0);
}

#[test]
fn source_deletion_replacement_and_shared_bytes_keep_per_reference_identity() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let one = docker_save(&[image("one:1")]);
    let a = write(temp.path(), "a.tar", &one);
    let b = write(temp.path(), "b.tar", &one);
    let acq = acquirer(&state);
    let from_a = acq
        .acquire(
            &archive_request("one:1", &a, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();
    let from_b = acq
        .acquire(
            &archive_request("one:1", &b, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(from_a.archive, from_b.archive, "shared bytes, one archive");
    assert_eq!(cached_archives(&state), 1);
    assert!(cached_image_config(&state, &from_a.archive)
        .unwrap()
        .is_some());

    // Deleting a source leaves cached use for that source intact.
    std::fs::remove_file(&a).unwrap();
    assert_eq!(
        acq.acquire(
            &archive_request("one:1", &a, AcquirePolicy::CachedOnly),
            &mut |_| {}
        )
        .unwrap(),
        from_a
    );
    // IfMissing with the source gone still serves the cache (the source is
    // not needed to decide it is unchanged).
    assert_eq!(
        acq.acquire(
            &archive_request("one:1", &a, AcquirePolicy::IfMissing),
            &mut |_| {}
        )
        .unwrap(),
        from_a
    );

    // Replacing b's bytes re-imports b; a's entry is untouched.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let two = docker_save(&[Image::new(
        OciPlatform::host_linux(),
        vec![layer(&[Entry::file("bin/other", b"2", 0o755)])],
        "one:1",
    )]);
    std::fs::write(&b, &two).unwrap();
    let replaced = acq
        .acquire(
            &archive_request("one:1", &b, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();
    assert_ne!(
        replaced.identity.manifest_digest,
        from_b.identity.manifest_digest
    );
    assert_eq!(cached_archives(&state), 2);
    assert_eq!(
        acq.acquire(
            &archive_request("one:1", &a, AcquirePolicy::CachedOnly),
            &mut |_| {}
        )
        .unwrap(),
        from_a
    );

    // Pruning to the identities still in use removes only the unreferenced
    // archive.
    drop(from_a);
    drop(from_b);
    let report = prune_cache(
        &state,
        std::slice::from_ref(&replaced.identity.manifest_digest),
    )
    .unwrap();
    assert_eq!(report.archives_removed, 1);
    assert_eq!(cached_archives(&state), 1);
    assert!(is_cache_miss(
        &acq.acquire(
            &archive_request("one:1", &a, AcquirePolicy::CachedOnly),
            &mut |_| {}
        )
        .unwrap_err()
    ));
    assert_eq!(
        acq.acquire(
            &archive_request("one:1", &b, AcquirePolicy::CachedOnly),
            &mut |_| {}
        )
        .unwrap(),
        replaced
    );
}

#[test]
fn acquisition_holds_lease_before_publication_callback_and_through_cloned_handoff() {
    const CHILD_STATE: &str = "AWMAN_TEST_LEASE_PRUNE_STATE";
    if let Some(state) = std::env::var_os(CHILD_STATE) {
        let report = prune_cache(Path::new(&state), &[]).unwrap();
        assert_eq!(report.archives_removed, 0);
        assert_eq!(report.archives_in_use, 1);
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let source = write(temp.path(), "source.tar", &docker_save(&[image("agent:1")]));
    let acq = acquirer(&state);
    let prune_in_child = || {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "cache_invariants::acquisition_holds_lease_before_publication_callback_and_through_cloned_handoff", "--nocapture"])
            .env(CHILD_STATE, &state)
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    for policy in [AcquirePolicy::IfMissing, AcquirePolicy::CachedOnly] {
        let mut callbacks = 0;
        let acquired = acq
            .acquire(&archive_request("agent:1", &source, policy), &mut |event| {
                if matches!(event, awman::engine::oci::AcquireProgress::Cached { .. }) {
                    // Block the real acquirer at its final callback, before its
                    // consumer could acquire a lease. A separate process prunes.
                    prune_in_child();
                    callbacks += 1;
                }
            })
            .unwrap();
        assert_eq!(callbacks, 1);
        let consumer = acquired.clone();
        drop(acquired);
        prune_in_child();
        let (_dir, projected) = prepare_runtime_archive(&consumer, "runtime:1", &limits()).unwrap();
        assert!(projected.is_file());
        drop(consumer);
        if policy == AcquirePolicy::IfMissing {
            std::fs::remove_file(&source).unwrap();
        }
    }
    assert_eq!(prune_cache(&state, &[]).unwrap().archives_removed, 1);
}

#[test]
fn a_corrupted_cached_archive_is_a_miss_and_is_reacquired_once() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let bytes = docker_save(&[image("agent:1")]);
    let a = write(temp.path(), "a.tar", &bytes);
    let acq = acquirer(&state);
    let got = acq
        .acquire(
            &archive_request("agent:1", &a, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();

    // Flip one byte inside the cached archive without changing its length.
    let mut cached = std::fs::read(&got.archive).unwrap();
    let middle = cached.len() / 2;
    cached[middle] ^= 0xff;
    std::fs::write(&got.archive, &cached).unwrap();

    let err = acq
        .acquire(
            &archive_request("agent:1", &a, AcquirePolicy::CachedOnly),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(is_cache_miss(&err), "{err:?}");

    let again = acq
        .acquire(
            &archive_request("agent:1", &a, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(again.identity, got.identity);
    assert_eq!(std::fs::read(&again.archive).unwrap(), bytes);
    assert_eq!(
        acq.acquire(
            &archive_request("agent:1", &a, AcquirePolicy::CachedOnly),
            &mut |_| {}
        )
        .unwrap(),
        again
    );
}

#[test]
fn a_pre_metadata_cache_record_is_migrated_by_one_reacquisition() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let bytes = docker_save(&[image("agent:1")]);
    let a = write(temp.path(), "a.tar", &bytes);
    let acq = acquirer(&state);
    let got = acq
        .acquire(
            &archive_request("agent:1", &a, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();

    // Rewrite every record the way a cache from before `config` metadata
    // existed would have written it: no `config` field at all.
    let strip = |path: &Path| {
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        if let Some(obj) = json.as_object_mut() {
            obj.remove("config");
            if let Some(record) = obj.get_mut("record").and_then(|r| r.as_object_mut()) {
                record.remove("config");
            }
        }
        std::fs::write(path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
    };
    strip(&got.archive.with_extension("json"));
    for entry in std::fs::read_dir(cache_dir(&state).join("refs")).unwrap() {
        strip(&entry.unwrap().path());
    }
    assert!(cached_image_config(&state, &got.archive).unwrap().is_none());

    // Cached-only cannot serve an unreadable record: an actionable miss.
    let err = acq
        .acquire(
            &archive_request("agent:1", &a, AcquirePolicy::CachedOnly),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(is_cache_miss(&err), "{err:?}");

    // One re-acquisition rewrites the records; afterwards it is a plain hit.
    let migrated = acq
        .acquire(
            &archive_request("agent:1", &a, AcquirePolicy::IfMissing),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(migrated.identity, got.identity);
    assert!(cached_image_config(&state, &migrated.archive)
        .unwrap()
        .is_some_and(|c| c.home.as_deref() == Some("/home/agent")));
    std::fs::remove_file(&a).unwrap();
    assert_eq!(
        acq.acquire(
            &archive_request("agent:1", &a, AcquirePolicy::CachedOnly),
            &mut |_| {}
        )
        .unwrap(),
        migrated
    );
    assert_eq!(cached_archives(&state), 1);
}

#[test]
fn unselected_images_and_tags_never_reach_the_runtime_projection() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let mut wanted = image("selected:latest");
    wanted.names.push("unrelated:latest".into());
    let other = Image::new(
        OciPlatform::host_linux(),
        vec![layer(&[Entry::file("bin/other", b"other", 0o755)])],
        "other:latest",
    );
    for (name, bytes) in [
        ("save.tar", docker_save(&[other.clone(), wanted.clone()])),
        (
            "oci.tar",
            oci_layout(&[other.clone(), wanted.clone()], true, false).bytes,
        ),
    ] {
        let path = write(temp.path(), name, &bytes);
        let acq = acquirer(&state);
        let got = acq
            .acquire(
                &archive_request("selected:latest", &path, AcquirePolicy::IfMissing),
                &mut |_| {},
            )
            .unwrap();
        let (_dir, projected) =
            prepare_runtime_archive(&got, "awman-runtime:latest", &limits()).unwrap();
        let text = String::from_utf8_lossy(&std::fs::read(&projected).unwrap()).into_owned();
        assert!(text.contains("awman-runtime:latest"), "{name}");
        assert!(
            !text.contains("unrelated:latest"),
            "{name}: unrelated tag dropped"
        );
        assert!(
            !text.contains("other:latest"),
            "{name}: other image dropped"
        );
        assert!(
            !text.contains("bin/other"),
            "{name}: other image's layer absent"
        );
        let checked = validate_archive(
            &projected,
            &OciPlatform::host_linux(),
            &[],
            &limits(),
            &mut |_| {},
        )
        .expect("a projection is unambiguous without any wanted refs");
        assert_eq!(checked.config_digest, got.identity.config_digest);
        // The archive without a selecting reference is ambiguous: the
        // original bytes are never what the SDK sees.
        assert!(validate_archive(
            &path,
            &OciPlatform::host_linux(),
            &[],
            &limits(),
            &mut |_| {}
        )
        .is_err());
    }
}
