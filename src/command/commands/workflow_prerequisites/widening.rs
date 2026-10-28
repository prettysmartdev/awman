//! Widening checks for frontends that act for a remote client.
//!
//! An API or squad-daemon request may only *narrow* what the server's own
//! global policy grants. A flag that would grant more (mount the operator's
//! SSH key, hand over a token from the server's environment, run the
//! server's token command, inject its identity) is refused unless the
//! operator set `workflows.prerequisites.allowApiClientGrants: true` in the
//! server's **global** config. Narrowing flags (`deny`, `--prerequisite-unresolved
//! fail`) always pass.

use crate::command::dispatch::catalogue::FrontendKind;
use crate::command::error::CommandError;
use crate::data::config::flags::{IdentityFlag, PrerequisiteFlags, SshFlag, TokenFlag};
use crate::data::config::workflows::{CredentialUse, PrerequisitePolicy, UnresolvedMode};

/// Refuse any `--prerequisite-*` flag from an API or squad-daemon client that
/// grants beyond `global_policy`, the server's policy resolved **without**
/// the request's flags. CLI and TUI callers are the user and always pass.
pub fn check_flag_widening(
    kind: FrontendKind,
    flags: &PrerequisiteFlags,
    global_policy: &PrerequisitePolicy,
) -> Result<(), CommandError> {
    if kind.is_local_user() || global_policy.allow_api_client_grants {
        return Ok(());
    }
    let refuse = |flag: &str, value: String| {
        Err(CommandError::PrerequisiteWideningRefused {
            flag: format!("--{flag} {value}"),
        })
    };
    match &flags.identity {
        None | Some(IdentityFlag::Deny) => {}
        Some(IdentityFlag::Fixed { .. }) => {
            // A fixed name and email is not a secret and reads nothing from
            // the server; it widens only when the server denies identities.
            if global_policy.git_identity.use_ == CredentialUse::Deny {
                return refuse(super::flags::FLAG_IDENTITY, "<fixed identity>".into());
            }
        }
        Some(IdentityFlag::Host) => {
            if global_policy.git_identity.use_ != CredentialUse::Allow {
                return refuse(super::flags::FLAG_IDENTITY, "host".into());
            }
        }
    }
    match &flags.ssh {
        None | Some(SshFlag::Deny) => {}
        Some(SshFlag::All) => {
            if !(global_policy.ssh.use_ == CredentialUse::Allow && global_policy.ssh.whole_dir) {
                return refuse(super::flags::FLAG_SSH, "all".into());
            }
        }
        Some(SshFlag::Key(path)) => {
            let same_key = global_policy.ssh.key.as_deref() == Some(path.as_path())
                && !global_policy.ssh.key_is_suggestion;
            if !(global_policy.ssh.use_ == CredentialUse::Allow && same_key) {
                return refuse(super::flags::FLAG_SSH, path.display().to_string());
            }
        }
    }
    match &flags.token {
        None | Some(TokenFlag::Deny) => {}
        Some(TokenFlag::Env(name)) => {
            let listed = global_policy
                .github_token
                .env_vars
                .iter()
                .any(|v| v == name);
            if !(global_policy.github_token.use_ == CredentialUse::Allow && listed) {
                return refuse(super::flags::FLAG_TOKEN, format!("env:{name}"));
            }
        }
        Some(TokenFlag::Acquire) => {
            let global_acquire = global_policy
                .github_token
                .acquire
                .as_ref()
                .is_some_and(|a| {
                    a.scope == crate::data::workflow_prerequisites::AcquireScope::Global
                });
            if !(global_policy.github_token.use_ == CredentialUse::Allow && global_acquire) {
                return refuse(super::flags::FLAG_TOKEN, "acquire".into());
            }
        }
        Some(TokenFlag::Secret(name)) => {
            return Err(CommandError::PrerequisiteFlagUnsupported {
                flag: format!("--{} secret:{name}", super::flags::FLAG_TOKEN),
            })
        }
    }
    // `--prerequisite-unresolved warn` cannot loosen a server `fail`.
    if flags.unresolved == Some(UnresolvedMode::Warn)
        && global_policy.unresolved == Some(UnresolvedMode::Fail)
    {
        return refuse(super::flags::FLAG_UNRESOLVED, "warn".into());
    }
    Ok(())
}

/// Recheck remotely stored decisions against the operator's current ceiling.
pub fn check_task_widening(
    task: &PrerequisitePolicy,
    global: &PrerequisitePolicy,
) -> Result<(), CommandError> {
    use crate::data::config::workflows::PolicySource;
    let mut flags = PrerequisiteFlags::default();
    if task.git_identity.source == PolicySource::RemoteTask
        && task.git_identity.use_ == CredentialUse::Allow
    {
        flags.identity = Some(match &task.git_identity.fixed {
            Some(id) => IdentityFlag::Fixed {
                name: id.name.clone(),
                email: id.email.clone(),
            },
            None => IdentityFlag::Host,
        });
    }
    if task.ssh.source == PolicySource::RemoteTask && task.ssh.use_ == CredentialUse::Allow {
        flags.ssh = if task.ssh.whole_dir {
            Some(SshFlag::All)
        } else {
            task.ssh.key.clone().map(SshFlag::Key)
        };
    }
    if task.github_token.source == PolicySource::RemoteTask
        && task.github_token.use_ == CredentialUse::Allow
    {
        flags.token = Some(match task.github_token.env_vars.first() {
            Some(var) => TokenFlag::Env(var.clone()),
            None => TokenFlag::Acquire,
        });
    }
    check_flag_widening(FrontendKind::SquadDaemon, &flags, global)
}
