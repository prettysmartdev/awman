//! What `awman ready` prepares, and what it can say about authentication —
//! Layer 0.
//!
//! `ready --prepare-only` prepares images without a host-agent ping, a
//! credential refresh or an agent audit. Its result must say so: an
//! image-ready outcome never implies that a skipped authentication check
//! passed. These are the wire spellings of the ready JSON fields
//! `preparation_policy` and `authentication`.

use serde::{Deserialize, Serialize};

/// The catalogue flag (`--prepare-only`) on `ready`.
pub const PREPARE_ONLY_FLAG: &str = "prepare-only";

/// The `/v1/status` capability advertised once `ready --prepare-only` works.
pub const READY_PREPARE_ONLY_CAPABILITY: &str = "ready_prepare_only";

/// How much of readiness one `ready` invocation establishes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationPolicy {
    /// The existing behaviour: images, then host-agent checks and any
    /// requested audit. Ordinary `ready` and daemon session setup.
    #[default]
    Full,
    /// Images, definition trust and registry authentication only. Selected
    /// by an explicit `ready --prepare-only`, never inferred.
    ImagesOnly,
}

impl PreparationPolicy {
    pub const ALL: [Self; 2] = [Self::Full, Self::ImagesOnly];

    pub fn from_prepare_only(prepare_only: bool) -> Self {
        if prepare_only {
            Self::ImagesOnly
        } else {
            Self::Full
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ImagesOnly => "images_only",
        }
    }

    /// Whether host-agent pings, credential refresh registration and agent
    /// audits run under this policy.
    pub fn checks_authentication(self) -> bool {
        self == Self::Full
    }
}

/// What a `ready` run established about provider authentication.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthenticationCheck {
    /// The check was skipped (`images_only`). Not a pass.
    NotChecked,
    Passed,
    Failed,
    /// A check was attempted or expected but produced no verdict.
    #[default]
    Unknown,
}

impl AuthenticationCheck {
    pub const ALL: [Self; 4] = [Self::NotChecked, Self::Passed, Self::Failed, Self::Unknown];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotChecked => "not_checked",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_spellings_match_as_str() {
        for policy in PreparationPolicy::ALL {
            assert_eq!(
                serde_json::to_value(policy).unwrap(),
                serde_json::json!(policy.as_str())
            );
        }
        for check in AuthenticationCheck::ALL {
            assert_eq!(
                serde_json::to_value(check).unwrap(),
                serde_json::json!(check.as_str())
            );
        }
        assert!(serde_json::from_str::<PreparationPolicy>("\"image_only\"").is_err());
    }

    #[test]
    fn only_the_explicit_flag_selects_images_only() {
        assert_eq!(PreparationPolicy::default(), PreparationPolicy::Full);
        assert_eq!(
            PreparationPolicy::from_prepare_only(false),
            PreparationPolicy::Full
        );
        assert_eq!(
            PreparationPolicy::from_prepare_only(true),
            PreparationPolicy::ImagesOnly
        );
        assert!(PreparationPolicy::Full.checks_authentication());
        assert!(!PreparationPolicy::ImagesOnly.checks_authentication());
    }

    #[test]
    fn a_skipped_check_is_never_the_default_or_a_pass() {
        assert_eq!(AuthenticationCheck::default(), AuthenticationCheck::Unknown);
        assert_ne!(AuthenticationCheck::NotChecked, AuthenticationCheck::Passed);
    }
}
