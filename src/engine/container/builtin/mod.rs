//! Engine-owned embedded Microsandbox backend. Unsupported builds fail closed;
//! supported builds use SDK control IPC and the binary's private worker route.

use std::sync::Arc;

use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::runtime::BuiltinRuntimeSettings;
use crate::engine::error::EngineError;

#[cfg(awman_builtin)]
mod backend;
#[cfg(awman_builtin)]
mod catalog;
#[cfg(awman_builtin)]
mod driver;
#[cfg(awman_builtin)]
mod embedded;
#[cfg(awman_builtin)]
mod exec_bridge;
#[cfg(awman_builtin)]
mod instance;
#[cfg(awman_builtin)]
mod msb_driver;
#[cfg(awman_builtin)]
mod naming;
#[cfg(awman_builtin)]
mod paths;
#[cfg(awman_builtin)]
mod resources;
#[cfg(all(test, awman_builtin))]
mod testing;

/// `(target_os, target_arch)` pairs the builtin runtime supports.
pub(super) const SUPPORTED_TARGETS: &[(&str, &str)] = &[
    ("linux", "x86_64"),
    ("linux", "aarch64"),
    ("macos", "aarch64"),
];

/// Open the builtin backend with `settings`.
///
/// Refusals, in order:
/// 1. an ambient `MSB_*` override is set → `AmbientRuntimeOverride` (it
///    would outrank the embedded runtime);
/// 2. this OS/architecture is not supported → `BuiltinRuntimeUnavailable`;
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
    if !is_supported_target(os, arch) {
        return Err(EngineError::BuiltinRuntimeUnavailable {
            reason: unsupported_target_reason(os, arch),
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
        let driver = msb_driver::MsbDriver::open(paths.clone())?;
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
pub(super) fn is_supported_target(os: &str, arch: &str) -> bool {
    SUPPORTED_TARGETS.contains(&(os, arch))
}

fn unsupported_target_reason(os: &str, arch: &str) -> String {
    let supported: Vec<String> = SUPPORTED_TARGETS
        .iter()
        .map(|(os, arch)| format!("{os}/{arch}"))
        .collect();
    format!(
        "not supported on {os}/{arch}; supported platforms: {}",
        supported.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn settings(ambient: Vec<&'static str>) -> BuiltinRuntimeSettings {
        BuiltinRuntimeSettings {
            state_dir: "/tmp/awman-builtin-test".into(),
            vcpus: 2,
            memory_mib: 4096,
            image_source: None,
            images: BTreeMap::new(),
            registries: BTreeMap::new(),
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
    fn unavailable_build_or_test_isolation_reports_a_precise_reason() {
        match open(settings(Vec::new())) {
            Err(EngineError::BuiltinRuntimeUnavailable { reason }) => {
                if is_supported_target(std::env::consts::OS, std::env::consts::ARCH) {
                    assert!(
                        reason.contains("does not include") || reason.contains("test isolation"),
                        "{reason}"
                    );
                } else {
                    assert!(reason.contains("not supported on"), "{reason}");
                }
            }
            Err(e) => panic!("expected BuiltinRuntimeUnavailable, got {e:?}"),
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
        let reason = unsupported_target_reason("windows", "x86_64");
        assert!(reason.contains("windows/x86_64"));
        assert!(reason.contains("macos/aarch64"));
    }
}
