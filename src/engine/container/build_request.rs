//! One build input for runtime and source-bound image builds.
use crate::data::oci_identity::OciPlatform;
use std::path::{Path, PathBuf};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildTarget {
    Runtime,
    Docker { endpoint: String },
    Apple,
}

impl BuildTarget {
    /// The builder as named in messages: the container system and, for
    /// Docker, the endpoint it is bound to.
    pub fn describe(&self) -> String {
        match self {
            Self::Docker { endpoint } => format!("Docker at {endpoint}"),
            Self::Runtime => "selected runtime".to_string(),
            Self::Apple => "Apple Containers".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuildRequest {
    /// `Runtime` preserves the selected CLI's ordinary build defaults;
    /// explicit Docker/Apple sources belong to `HostLocalImageBuilder`.
    pub target: BuildTarget,
    pub reference: String,
    pub dockerfile: PathBuf,
    pub context: PathBuf,
    /// Required platform for source-bound builds. Ordinary runtime builds
    /// retain the CLI's defaults instead of adding a platform flag.
    pub platform: OciPlatform,
    /// `--build-arg` values; empty for every current ready build, as for the
    /// Docker runtime's builds.
    pub build_args: Vec<(String, String)>,
    pub no_cache: bool,
}

impl BuildRequest {
    /// Ordinary runtime builds retain the runtime CLI's platform defaults.
    pub fn runtime(reference: &str, dockerfile: &Path, context: &Path, no_cache: bool) -> Self {
        Self {
            target: BuildTarget::Runtime,
            reference: reference.into(),
            dockerfile: dockerfile.into(),
            context: context.into(),
            platform: OciPlatform::host_linux(),
            build_args: Vec::new(),
            no_cache,
        }
    }

    /// The Docker and Apple runtimes build only ordinary runtime requests:
    /// their CLI defaults decide the endpoint and platform. A source-bound
    /// target or build arguments would be silently lost there, so they are
    /// refused instead; source-bound builds go through
    /// `HostLocalImageBuilder`.
    pub fn require_runtime_build(&self) -> Result<(), crate::engine::error::EngineError> {
        if self.target != BuildTarget::Runtime {
            return Err(crate::engine::error::EngineError::Config(format!(
                "a runtime image build cannot target {}; source-bound builds go through the \
                 builtin runtime's image builder",
                self.target.describe()
            )));
        }
        if !self.build_args.is_empty() {
            return Err(crate::engine::error::EngineError::Config(
                "a runtime image build does not take build arguments".into(),
            ));
        }
        Ok(())
    }
}
