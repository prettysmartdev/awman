//! Engine-owned embedded Microsandbox backend. Unsupported builds fail closed;
//! supported builds use SDK control IPC and the binary's private worker route.

use std::sync::Arc;

use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::runtime::BuiltinRuntimeSettings;
use crate::engine::error::EngineError;

// Gating follows the actual runtime boundary. Only the modules that call the
// Microsandbox SDK or embed the verified payload (`embedded`, `msb_driver`)
// need `awman_builtin`. Planning, paths, naming, resources, CPU accounting,
// network policy, the catalog marker, attach framing and the fake-driver
// lifecycle are plain Unix code, so their tests run in the default hermetic
// tier without payloads or the VM SDK. A default build has no production caller for them.
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
mod cpu;
#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
mod driver;
#[cfg(awman_builtin)]
mod embedded;
#[cfg(unix)]
#[cfg_attr(not(awman_builtin), allow(dead_code))]
mod exec_bridge;

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
        let (driver, upgraded) =
            msb_driver::MsbDriver::open(paths.clone(), &settings.ambient_overrides)?;
        Ok(Arc::new(backend::BuiltinBackend {
            driver,
            paths,
            settings,
            owner: uuid::Uuid::new_v4().to_string(),
            background_env: std::sync::Mutex::new(Default::default()),
            notices: std::sync::Mutex::new(upgraded.iter().map(|u| u.notice()).collect()),
            cancel_grace: exec_bridge::DEFAULT_CANCEL_GRACE,
        }))
    }
    #[cfg(not(awman_builtin))]
    Err(EngineError::BuiltinRuntimeUnavailable {
        reason: "this build of awman does not include the builtin-runtime feature".into(),
    })
}

/// Whether `(os, arch)` is one of [`RELEASED_TARGETS`].
pub(super) fn is_released_target(os: &str, arch: &str) -> bool {
    RELEASED_TARGETS.contains(&(os, arch))
}
