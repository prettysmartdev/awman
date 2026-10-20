//! Engine-owned embedded Microsandbox backend. Unsupported builds fail closed;
//! supported builds use SDK control IPC and the binary's private worker route.

use std::sync::Arc;

use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::runtime::BuiltinRuntimeSettings;
use crate::engine::error::EngineError;

// Gating follows the actual runtime boundary. Only the modules that call the
// Microsandbox SDK or embed the verified payload (`embedded`, `msb_driver`)
// need `awman_builtin`. Planning, paths, naming, resources, network policy,
// the catalog marker, attach framing and the fake-driver lifecycle are plain
// Unix code, so their tests run in the default hermetic tier without payloads
// or the VM SDK. A default build has no production caller for them.
#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
mod backend;
#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
mod blocking;
#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
mod catalog;
#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
mod driver;
#[cfg(awman_builtin)]
mod embedded;
#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
mod exec_bridge;
#[cfg(all(test, unix))]
pub(super) use exec_bridge::conformance_launcher;
#[cfg(all(test, unix))]
mod fake;
#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
mod instance;
#[cfg(awman_builtin)]
mod msb_driver;
#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
mod naming;
#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
mod network;
#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
mod paths;
#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
mod resources;
#[cfg(all(test, unix))]
mod testing;
#[cfg(all(test, unix))]
pub(crate) use fake::{FakeBuiltin, LaunchedVm};

/// `(target_os, target_arch)` pairs the builtin engine can run on.
pub(super) const SUPPORTED_TARGETS: &[(&str, &str)] = &[
    ("linux", "x86_64"),
    ("linux", "aarch64"),
    ("macos", "aarch64"),
];

/// The subset released as `builtin-experimental`. Linux stays compiled and
/// unit-tested, but users cannot select it until its native gates pass
/// (WI 0124).
pub(super) const RELEASED_TARGETS: &[(&str, &str)] = &[("macos", "aarch64")];

/// Open the builtin backend with `settings`.
///
/// Refusals, in order:
/// 1. an ambient `MSB_*` override is set → `AmbientRuntimeOverride` (it
///    would outrank the embedded runtime);
/// 2. this OS/architecture is not released → `BuiltinTargetUnreleased`;
/// 3. this build does not contain the runtime → `BuiltinRuntimeUnavailable`.
pub(super) fn open(
    settings: BuiltinRuntimeSettings,
) -> Result<Arc<dyn ContainerBackend>, EngineError> {
    if let Some(variable) = settings.ambient_overrides.first() {
        return Err(EngineError::AmbientRuntimeOverride {
            variable: (*variable).to_string(),
        });
    }
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    if !is_released_target(os, arch) {
        return Err(EngineError::BuiltinTargetUnreleased {
            os: os.to_string(),
            arch: arch.to_string(),
        });
    }
    #[cfg(awman_builtin)]
    {
        if settings.test_isolation {
            return Err(EngineError::BuiltinRuntimeUnavailable {
                reason: "disabled under test isolation".into(),
            });
        }
        let paths = paths::BuiltinPaths::resolve(&settings.state_dir)?;
        let driver = msb_driver::MsbDriver::open(paths.clone(), &settings.ambient_overrides)?;
        Ok(Arc::new(backend::BuiltinBackend {
            driver,
            paths,
            settings,
            owner: uuid::Uuid::new_v4().to_string(),
            background_env: std::sync::Mutex::new(Default::default()),
        }))
    }
    #[cfg(not(awman_builtin))]
    Err(EngineError::BuiltinRuntimeUnavailable {
        reason: "this build of awman does not include the builtin-runtime feature".into(),
    })
}

/// Whether `(os, arch)` is one of [`SUPPORTED_TARGETS`].
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn is_supported_target(os: &str, arch: &str) -> bool {
    SUPPORTED_TARGETS.contains(&(os, arch))
}

/// Whether `(os, arch)` is one of [`RELEASED_TARGETS`].
pub(super) fn is_released_target(os: &str, arch: &str) -> bool {
    RELEASED_TARGETS.contains(&(os, arch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(ambient: Vec<&'static str>) -> BuiltinRuntimeSettings {
        BuiltinRuntimeSettings {
            state_dir: "/tmp/awman-builtin-test".into(),
            ambient_overrides: ambient,
            test_isolation: true,
        }
    }

    #[test]
    fn ambient_override_is_refused_first() {
        match open(settings(vec!["MSB_PATH", "MSB_HOME"])) {
            Err(EngineError::AmbientRuntimeOverride { variable }) => {
                assert_eq!(variable, "MSB_PATH")
            }
            Err(e) => panic!("expected AmbientRuntimeOverride, got {e:?}"),
            Ok(_) => panic!("expected an error"),
        }
    }

    #[test]
    fn every_ambient_override_is_refused_at_open() {
        for variable in crate::data::config::env::AMBIENT_MSB_OVERRIDES {
            match open(settings(vec![variable])) {
                Err(EngineError::AmbientRuntimeOverride { variable: found }) => {
                    assert_eq!(found, *variable)
                }
                Err(error) => panic!("{variable} returned the wrong error: {error:?}"),
                Ok(_) => panic!("{variable} should be rejected"),
            }
        }
    }

    #[test]
    fn unavailable_build_or_test_isolation_reports_a_precise_reason() {
        match open(settings(Vec::new())) {
            Err(EngineError::BuiltinRuntimeUnavailable { reason }) => {
                assert!(is_released_target(
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ));
                assert!(
                    reason.contains("does not include") || reason.contains("test isolation"),
                    "{reason}"
                );
            }
            Err(e @ EngineError::BuiltinTargetUnreleased { .. }) => {
                assert!(!is_released_target(
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ));
                assert!(e.to_string().contains("only available on macOS"), "{e}");
            }
            Err(e) => panic!("expected the builtin runtime to be unavailable, got {e:?}"),
            Ok(_) => panic!("the stub never opens"),
        }
    }

    #[test]
    fn supported_targets() {
        assert!(is_supported_target("linux", "x86_64"));
        assert!(is_supported_target("linux", "aarch64"));
        assert!(is_supported_target("macos", "aarch64"));
        assert!(!is_supported_target("macos", "x86_64"));
        assert!(!is_supported_target("windows", "x86_64"));
        assert!(is_released_target("macos", "aarch64"));
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "x86_64"),
            ("windows", "x86_64"),
        ] {
            assert!(!is_released_target(os, arch), "{os}/{arch}");
            let reason = EngineError::BuiltinTargetUnreleased {
                os: os.into(),
                arch: arch.into(),
            }
            .to_string();
            assert!(reason.contains(&format!("{os}/{arch}")), "{reason}");
            assert!(reason.contains("macOS on Apple Silicon"), "{reason}");
        }
    }
}
