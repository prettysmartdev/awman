//! Serializable workflow prerequisite types.

use crate::data::workflow_state::PhaseKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteStepKind {
    CommitChanges,
    PushBranch,
    CreatePullRequest,
    PollCi,
    PullBranch,
    CloneRepo,
    CheckoutCreateBranch,
    RunShell,
    RunScript,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRef {
    pub phase: PhaseKind,
    pub kind: PrerequisiteStepKind,
    pub index: usize,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitRemoteKind {
    Ssh { host: String },
    Https { host: String },
    File,
    Other,
}

/// A remote URL with any embedded credentials removed at parse time. The raw URL is never stored.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GitRemoteUrl {
    pub redacted: String,
    pub kind: GitRemoteKind,
    pub owner_repo: Option<(String, String)>,
}

impl GitRemoteUrl {
    /// Parse and redact a remote URL before it can enter a persisted or
    /// serialized prerequisite model.
    pub fn parse(url: &str) -> Self {
        let url = url.trim();
        let (redacted, kind, path) = if let Some((scheme, rest)) = url.split_once("://") {
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            let authority = rest[..end].rsplit('@').next().unwrap_or_default();
            let host = if let Some(ipv6) = authority.strip_prefix('[') {
                ipv6.split(']').next().unwrap_or_default()
            } else {
                authority.split(':').next().unwrap_or_default()
            }
            .to_ascii_lowercase();
            let path = rest[end..].split(['?', '#']).next().unwrap_or_default();
            let kind = match scheme.to_ascii_lowercase().as_str() {
                "ssh" => GitRemoteKind::Ssh { host },
                "https" => GitRemoteKind::Https { host },
                "file" => GitRemoteKind::File,
                _ => GitRemoteKind::Other,
            };
            (
                format!("{scheme}://{authority}{path}"),
                kind,
                path.to_owned(),
            )
        } else if let Some((authority, path)) = url
            .split_once(':')
            .filter(|(a, _)| !a.contains('/') && !a.contains('\\') && a.len() > 1)
        {
            let host = authority
                .rsplit('@')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            (
                format!("{host}:{path}"),
                GitRemoteKind::Ssh { host },
                path.to_owned(),
            )
        } else {
            (url.to_owned(), GitRemoteKind::File, String::new())
        };
        let parts: Vec<_> = path.trim_matches('/').split('/').collect();
        let owner_repo = if parts.len() == 2 && parts.iter().all(|p| !p.is_empty()) {
            Some((
                parts[0].to_owned(),
                parts[1].trim_end_matches(".git").to_owned(),
            ))
        } else {
            None
        };
        Self {
            redacted,
            kind,
            owner_repo,
        }
    }

    pub fn host(&self) -> Option<&str> {
        match &self.kind {
            GitRemoteKind::Ssh { host } | GitRemoteKind::Https { host } => Some(host),
            _ => None,
        }
    }

    pub fn is_github_com(&self) -> bool {
        self.host() == Some("github.com")
    }

    pub fn display(&self) -> &str {
        &self.redacted
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowPrerequisite {
    GitIdentity,
    GitRemote {
        remote: String,
    },
    /// Derived before classification. The preflight replaces it (see §2 item 5).
    PushAuth {
        remote: String,
    },
    PushAuthSsh {
        remote: String,
        host: String,
    },
    PushAuthHttps {
        remote: String,
        host: String,
    },
    GitHubToken {
        host: String,
    },
    ContainerTool {
        name: String,
    },
    PushedBranch,
    NotDetachedHead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteOrigin {
    Static,
    AssumedAll,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrerequisiteNeed {
    pub prerequisite: WorkflowPrerequisite,
    pub needed_by: Vec<StepRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrerequisiteAdvisory {
    pub step: StepRef,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPrerequisites {
    pub origin: PrerequisiteOrigin,
    pub needs: Vec<PrerequisiteNeed>,
    pub advisories: Vec<PrerequisiteAdvisory>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteStatus {
    Satisfied,
    NeedsConsent,
    Missing,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquireScope {
    Global,
    Repo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Remediation {
    ReplaceRejectedToken,
    UseHostIdentity {
        name: String,
        email: String,
    },
    UseRepoConfigIdentity {
        name: String,
        email: String,
    },
    EnterIdentity,
    SetLocalIdentity {
        name: String,
        email: String,
        command: String,
    },
    MountSshKey {
        path: String,
        passphrase_protected: Option<bool>,
        mode_ok: bool,
        remembered: bool,
        suggested: bool,
    },
    MountSshDir,
    UseEnvToken {
        var: String,
        remembered: bool,
    },
    PasteToken,
    RunGhAuthToken {
        argv: Vec<String>,
        resolved_path: String,
        logged_in: Option<bool>,
    },
    RunGhAuthLogin {
        argv: Vec<String>,
        resolved_path: String,
    },
    RunAcquireCommand {
        label: String,
        argv: Vec<String>,
        resolved_path: Option<String>,
        scope: AcquireScope,
        approved: bool,
        sha256: String,
    },
    AddDockerfileLine {
        file: String,
        line: String,
    },
    AddOverlayHint {
        hint: String,
    },
    Continue,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrerequisiteCheck {
    pub prerequisite: WorkflowPrerequisite,
    pub status: PrerequisiteStatus,
    pub explanation: String,
    /// Empty means "any teardown step" (AssumedAll).
    pub needed_by: Vec<StepRef>,
    pub remediations: Vec<Remediation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisitePhase {
    Start,
    BeforeTeardown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPrerequisiteReport {
    pub phase: PrerequisitePhase,
    pub origin: PrerequisiteOrigin,
    pub checks: Vec<PrerequisiteCheck>,
    pub advisories: Vec<PrerequisiteAdvisory>,
    /// Free-text notes: policy summary ("identity from repo config"), the global acquire argv shown once,
    /// "repo config mounts ~/.ssh into every step", "granted but unused: SSH key (not mounted)".
    pub notes: Vec<String>,
}

impl WorkflowPrerequisiteReport {
    /// A compact, stable summary for persisted execution events.
    pub fn summary_line(&self) -> String {
        let count = self
            .checks
            .iter()
            .filter(|check| {
                matches!(
                    check.status,
                    PrerequisiteStatus::Missing | PrerequisiteStatus::NeedsConsent
                )
            })
            .count();
        if count == 0 {
            format!(
                "all {} workflow prerequisites are satisfied",
                self.checks.len()
            )
        } else {
            format!(
                "{count} of {} workflow prerequisites need your attention",
                self.checks.len()
            )
        }
    }
}
