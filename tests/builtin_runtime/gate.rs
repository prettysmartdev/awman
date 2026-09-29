//! Hardware gating for `builtin_hw_*` tests.
//!
//! A missing prerequisite is reported as SKIP (gate off) or BLOCKED (gate on
//! but the machine cannot boot a guest) and recorded in the optional TSV named
//! by `AWMAN_TEST_BUILTIN_REPORT`. With `AWMAN_TEST_BUILTIN_REQUIRE_HW=1` a hardware job turns
//! SKIP/BLOCKED into a failure, so it can never go green without a guest.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run,
    Skip(String),
    Blocked(String),
}

pub struct Facts {
    pub gate_on: bool,
    pub hypervisor: Result<(), String>,
    pub binary_has_builtin: bool,
    pub driver_built: bool,
    pub archive: Option<String>,
}

/// Pure policy: what to do given the facts about this host.
pub fn decide(facts: &Facts, needs_archive: bool) -> Decision {
    if !facts.gate_on {
        return Decision::Skip("AWMAN_TEST_BUILTIN=1 is not set (opt-in hardware gate)".into());
    }
    if let Err(reason) = &facts.hypervisor {
        return Decision::Blocked(reason.clone());
    }
    if !facts.binary_has_builtin {
        return Decision::Blocked(
            "the awman binary was built without the builtin runtime; run `make test-builtin` \
             (cargo test --features builtin-runtime)"
                .into(),
        );
    }
    if !facts.driver_built {
        return Decision::Blocked(
            "the guest driver is not built; run `make test-builtin` or `cargo build --features \
             builtin-runtime --example builtin_hw_driver`"
                .into(),
        );
    }
    if needs_archive && facts.archive.is_none() {
        return Decision::Blocked(
            "AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE must name the promoted fixture image (OCI layout \
             tar produced by tools/oci-runtime-spike/fixture)"
                .into(),
        );
    }
    Decision::Run
}

fn truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Linux: `/dev/kvm` must be openable read/write. macOS: Apple Silicon with
/// Hypervisor.framework support. Actual boot verifies OS permission to run a VM.
pub fn hypervisor(binary: &Path) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let _ = binary;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .map(|_| ())
            .map_err(|e| format!("/dev/kvm is not usable ({e}); KVM access is required"))
    }
    #[cfg(target_os = "macos")]
    {
        if std::env::consts::ARCH != "aarch64" {
            return Err("Apple Silicon is required".into());
        }
        let hv = std::process::Command::new("sysctl")
            .args(["-n", "kern.hv_support"])
            .output()
            .map_err(|e| format!("cannot query kern.hv_support: {e}"))?;
        if String::from_utf8_lossy(&hv.stdout).trim() != "1" {
            return Err("Hypervisor.framework is not supported here (kern.hv_support != 1)".into());
        }
        let _ = binary;
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = binary;
        Err("unsupported platform for the builtin runtime".into())
    }
}

pub fn facts(binary: &Path) -> Facts {
    Facts {
        gate_on: truthy("AWMAN_TEST_BUILTIN"),
        hypervisor: hypervisor(binary),
        binary_has_builtin: super::binary::has_builtin_runtime(),
        driver_built: super::hardware::driver_path().is_file(),
        archive: super::hardware::fixture_archive().map(|p| p.to_string_lossy().into_owned()),
    }
}

fn record(test: &str, outcome: &str, detail: &str) {
    eprintln!("{outcome}: {test}: {detail}");
    if let Ok(path) = std::env::var("AWMAN_TEST_BUILTIN_REPORT") {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{outcome}\t{test}\t{}", detail.replace('\n', " "));
        }
    }
}

/// Returns true when the calling hardware test may boot a guest. Otherwise it
/// has already reported SKIP/BLOCKED (or panicked under REQUIRE_HW) and the
/// caller must return without asserting anything about a guest.
pub fn hardware_or_skip(test: &str, needs_archive: bool) -> bool {
    // Check the host for the driver that will boot the guest. Actual boot
    // remains mandatory; these preflights do not prove OS access.
    let binary = super::hardware::driver_path();
    match decide(&facts(&binary), needs_archive) {
        Decision::Run => {
            record(test, "RUN", "prerequisites satisfied");
            true
        }
        Decision::Skip(reason) => {
            record(test, "SKIP", &reason);
            require_hw_panics(test, &reason);
            false
        }
        Decision::Blocked(reason) => {
            record(test, "BLOCKED", &reason);
            require_hw_panics(test, &reason);
            false
        }
    }
}

fn require_hw_panics(test: &str, reason: &str) {
    if truthy("AWMAN_TEST_BUILTIN_REQUIRE_HW") {
        panic!("{test}: hardware is required (AWMAN_TEST_BUILTIN_REQUIRE_HW=1) but: {reason}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(gate: bool, hv: Result<(), &str>, builtin: bool, archive: bool) -> Facts {
        facts_with_driver(gate, hv, builtin, true, archive)
    }

    fn facts_with_driver(
        gate: bool,
        hv: Result<(), &str>,
        builtin: bool,
        driver: bool,
        archive: bool,
    ) -> Facts {
        Facts {
            gate_on: gate,
            hypervisor: hv.map_err(str::to_owned),
            binary_has_builtin: builtin,
            driver_built: driver,
            archive: archive.then(|| "/tmp/x.tar".into()),
        }
    }

    #[test]
    fn gate_off_is_a_skip_even_on_capable_hardware() {
        assert!(matches!(
            decide(&facts(false, Ok(()), true, true), true),
            Decision::Skip(_)
        ));
    }

    #[test]
    fn gate_on_without_hypervisor_is_blocked_never_run() {
        assert!(matches!(
            decide(&facts(true, Err("no kvm"), true, true), true),
            Decision::Blocked(r) if r == "no kvm"
        ));
    }

    #[test]
    fn a_binary_without_the_runtime_or_a_missing_archive_is_blocked() {
        assert!(matches!(
            decide(&facts(true, Ok(()), false, true), true),
            Decision::Blocked(_)
        ));
        assert!(matches!(
            decide(&facts(true, Ok(()), true, false), true),
            Decision::Blocked(_)
        ));
        assert_eq!(
            decide(&facts(true, Ok(()), true, false), false),
            Decision::Run
        );
    }

    #[test]
    fn a_missing_guest_driver_is_blocked() {
        assert!(matches!(
            decide(&facts_with_driver(true, Ok(()), true, false, true), true),
            Decision::Blocked(r) if r.contains("driver")
        ));
    }

    #[test]
    fn only_a_fully_capable_host_runs() {
        assert_eq!(
            decide(&facts(true, Ok(()), true, true), true),
            Decision::Run
        );
    }
}
