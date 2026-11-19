//! Native starting mode and the resolved agent launch policy — Layer 0.
//!
//! `--plan` asks an agent to *start* in its own planning (or read-only)
//! mode; `--read-only-workspace` asks the runtime to mount the workspace
//! read-only. The two are independent: neither implies the other, and a
//! launch reports them separately. These are the wire spellings every
//! frontend, result and event uses for that pair.
//!
//! Layer 2's `LaunchPolicy` produces a [`ResolvedAgentPolicy`] once per
//! launch; Layer 1 turns it into argv, settings and mounts. Nothing here
//! knows an agent's name or a runtime.

use serde::{Deserialize, Serialize};

use crate::data::config::agents::LaunchMode;
use crate::data::workspace::WorkspaceAccess;

/// What an agent definition's native plan mapping actually selects.
///
/// `plan` is a provider planning mode, `read_only` a provider read-only
/// sandbox that is the supported equivalent (Codex), and `custom` a mode a
/// custom definition declares without claiming either meaning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativePlanKind {
    Plan,
    ReadOnly,
    /// The default for a custom definition that does not state a kind.
    #[default]
    Custom,
}

impl NativePlanKind {
    pub const ALL: [Self; 3] = [Self::Plan, Self::ReadOnly, Self::Custom];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::ReadOnly => "read_only",
            Self::Custom => "custom",
        }
    }
}

/// The native mode a launch asked its agent to start in.
///
/// It is a *starting* mode: a provider-supported later transition to
/// implementation does not change the value recorded for the launch, and it
/// says nothing about workspace mounts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeStartMode {
    /// No native mode was requested; the agent's own default applies.
    #[default]
    Default,
    Plan,
    ReadOnly,
    Custom,
}

impl NativeStartMode {
    pub const ALL: [Self; 4] = [Self::Default, Self::Plan, Self::ReadOnly, Self::Custom];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Plan => "plan",
            Self::ReadOnly => "read_only",
            Self::Custom => "custom",
        }
    }

    /// Whether a native plan/read-only starting mode was requested.
    pub fn is_requested(self) -> bool {
        self != Self::Default
    }
}

impl From<NativePlanKind> for NativeStartMode {
    fn from(kind: NativePlanKind) -> Self {
        match kind {
            NativePlanKind::Plan => Self::Plan,
            NativePlanKind::ReadOnly => Self::ReadOnly,
            NativePlanKind::Custom => Self::Custom,
        }
    }
}

/// The launch policy a command resolved, once, for one agent launch.
///
/// The three fields are resolved independently. `workspace_access` reuses
/// the existing [`WorkspaceAccess`] spellings (`read_write | read_only`) and
/// `launch_mode` the existing `agents.launchMode` spellings (`stdio | acp`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedAgentPolicy {
    pub native_start_mode: NativeStartMode,
    pub workspace_access: WorkspaceAccess,
    pub launch_mode: LaunchMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_spellings_match_as_str() {
        for mode in NativeStartMode::ALL {
            assert_eq!(
                serde_json::to_value(mode).unwrap(),
                serde_json::json!(mode.as_str())
            );
            let back: NativeStartMode =
                serde_json::from_value(serde_json::json!(mode.as_str())).unwrap();
            assert_eq!(back, mode);
        }
        for kind in NativePlanKind::ALL {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::json!(kind.as_str())
            );
            assert_eq!(NativeStartMode::from(kind).as_str(), kind.as_str());
        }
        assert!(serde_json::from_str::<NativeStartMode>("\"planning\"").is_err());
    }

    #[test]
    fn default_policy_requests_nothing() {
        let policy = ResolvedAgentPolicy::default();
        assert!(!policy.native_start_mode.is_requested());
        assert!(!policy.workspace_access.is_read_only());
        assert_eq!(
            serde_json::to_value(policy).unwrap(),
            serde_json::json!({
                "native_start_mode": "default",
                "workspace_access": "read_write",
                "launch_mode": "stdio",
            })
        );
    }

    #[test]
    fn mode_and_mount_access_are_independent_fields() {
        for mode in NativeStartMode::ALL {
            for access in [WorkspaceAccess::ReadWrite, WorkspaceAccess::ReadOnly] {
                let policy = ResolvedAgentPolicy {
                    native_start_mode: mode,
                    workspace_access: access,
                    launch_mode: LaunchMode::Stdio,
                };
                let value = serde_json::to_value(policy).unwrap();
                assert_eq!(value["native_start_mode"], mode.as_str());
                assert_eq!(value["workspace_access"], access.as_str());
                let back: ResolvedAgentPolicy = serde_json::from_value(value).unwrap();
                assert_eq!(back, policy);
            }
        }
    }
}
