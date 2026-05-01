//! `ContainerRuntime` — the typed factory for `ContainerInstance` builds.
//!
//! Holds a `Box<dyn ContainerBackend>` chosen by `detect`. The concrete
//! backend is invisible outside this module.

use crate::data::config::global::GlobalConfig;
use crate::data::session::{ContainerHandle, Session};
use crate::engine::container::apple::AppleBackend;
use crate::engine::container::backend::ContainerBackend;
use crate::engine::container::docker::DockerBackend;
use crate::engine::container::instance::{ContainerInstance, ContainerStats};
use crate::engine::container::options::{ContainerOption, ResolvedContainerOptions};
use crate::engine::error::EngineError;

pub struct ContainerRuntime {
    backend: Box<dyn ContainerBackend>,
}

impl ContainerRuntime {
    /// Inspect `global_config` and the host environment to select the correct
    /// backend (Docker by default, Apple Containers when configured + macOS).
    pub fn detect(global_config: &GlobalConfig) -> Result<Self, EngineError> {
        let runtime_name = global_config
            .runtime
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        let chosen = match runtime_name {
            Some("docker") | None => Backend::Docker,
            Some("apple-containers") => {
                if cfg!(target_os = "macos") {
                    Backend::Apple
                } else {
                    return Err(EngineError::BackendUnsupportedOnPlatform {
                        backend: "apple-containers".into(),
                        platform: std::env::consts::OS.into(),
                    });
                }
            }
            Some(other) => {
                return Err(EngineError::Config(format!(
                    "unknown runtime '{other}'; supported values are 'docker' and 'apple-containers'"
                )));
            }
        };
        let backend: Box<dyn ContainerBackend> = match chosen {
            Backend::Docker => Box::new(DockerBackend::new()),
            Backend::Apple => Box::new(AppleBackend::new()),
        };
        Ok(Self { backend })
    }

    /// Construct directly with a Docker backend (escape hatch for tests
    /// and code paths that have already resolved the backend).
    pub fn docker() -> Self {
        Self {
            backend: Box::new(DockerBackend::new()),
        }
    }

    /// Static name of the chosen backend (e.g. `"docker"`).
    pub fn runtime_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Build a fully-configured `ContainerInstance` from the given options.
    pub fn build(
        &self,
        options: impl IntoIterator<Item = ContainerOption>,
    ) -> Result<Box<dyn ContainerInstance>, EngineError> {
        let resolved = ResolvedContainerOptions::from_iter(options).map_err(|e| match e {
            crate::engine::container::options::ResolveError::Conflict(msg) => {
                EngineError::ConflictingOptions(msg)
            }
        })?;
        self.backend.build(resolved)
    }

    pub fn list_running(&self, session: &Session) -> Result<Vec<ContainerHandle>, EngineError> {
        self.backend.list_running(session)
    }

    pub fn stats(&self, handle: &ContainerHandle) -> Result<ContainerStats, EngineError> {
        self.backend.stats(handle)
    }

    pub fn stop(&self, handle: &ContainerHandle) -> Result<(), EngineError> {
        self.backend.stop(handle)
    }
}

enum Backend {
    Docker,
    Apple,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_default_picks_docker() {
        let cfg = GlobalConfig::default();
        let rt = ContainerRuntime::detect(&cfg).unwrap();
        assert_eq!(rt.runtime_name(), "docker");
    }

    #[test]
    fn detect_apple_on_non_mac_errors() {
        let cfg = GlobalConfig {
            runtime: Some("apple-containers".into()),
            ..Default::default()
        };
        let res = ContainerRuntime::detect(&cfg);
        if cfg!(target_os = "macos") {
            assert!(res.is_ok());
        } else {
            match res {
                Err(EngineError::BackendUnsupportedOnPlatform { .. }) => {}
                Err(e) => panic!("expected BackendUnsupportedOnPlatform, got: {e:?}"),
                Ok(_) => panic!("expected error on non-macOS"),
            }
        }
    }

    #[test]
    fn detect_unknown_runtime_is_hard_error() {
        let cfg = GlobalConfig {
            runtime: Some("blarg".into()),
            ..Default::default()
        };
        match ContainerRuntime::detect(&cfg) {
            Err(EngineError::Config(msg)) => {
                assert!(msg.contains("blarg"), "error message should name the bad value");
            }
            Ok(_) => panic!("expected Config error for unknown runtime, got Ok"),
            Err(e) => panic!("expected Config error for unknown runtime, got Err({e:?})"),
        }
    }

    #[test]
    fn build_requires_image_option() {
        let rt = ContainerRuntime::docker();
        match rt.build([]) {
            Err(EngineError::MissingRequiredOption(opt)) => {
                assert_eq!(opt, "Image");
            }
            Err(e) => panic!("expected MissingRequiredOption, got: {e:?}"),
            Ok(_) => panic!("expected error from missing Image option"),
        }
    }
}
