use crate::engine::error::EngineError;

pub struct EmbeddedRuntime;
impl EmbeddedRuntime {
    pub fn resolve(ambient_overrides: &[&'static str]) -> Result<std::path::PathBuf, EngineError> {
        if let Some(variable) = ambient_overrides.first() {
            return Err(EngineError::AmbientRuntimeOverride {
                variable: (*variable).to_string(),
            });
        }
        let executable =
            std::env::current_exe().map_err(|e| EngineError::io("current executable", e))?;
        // The worker is this same executable re-run by path. After an in-place
        // upgrade (e.g. `make install` during a session) that path is a different
        // binary, or on Linux the running one reads as `<path> (deleted)`.
        if replaced_on_disk(&executable) {
            return Err(EngineError::BuiltinRuntimeUnavailable {
            reason: "awman was replaced on disk while this session was running; restart awman to use the builtin runtime".into(),
        });
        }
        // Unit/integration test harnesses must never accidentally re-execute as VMs.
        if executable
            .parent()
            .and_then(std::path::Path::file_name)
            .is_some_and(|n| n == "deps")
        {
            return Err(EngineError::BuiltinRuntimeUnavailable {
                reason: "test executables cannot host the private builtin worker".into(),
            });
        }
        let version = microsandbox::setup::resolve_runtime_version(&executable).map_err(|_| {
            EngineError::BuiltinRuntimeUnavailable {
                reason: "cannot inspect embedded runtime metadata".into(),
            }
        })?;
        if version.as_ref().map(ToString::to_string).as_deref()
            != Some(crate::data::builtin_runtime::MSB_VERSION)
        {
            return Err(EngineError::BuiltinRuntimeUnavailable {
                reason: "executable lacks the matching builtin runtime version section".into(),
            });
        }
        microsandbox::config::set_sdk_msb_path(&executable);
        microsandbox::set_libkrunfw_path(&executable);
        Ok(executable)
    }
}

fn replaced_on_disk(executable: &std::path::Path) -> bool {
    executable
        .to_str()
        .is_some_and(|path| path.ends_with(" (deleted)"))
        || !executable.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambient_overrides_are_refused_before_resolving_the_executable() {
        let error = EmbeddedRuntime::resolve(&["MSB_PATH"]).unwrap_err();
        assert!(
            matches!(error, EngineError::AmbientRuntimeOverride { variable } if variable == "MSB_PATH")
        );
    }
    #[test]
    fn detects_a_replaced_executable() {
        assert!(replaced_on_disk(std::path::Path::new(
            "/usr/local/bin/awman (deleted)"
        )));
        assert!(replaced_on_disk(std::path::Path::new("/nonexistent/awman")));
        assert!(!replaced_on_disk(&std::env::current_exe().unwrap()));
    }
}
