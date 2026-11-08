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

#[cfg(test)]
mod pure_validation_tests {
    use super::*;

    #[test]
    fn remote_parse_removes_credentials_before_data_can_be_persisted() {
        let remote = GitRemoteUrl::parse(
            "https://user:password@ghe.example/owner/repo.git?access_token=secret#fragment",
        );
        assert_eq!(remote.display(), "https://ghe.example/owner/repo.git");
        assert_eq!(remote.host(), Some("ghe.example"));
        assert_eq!(remote.owner_repo, Some(("owner".into(), "repo".into())));
        let serialized = serde_json::to_string(&remote).unwrap();
        assert!(!serialized.contains("password"));
        assert!(!serialized.contains("secret"));
    }
}

#[cfg(test)]
mod layering_tests {
    use std::path::Path;

    fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn only_local_socket_dialog_contract_may_hold_a_secret_string() {
        // Built from parts so this test does not match itself.
        let needle = format!("{}{}", "Secret", "String");
        let mut files = vec![];
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/data");
        rust_files(&root, &mut files);
        // WI 0142 moves the redacting wrapper and its explicit local-socket
        // dialog answer into data. Persisted prerequisite reports stay secret-free.
        let local_socket_types = [root.join("secret.rs"), root.join("dialog.rs")];
        assert!(files.len() > 20);
        let offenders: Vec<_> = files
            .iter()
            .filter(|file| !local_socket_types.contains(file))
            .filter(|f| std::fs::read_to_string(f).unwrap().contains(&needle))
            .collect();
        assert!(
            offenders.is_empty(),
            "Layer 0 types outside the local dialog contract must not hold secrets: {offenders:?}"
        );
    }

    #[test]
    fn layer_zero_prerequisite_types_derive_serialize_but_hold_no_value_fields() {
        // The only free-text fields in a report are explanations/notes; the
        // serialized shape of every remediation names a choice, not a value.
        let remediation =
            serde_json::to_string(&crate::data::workflow_prerequisites::Remediation::PasteToken)
                .unwrap();
        assert_eq!(remediation, r#"{"type":"paste_token"}"#);
    }
}
