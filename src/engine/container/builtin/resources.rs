//! Per-launch CPU and memory for a session VM.
//!
//! A vCPU count is a whole number of virtual CPUs the guest sees, not a share
//! of host CPU time: the runtime has no fractional quota, so fractional,
//! non-finite and out-of-range requests are refused rather than rounded.
//! Guest memory is a hard ceiling enforced by the VMM: exceeding it triggers
//! the guest kernel's OOM killer inside the VM, never host memory pressure
//! beyond the configured size.
use crate::data::config::builtin_runtime::MIN_MEMORY_MIB;
use crate::engine::container::options::{CpuLimit, MemoryLimit};
use crate::engine::error::EngineError;

/// Resolve a launch's `(vcpus, memory MiB)` from its optional limits and the
/// configured defaults.
pub fn resolve(
    cpu: Option<CpuLimit>,
    memory: Option<MemoryLimit>,
    defaults: (u8, u32),
) -> Result<(u8, u32), EngineError> {
    let cpus = cpu.map(|c| c.0).unwrap_or(f64::from(defaults.0));
    if !cpus.is_finite() || cpus.fract() != 0.0 || !(1.0..=255.0).contains(&cpus) {
        return Err(EngineError::UnsupportedResourceRequest {
            runtime: "builtin",
            request: "CPU limit".into(),
            reason:
                "requires an integer vCPU allocation in 1..=255; fractional quotas are unavailable"
                    .into(),
        });
    }
    let memory = memory.map(|m| m.0).unwrap_or(u64::from(defaults.1));
    let memory = u32::try_from(memory)
        .ok()
        .filter(|m| *m >= MIN_MEMORY_MIB)
        .ok_or_else(|| EngineError::UnsupportedResourceRequest {
            runtime: "builtin",
            request: "memory limit".into(),
            reason: format!("requires {MIN_MEMORY_MIB}..={} MiB", u32::MAX),
        })?;
    Ok((cpus as u8, memory))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_fractional_nonfinite_and_overflowing_resources() {
        for cpu in [0.0, -1.0, 1.5, f64::NAN, f64::INFINITY, 256.0] {
            assert!(resolve(Some(CpuLimit(cpu)), None, (2, 4096)).is_err());
        }
        assert!(resolve(None, Some(MemoryLimit(u64::MAX)), (2, 4096)).is_err());
        assert_eq!(
            resolve(Some(CpuLimit(3.0)), None, (2, 4096)).unwrap(),
            (3, 4096)
        );
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    #[test]
    fn integer_vcpus_and_memory_are_accepted_exactly_at_their_bounds() {
        assert_eq!(
            resolve(Some(CpuLimit(1.0)), Some(MemoryLimit(128)), (2, 4096)).unwrap(),
            (1, 128)
        );
        assert_eq!(
            resolve(
                Some(CpuLimit(255.0)),
                Some(MemoryLimit(u64::from(u32::MAX))),
                (2, 4096)
            )
            .unwrap(),
            (255, u32::MAX)
        );
        assert!(resolve(Some(CpuLimit(256.0)), None, (2, 4096)).is_err());
        assert!(resolve(None, Some(MemoryLimit(127)), (2, 4096)).is_err());
        assert_eq!(
            resolve(None, None, (4, 2048)).unwrap(),
            (4, 2048),
            "config defaults apply"
        );
    }

    #[test]
    fn a_fractional_request_is_rejected_not_rounded() {
        for cpu in [0.25, 0.5, 1.5, 2.000001] {
            let error = resolve(Some(CpuLimit(cpu)), None, (2, 4096)).unwrap_err();
            assert!(
                matches!(&error, EngineError::UnsupportedResourceRequest { runtime: "builtin", reason, .. } if reason.contains("fractional")),
                "{cpu}: {error:?}"
            );
        }
    }
}
