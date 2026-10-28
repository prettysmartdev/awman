//! The `--prerequisite-*` catalogue flags, parsed once in Layer 2 for every
//! frontend (CLI args, the TUI command box and API `args` alike).

use std::path::PathBuf;

use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::data::config::flags::{IdentityFlag, PrerequisiteFlags, SshFlag, TokenFlag};
use crate::data::config::workflows::{CredentialUse, PrerequisitePolicy, UnresolvedMode};
use crate::data::session::Session;

pub const FLAG_IDENTITY: &str = "prerequisite-identity";
pub const FLAG_SSH: &str = "prerequisite-ssh";
pub const FLAG_TOKEN: &str = "prerequisite-token";
pub const FLAG_UNRESOLVED: &str = "prerequisite-unresolved";
pub const FLAG_CHECK: &str = "check-prerequisites";

/// Read the `--prerequisite-*` flags from a command that declares them.
/// `with_check` is true for `exec workflow`, the only command that also
/// declares `--check-prerequisites`.
///
/// `--prerequisite-token secret:NAME` is refused with
/// [`CommandError::PrerequisiteFlagUnsupported`] until per-command secrets
/// exist.
pub fn parse_prerequisite_flags(
    ctx: &BuildContext,
    with_check: bool,
) -> Result<PrerequisiteFlags, CommandError> {
    let path = ctx.path();
    let invalid = |flag: &str, reason: String| CommandError::InvalidFlagValue {
        command: path.iter().map(|s| s.to_string()).collect(),
        flag: flag.to_string(),
        reason,
    };
    let identity = ctx
        .flags
        .string(FLAG_IDENTITY)
        .map(|raw| parse_identity(&raw).map_err(|r| invalid(FLAG_IDENTITY, r)))
        .transpose()?;
    let ssh = ctx
        .flags
        .string(FLAG_SSH)
        .map(|raw| parse_ssh(&raw).map_err(|r| invalid(FLAG_SSH, r)))
        .transpose()?;
    let token = match ctx.flags.string(FLAG_TOKEN) {
        None => None,
        Some(raw) => match parse_token(&raw).map_err(|r| invalid(FLAG_TOKEN, r))? {
            TokenFlag::Secret(_) => {
                return Err(CommandError::PrerequisiteFlagUnsupported {
                    flag: format!("--{FLAG_TOKEN} {raw}"),
                })
            }
            other => Some(other),
        },
    };
    let unresolved = match ctx.flags.r#enum(FLAG_UNRESOLVED) {
        None => None,
        Some("warn") => Some(UnresolvedMode::Warn),
        Some("fail") => Some(UnresolvedMode::Fail),
        Some(other) => return Err(invalid(FLAG_UNRESOLVED, format!("unknown mode {other:?}"))),
    };
    Ok(PrerequisiteFlags {
        identity,
        ssh,
        token,
        unresolved,
        check_only: with_check && ctx.flags.bool(FLAG_CHECK),
    })
}

/// `host`, `deny`, or `Name <email>`.
pub fn parse_identity(raw: &str) -> Result<IdentityFlag, String> {
    let raw = raw.trim();
    match raw {
        "host" => return Ok(IdentityFlag::Host),
        "deny" => return Ok(IdentityFlag::Deny),
        _ => {}
    }
    let (name, rest) = raw
        .split_once('<')
        .ok_or_else(|| "expected host, deny or \"Name <email>\"".to_string())?;
    let email = rest
        .strip_suffix('>')
        .ok_or_else(|| "expected host, deny or \"Name <email>\"".to_string())?
        .trim();
    let name = name.trim();
    validate_identity(name, email)?;
    Ok(IdentityFlag::Fixed {
        name: name.to_string(),
        email: email.to_string(),
    })
}

/// The identity rules every source shares: non-empty, no control characters
/// (so no newline), and an email containing `@`.
pub fn validate_identity(name: &str, email: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("the name is empty".into());
    }
    if email.trim().is_empty() {
        return Err("the email is empty".into());
    }
    if name.chars().chain(email.chars()).any(char::is_control) {
        return Err("the name and email must not contain newlines or control characters".into());
    }
    if !email.contains('@') {
        return Err("the email must contain '@'".into());
    }
    Ok(())
}

/// `deny`, `all` (the whole `~/.ssh`), or a key path.
pub fn parse_ssh(raw: &str) -> Result<SshFlag, String> {
    match raw.trim() {
        "" => Err("expected a key path, all or deny".into()),
        "deny" => Ok(SshFlag::Deny),
        "all" => Ok(SshFlag::All),
        path => Ok(SshFlag::Key(PathBuf::from(path))),
    }
}

/// `deny`, `acquire`, `env:NAME` or `secret:NAME`.
pub fn parse_token(raw: &str) -> Result<TokenFlag, String> {
    let raw = raw.trim();
    match raw {
        "deny" => return Ok(TokenFlag::Deny),
        "acquire" => return Ok(TokenFlag::Acquire),
        _ => {}
    }
    let name_ok = |name: &str| crate::data::config::workflows::valid_env_name(name);
    if let Some(name) = raw.strip_prefix("env:") {
        if !name_ok(name) {
            return Err(format!("{name:?} is not a valid environment variable name"));
        }
        return Ok(TokenFlag::Env(name.to_string()));
    }
    if let Some(name) = raw.strip_prefix("secret:") {
        if name.is_empty() {
            return Err("secret: needs a name".into());
        }
        return Ok(TokenFlag::Secret(name.to_string()));
    }
    Err("expected env:NAME, acquire, secret:NAME or deny".into())
}

fn use_label(use_: CredentialUse) -> &'static str {
    match use_ {
        CredentialUse::Ask => "ask",
        CredentialUse::Allow => "allow",
        CredentialUse::Deny => "deny",
    }
}

/// `flags_applied.prerequisites` for the API: the resolved policy summary
/// (use per credential, where it came from, `unresolved`). Names only, never
/// a value.
pub fn flags_applied_policy(session: &Session, flags: &PrerequisiteFlags) -> serde_json::Value {
    let mut session = session.clone();
    let mut flag_config = session.flags().clone();
    flag_config.prerequisites = flags.clone();
    session.set_flags(flag_config);
    policy_summary(&session.prerequisite_policy())
}

/// The JSON summary of a resolved policy, shared by `flags_applied` and
/// `squad show`.
pub fn policy_summary(policy: &PrerequisitePolicy) -> serde_json::Value {
    serde_json::json!({
        "preflight": policy.preflight,
        "unresolved": policy.unresolved,
        "gitIdentity": {
            "use": use_label(policy.git_identity.use_),
            "fixed": policy.git_identity.fixed.as_ref().map(|f| format!("{} <{}>", f.name, f.email)),
            "source": policy.git_identity.source,
        },
        "ssh": {
            "use": use_label(policy.ssh.use_),
            "key": policy.ssh.key,
            "keyIsSuggestion": policy.ssh.key_is_suggestion,
            "wholeDir": policy.ssh.whole_dir,
            "source": policy.ssh.source,
        },
        "githubToken": {
            "use": use_label(policy.github_token.use_),
            "envVars": policy.github_token.env_vars,
            "acquire": policy.github_token.acquire.as_ref().map(|a| a.argv.clone()),
            "source": policy.github_token.source,
        },
    })
}

/// One line per credential, for `squad show` and the run log.
pub fn policy_lines(policy: &PrerequisitePolicy) -> Vec<String> {
    let mut identity = use_label(policy.git_identity.use_).to_string();
    if let Some(fixed) = &policy.git_identity.fixed {
        identity.push_str(&format!(" ({} <{}>)", fixed.name, fixed.email));
    }
    let mut ssh = use_label(policy.ssh.use_).to_string();
    if policy.ssh.whole_dir {
        ssh.push_str(" (whole ~/.ssh)");
    } else if let Some(key) = &policy.ssh.key {
        ssh.push_str(&format!(" ({})", key.display()));
    }
    let mut token = use_label(policy.github_token.use_).to_string();
    if !policy.github_token.env_vars.is_empty() {
        token.push_str(&format!(
            " (env {})",
            policy.github_token.env_vars.join(", ")
        ));
    }
    vec![
        format!("git identity: {identity}"),
        format!("ssh: {ssh}"),
        format!("github token: {token}"),
    ]
}

/// A stored policy as the `--prerequisite-*` argv that reproduces it, for
/// the remote squad gateway (which speaks catalogue args to the daemon).
/// `ask` has no flag, so an `ask` credential is simply left out.
pub fn prerequisite_flag_args(policy: &PrerequisitePolicy) -> Vec<String> {
    let mut args = Vec::new();
    match (policy.git_identity.use_, &policy.git_identity.fixed) {
        (CredentialUse::Deny, _) => args.extend(["--prerequisite-identity".into(), "deny".into()]),
        (CredentialUse::Allow, Some(fixed)) => args.extend([
            "--prerequisite-identity".into(),
            format!("{} <{}>", fixed.name, fixed.email),
        ]),
        (CredentialUse::Allow, None) => {
            args.extend(["--prerequisite-identity".into(), "host".into()])
        }
        (CredentialUse::Ask, _) => {}
    }
    match policy.ssh.use_ {
        CredentialUse::Deny => args.extend(["--prerequisite-ssh".into(), "deny".into()]),
        CredentialUse::Allow if policy.ssh.whole_dir => {
            args.extend(["--prerequisite-ssh".into(), "all".into()])
        }
        CredentialUse::Allow => {
            if let (Some(key), false) = (&policy.ssh.key, policy.ssh.key_is_suggestion) {
                args.extend(["--prerequisite-ssh".into(), key.display().to_string()]);
            }
        }
        CredentialUse::Ask => {}
    }
    match policy.github_token.use_ {
        CredentialUse::Deny => args.extend(["--prerequisite-token".into(), "deny".into()]),
        CredentialUse::Allow => match policy.github_token.env_vars.first() {
            Some(var) => args.extend(["--prerequisite-token".into(), format!("env:{var}")]),
            None if policy.github_token.acquire.is_some() => {
                args.extend(["--prerequisite-token".into(), "acquire".into()])
            }
            None => {}
        },
        CredentialUse::Ask => {}
    }
    match policy.unresolved {
        Some(UnresolvedMode::Warn) => {
            args.extend(["--prerequisite-unresolved".into(), "warn".into()])
        }
        Some(UnresolvedMode::Fail) => {
            args.extend(["--prerequisite-unresolved".into(), "fail".into()])
        }
        None => {}
    }
    args
}
