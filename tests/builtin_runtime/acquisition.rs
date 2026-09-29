//! Acquisition meets execution: an image exported from a (disposable) source
//! daemon is cached by awman's verified archive cache, the daemon is then
//! stopped, and the cached bytes boot a real guest with an empty host
//! environment (no PATH, no HOME of the developer, no source configuration).
//!
//! `builtin_hw_*` here follow the hardware gate in `gate.rs`: without
//! `AWMAN_TEST_BUILTIN=1`, KVM/HVF, a feature build, the driver and a fixture
//! image they report SKIP/BLOCKED and pass nothing.
//!
//! What this proves and does not prove: the guest runs from the cache after
//! the *source* is gone and the driver has no PATH; enforced network denial
//! for the guest itself is the network-policy suite's contract (WI 0121 D),
//! and dispatch through the `awman` entrypoint is the lifecycle suite's.

#[path = "../oci_import/support.rs"]
mod oci_support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use awman::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
use awman::data::oci_identity::OciPlatform;
use awman::engine::error::EngineError;
use awman::engine::oci::{AcquirePolicy, AcquireRequest, ImageAcquirer};

use crate::gate::hardware_or_skip;
use crate::hardware::{driver_path, fixture_archive};
use oci_support::{acquirer, engine_script, FakeEngine, Reply};

const TAG: &str = "awman-hw/cached:latest";
const REFERENCE: &str = "awman-hw-fixture:latest";

fn host_os_arch() -> (&'static str, &'static str) {
    match OciPlatform::host_linux().architecture.as_str() {
        "amd64" => ("linux", "amd64"),
        _ => ("linux", "arm64"),
    }
}

struct Run {
    facts: BTreeMap<String, Vec<String>>,
    stdout: String,
    stderr: String,
    code: i32,
    raw: String,
}

/// Run one driver scenario against `archive` with an empty environment.
fn run_driver(test: &str, scenario: &str, archive: &Path) -> Option<Run> {
    let state = tempfile::Builder::new()
        .prefix("awb.")
        .tempdir_in("/tmp")
        .unwrap();
    let work = tempfile::Builder::new()
        .prefix("awman-hw-acq-")
        .tempdir_in("/tmp")
        .unwrap();
    let output = Command::new(driver_path())
        .env_clear() // no PATH: nothing on the host may be looked up
        .env("HOME", work.path().join("home"))
        .env("TMPDIR", work.path())
        .args(["--scenario", scenario, "--state-dir"])
        .arg(state.path().join("s"))
        .arg("--archive")
        .arg(archive)
        .args(["--tag", TAG, "--work"])
        .arg(work.path())
        .arg("--scripts")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/oci-runtime-spike"))
        .output()
        .expect("spawn the guest driver");
    let raw = format!(
        "{}\n--stderr--\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let code = output.status.code().unwrap_or(-1);
    if code == 77 {
        eprintln!("BLOCKED: {test}: {raw}");
        if std::env::var("AWMAN_TEST_BUILTIN_REQUIRE_HW").is_ok() {
            panic!("{test}: blocked by the driver: {raw}");
        }
        return None;
    }
    let mut facts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((key, value)) = line.split_once('\t') {
            facts
                .entry(key.to_owned())
                .or_default()
                .push(value.to_owned());
        }
    }
    Some(Run {
        facts,
        stdout: std::fs::read_to_string(work.path().join(format!("{scenario}.stdout")))
            .unwrap_or_default(),
        stderr: std::fs::read_to_string(work.path().join(format!("{scenario}.stderr")))
            .unwrap_or_default(),
        code,
        raw,
    })
}

#[test]
fn builtin_hw_cached_execution_after_sources_stop() {
    const TEST: &str = "builtin_hw_cached_execution_after_sources_stop";
    if !hardware_or_skip(TEST, true) {
        return;
    }
    let fixture = fixture_archive().expect("checked by hardware_or_skip");
    let bytes = std::fs::read(&fixture).unwrap();

    // 1. A disposable source daemon (Docker Engine double on a Unix socket)
    //    exports the fixture; awman caches it under a private state root.
    let temp = tempfile::Builder::new()
        .prefix("awb-acq.")
        .tempdir_in("/tmp")
        .unwrap();
    let engine = FakeEngine::start(
        temp.path(),
        engine_script("1.45", host_os_arch(), REFERENCE, bytes, |_, a| {
            Reply::ok(a)
        }),
    );
    let state = temp.path().join("state");
    let request = |policy| AcquireRequest {
        tag: TAG.into(),
        source: ImageSourceSpec::DockerStore {
            host: Some(engine.host()),
            tls: None,
            reference: Some(REFERENCE.into()),
        },
        platform: OciPlatform::host_linux(),
        policy,
        registries: BTreeMap::new(),
    };
    let acq = acquirer(&state);
    let cached = acq
        .acquire(&request(AcquirePolicy::IfMissing), &mut |_| {})
        .expect("export from the disposable daemon");
    assert_eq!(cached.identity.source, ImageSourceKind::DockerStore);
    assert_eq!(engine.exports(), 1);

    // 2. The source is stopped. Cached use still resolves; refreshing fails
    //    cleanly without touching the cached entry.
    engine.stop();
    assert_eq!(
        acq.acquire(&request(AcquirePolicy::CachedOnly), &mut |_| {})
            .unwrap(),
        cached
    );
    assert!(matches!(
        acq.acquire(&request(AcquirePolicy::Refresh), &mut |_| {}),
        Err(EngineError::Network(_))
    ));
    assert_eq!(
        acq.acquire(&request(AcquirePolicy::CachedOnly), &mut |_| {})
            .unwrap(),
        cached
    );

    // 3. The cached archive boots a real guest with no host PATH and no
    //    source reachable. The exit code, stdout and stderr are the guest's.
    let archive: PathBuf = cached.archive.clone();
    let Some(run) = run_driver(TEST, "exit-code", &archive) else {
        return;
    };
    assert_eq!(run.code, 0, "{}", run.raw);
    assert_eq!(
        run.facts
            .get("EXIT")
            .and_then(|v| v.first())
            .map(String::as_str),
        Some("37"),
        "the guest's real exit code is propagated: {}",
        run.raw
    );
    assert_eq!(run.stdout, "out-marker", "{}", run.raw);
    assert_eq!(run.stderr, "err-marker", "{}", run.raw);
    assert_eq!(
        run.facts.get("IMPORT").map(Vec::len),
        Some(2),
        "the second import of the cached archive was served from the cache: {}",
        run.raw
    );
    eprintln!(
        "PASS: {TEST}: cached {} executed after the source daemon stopped",
        cached.identity.manifest_digest
    );
}
