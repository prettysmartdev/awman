//! Labels a worker reports about itself (`awman/…`). Configured labels come
//! from `workServers.<name>.labels`; server-assigned labels come from the
//! server. The three sources stay apart in [`awman_wsp::labels::WorkerLabels`].

use std::collections::BTreeMap;

use awman_wsp::labels::{LabelError, WorkerLabels, RESERVED_PREFIX};
use serde::{Deserialize, Serialize};

/// Which process hosts a worker registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerHostKind {
    Daemon,
    Tui,
}

impl WorkerHostKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Tui => "tui",
        }
    }
}

/// 0139 runtime kinds used by reported worker labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Docker,
    Apple,
    Sbx,
    Builtin,
}
impl RuntimeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Apple => "apple",
            Self::Sbx => "sbx",
            Self::Builtin => "builtin",
        }
    }
}

// Compatibility for existing configuration callers; the stored fact is typed.
impl From<&'static str> for RuntimeKind {
    /// Accept a 0139 runtime.kind spelling. Internal callers must use one of the
    /// four supported values; unsupported spellings panic before registration.
    fn from(kind: &'static str) -> Self {
        match kind {
            "docker" => Self::Docker,
            "apple" => Self::Apple,
            "sbx" => Self::Sbx,
            "builtin" => Self::Builtin,
            _ => panic!("unsupported runtime.kind: {kind}"),
        }
    }
}

/// Facts the worker reports automatically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFacts {
    pub os: &'static str,
    pub arch: &'static str,
    pub version: &'static str,
    /// `docker | apple | sbx | builtin` (0139's `runtime.kind`).
    pub runtime_kind: RuntimeKind,
    pub host: WorkerHostKind,
    /// Agents with a working image.
    pub agents_with_images: Vec<String>,
    pub interactive_allowed: bool,
    pub headless_allowed: bool,
}

impl HostFacts {
    /// OS, architecture and version of this build.
    /// Accepts 0139's runtime.kind spelling for existing configuration callers.
    /// Panics if an internal caller passes an unsupported runtime kind.
    pub fn for_this_build(
        runtime_kind: impl Into<RuntimeKind>,
        host: WorkerHostKind,
        agents_with_images: Vec<String>,
        interactive_allowed: bool,
        headless_allowed: bool,
    ) -> Self {
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            version: env!("CARGO_PKG_VERSION"),
            runtime_kind: runtime_kind.into(),
            host,
            agents_with_images,
            interactive_allowed,
            headless_allowed,
        }
    }

    /// Every reported label, each under `awman/`.
    pub fn reported_labels(&self) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        let mut put = |k: String, v: &str| {
            labels.insert(format!("{RESERVED_PREFIX}{k}"), v.to_owned());
        };
        put("os".into(), self.os);
        put("arch".into(), self.arch);
        put("version".into(), self.version);
        put("runtime".into(), self.runtime_kind.as_str());
        put("host".into(), self.host.as_str());
        for agent in &self.agents_with_images {
            // Agent names allow characters label keys do not (such as `_`).
            // Such an agent is left unreported rather than failing every
            // registration with an invalid key.
            let key = format!("agent.{}", agent.to_ascii_lowercase());
            if awman_wsp::labels::validate_key(&format!("{RESERVED_PREFIX}{key}")).is_ok() {
                put(key, "true");
            }
        }
        if self.interactive_allowed {
            put("interaction.interactive".into(), "true");
        }
        if self.headless_allowed {
            put("interaction.headless".into(), "true");
        }
        labels
    }

    /// Reported plus configured labels; configured `awman/` keys are refused.
    pub fn worker_labels(
        &self,
        configured: &BTreeMap<String, String>,
    ) -> Result<WorkerLabels, LabelError> {
        WorkerLabels::new(self.reported_labels(), configured.clone())
    }
}
