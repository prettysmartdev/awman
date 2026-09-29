//! `builtin_hw_*`: real guests, real hardware.
//!
//! Skipped by `make test-fast` (`--skip builtin_hw`) and by default everywhere.
//! Opt in with `AWMAN_TEST_BUILTIN=1` (`make test-builtin`). Every test first
//! calls `gate::hardware_or_skip`, which reports SKIP (gate off) or BLOCKED
//! (no KVM / hypervisor entitlement / no feature binary / no fixture image) and
//! returns without asserting anything about a guest - a machine that cannot
//! boot a guest never produces a passing execution test. Set
//! `AWMAN_TEST_BUILTIN_REQUIRE_HW=1` on hardware CI so SKIP/BLOCKED fail.
//!
//! The guest is driven by `examples/builtin_hw_driver` (see hw_driver.rs), which
//! runs with an EMPTY environment: no PATH, no HOME of the developer, no
//! credentials. The fixture image is the promoted spike fixture
//! (`tools/oci-runtime-spike/fixture`), supplied through
//! `AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE` or built from a docker-save base image
//! named by `AWMAN_TEST_BUILTIN_BASE_TAR`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::fixture_inventory::{
    guest_checks, image_checks, parse_transcript, problems, GUEST_COMPLETE, IMAGE_COMPLETE,
};
use crate::gate::hardware_or_skip;

pub(crate) const TAG: &str = "awman-hw/fixture:latest";

pub fn driver_path() -> PathBuf {
    // Cargo puts examples next to the binaries: target/<profile>/examples/.
    Path::new(env!("CARGO_BIN_EXE_awman"))
        .parent()
        .expect("target dir")
        .join("examples")
        .join("builtin_hw_driver")
}

/// The promoted fixture image, if one is available on this host.
pub fn fixture_archive() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let base = std::env::var("AWMAN_TEST_BUILTIN_BASE_TAR")
        .ok()
        .map(PathBuf::from)?;
    if !base.is_file() {
        return None;
    }
    // Build once per test process into a stable directory.
    static BUILT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let out = std::env::temp_dir().join(format!("awman-hw-fixture-{}", std::process::id()));
            let status = Command::new("cargo")
                .args(["run", "--locked", "--quiet", "--manifest-path"])
                .arg(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tools/oci-runtime-spike/fixture/Cargo.toml"),
                )
                .arg("--")
                .arg(&base)
                .arg(&out)
                .status()
                .ok()?;
            let archive = out.join("fixture-oci.tar");
            (status.success() && archive.is_file()).then_some(archive)
        })
        .clone()
}

/// Shared by every `builtin_hw_*` test module (`hardware.rs` plus later
/// modules such as `guest_compat.rs`): one call into the real guest driver.
pub(crate) struct Run {
    /// Scratch directory holding `<scenario>.stdout` / `.stderr` from the guest.
    work: tempfile::TempDir,
    /// `KEY -> values` from the driver's own stdout.
    facts: BTreeMap<String, Vec<String>>,
    pub(crate) code: i32,
    pub(crate) raw: String,
}

impl Run {
    pub(crate) fn guest_stdout(&self, name: &str) -> String {
        std::fs::read_to_string(self.work.path().join(format!("{name}.stdout"))).unwrap_or_default()
    }
    pub(crate) fn guest_stderr(&self, name: &str) -> String {
        std::fs::read_to_string(self.work.path().join(format!("{name}.stderr"))).unwrap_or_default()
    }
    pub(crate) fn fact(&self, key: &str) -> Option<&str> {
        self.facts
            .get(key)
            .and_then(|v| v.first())
            .map(String::as_str)
    }
}

/// Run one driver scenario with an empty environment and a short private state
/// dir. Returns `None` after reporting SKIP/BLOCKED when the host cannot run it.
pub(crate) fn scenario(test: &str, name: &str) -> Option<Run> {
    if !hardware_or_skip(test, true) {
        return None;
    }
    let archive = fixture_archive().expect("checked by hardware_or_skip");
    let state = tempfile::Builder::new()
        .prefix("awb.")
        .tempdir_in("/tmp")
        .unwrap();
    let work = tempfile::Builder::new()
        .prefix("awman-hw-")
        .tempdir_in("/tmp")
        .unwrap();
    let output = Command::new(driver_path())
        .env_clear() // no PATH: the embedded runtime must not look up any executable
        .env("HOME", work.path().join("home"))
        .env("TMPDIR", work.path())
        .args(["--scenario", name, "--state-dir"])
        .arg(state.path().join("s"))
        .arg("--archive")
        .arg(&archive)
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
        // The driver found a prerequisite the pre-check could not (e.g. KVM
        // permissions changed): still BLOCKED, never a pass.
        eprintln!("BLOCKED: {test}: {raw}");
        if std::env::var("AWMAN_TEST_BUILTIN_REQUIRE_HW").is_ok() {
            panic!("{test}: blocked by the driver: {raw}");
        }
        return None;
    }
    assert!(code == 0, "{test}: driver failed ({code}): {raw}");
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
        work,
        facts,
        code,
        raw,
    })
}

pub(crate) fn expect_clean(problems: Vec<String>, run: &Run) {
    assert!(
        problems.is_empty(),
        "{problems:#?}\n--- driver ---\n{}",
        run.raw
    );
}

#[test]
fn builtin_hw_image_defaults_hold_and_the_guest_is_not_root() {
    let Some(run) = scenario("builtin_hw_image_defaults", "image-defaults") else {
        return;
    };
    assert_eq!(run.fact("EXIT"), Some("0"), "{}", run.raw);
    let transcript = parse_transcript(&run.guest_stdout("image-defaults"));
    assert!(
        transcript.markers.contains("entrypoint-marker"),
        "the image ENTRYPOINT must run before its CMD"
    );
    // All nine promoted image assertions, including USER 1234 and the whiteouts.
    expect_clean(problems(&image_checks(), &transcript, IMAGE_COMPLETE), &run);
    assert!(
        transcript.passed.contains("image-user"),
        "non-root execution"
    );
    // The second import of the same archive was served from the cache.
    assert_eq!(run.facts["IMPORT"].len(), 2, "{}", run.raw);
    assert_eq!(run.code, 0);
}

#[test]
fn builtin_hw_runtime_overrides_win_over_image_defaults_but_never_root() {
    let Some(run) = scenario("builtin_hw_overrides", "image-overrides") else {
        return;
    };
    let lines: Vec<String> = run
        .guest_stdout("image-overrides")
        .lines()
        .map(str::to_owned)
        .collect();
    // uid, gid stay the image's non-root account; cwd/env are the explicit overrides;
    // HOME is still the image's.
    assert_eq!(
        lines,
        [
            "1234",
            "1234",
            "/tmp",
            "/home/probe",
            "runtime override",
            "added"
        ],
        "{}",
        run.raw
    );
}

#[test]
fn builtin_hw_exit_code_and_stream_separation() {
    let Some(run) = scenario("builtin_hw_exit_code", "exit-code") else {
        return;
    };
    assert_eq!(
        run.fact("EXIT"),
        Some("37"),
        "the guest's real exit code must be propagated"
    );
    assert_eq!(run.guest_stdout("exit-code"), "out-marker");
    assert_eq!(run.guest_stderr("exit-code"), "err-marker");
}

#[test]
fn builtin_hw_every_overlay_settings_and_prompt_variant_is_visible_in_the_guest() {
    let Some(run) = scenario("builtin_hw_mounts", "mounts") else {
        return;
    };
    let transcript = parse_transcript(&run.guest_stdout("mounts"));
    // All 32 promoted assertions: directory/file/skill/context overlays, ro/rw,
    // nested mounts, every SettingsMount family, file/env/AGENTS.md/add-dir
    // prompt delivery, environment, isolation and live atomic refresh.
    expect_clean(problems(&guest_checks(), &transcript, GUEST_COMPLETE), &run);
    assert!(run.guest_stderr("mounts").contains("guest-stderr-marker"));
    assert_eq!(
        run.fact("HOST_WRITEBACK"),
        Some("guest"),
        "guest writes reach the host"
    );
    assert_eq!(run.fact("EXIT"), Some("0"));
}

#[test]
fn builtin_hw_credential_refresh_reaches_every_consumer_atomically() {
    let Some(run) = scenario("builtin_hw_refresh_multi", "refresh-multi") else {
        return;
    };
    for consumer in 0..2 {
        assert_eq!(
            run.guest_stdout(&format!("refresh-multi-{consumer}"))
                .trim(),
            "seen-v2",
            "consumer {consumer} never observed the atomically replaced credential: {}",
            run.raw
        );
    }
}

#[test]
fn builtin_hw_resource_limits_are_enforced_in_the_guest() {
    let Some(run) = scenario("builtin_hw_limits", "limits") else {
        return;
    };
    let numbers = |name: &str| -> Vec<u64> {
        run.guest_stdout(name)
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    };
    // Config defaults for this scenario: 1 vCPU, 512 MiB.
    let default = numbers("limits-default");
    assert_eq!(default[0], 1, "vCPUs: {}", run.raw);
    assert!(
        default[1] <= 512 * 1024 && default[1] > 256 * 1024,
        "MemTotal kB {}",
        default[1]
    );
    // Explicit request: 2 vCPUs, 768 MiB.
    let explicit = numbers("limits");
    assert_eq!(explicit[0], 2, "{}", run.raw);
    assert!(
        explicit[1] <= 768 * 1024 && explicit[1] > 384 * 1024,
        "MemTotal kB {}",
        explicit[1]
    );
}

#[test]
fn builtin_hw_concurrent_vms_are_independent() {
    let Some(run) = scenario("builtin_hw_multi_vm", "multi-vm") else {
        return;
    };
    assert_eq!(run.fact("RUNNING_DURING"), Some("3"), "{}", run.raw);
    assert_eq!(
        run.fact("SURVIVORS"),
        Some("1:0:vm-1,2:0:vm-2"),
        "stopping one VM must not disturb the others"
    );
    assert_eq!(
        run.fact("RUNNING_AFTER"),
        Some("0"),
        "no VM may be left behind"
    );
}

#[test]
fn builtin_hw_cancel_stops_within_the_grace_and_removes_the_vm() {
    let Some(run) = scenario("builtin_hw_cancel", "cancel") else {
        return;
    };
    let seconds: u64 = run.fact("CANCEL_SECONDS").expect("timing").parse().unwrap();
    assert!(
        seconds <= 15,
        "cancel took {seconds}s (interrupt, 10s grace, then kill)"
    );
    assert_eq!(run.fact("RUNNING_AFTER"), Some("0"), "{}", run.raw);
}

#[test]
fn builtin_hw_pty_reports_the_initial_size_and_follows_resizes() {
    let Some(run) = scenario("builtin_hw_pty", "pty-resize") else {
        return;
    };
    let sizes = run.fact("PTY_SIZES").expect("sizes");
    assert!(sizes.contains("24 80"), "{sizes}");
    assert!(sizes.contains("30 100"), "{sizes}");
    assert_eq!(run.fact("EXIT"), Some("0"));
}

#[test]
fn builtin_hw_acp_frames_are_binary_clean_end_to_end() {
    let Some(run) = scenario("builtin_hw_acp", "acp-framing") else {
        return;
    };
    assert_eq!(run.fact("ACP_ECHO_EQUAL"), Some("true"), "{}", run.raw);
    assert_eq!(run.fact("EXIT"), Some("0"), "stdin EOF must end the agent");
}

#[test]
fn builtin_hw_a_second_client_can_reattach_while_the_launcher_lives() {
    let Some(run) = scenario("builtin_hw_reattach", "reattach") else {
        return;
    };
    assert_eq!(run.fact("REATTACH_BOTH_SAW"), Some("true"), "{}", run.raw);
    assert_eq!(
        run.fact("EXIT"),
        Some("5"),
        "the agent's own exit code, after a detach that never killed it"
    );
}
