//! `awman config prerequisite-grants list|revoke`: the remembered workflow
//! prerequisite decisions in `~/.awman/prerequisite-grants.json`.
//!
//! The file holds decisions only (a key path, a variable name, an identity),
//! never a secret, so listing it shows everything it contains.

use serde::Serialize;

use crate::command::error::CommandError;
use crate::data::config::global::GlobalConfig;
use crate::data::fs::prerequisite_grants::{
    GrantSelector, PrerequisiteGrantStore, RememberedGrant, RememberedKind,
};
use crate::data::session::Session;

/// What `config prerequisite-grants` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrerequisiteGrantsAction {
    List,
    /// The raw selector: a 1-based record number or `all`. Parsed when the
    /// command runs, like `config get` validates its field.
    Revoke(String),
}

/// One remembered decision, numbered as `revoke` accepts it.
#[derive(Debug, Clone, Serialize)]
pub struct PrerequisiteGrantRow {
    /// 1-based, in file order.
    pub index: usize,
    pub git_root: String,
    pub remote_url: String,
    pub prerequisite: RememberedKind,
    /// "ssh key ~/.ssh/id_ed25519", "whole ~/.ssh", "token from GH_TOKEN".
    pub decision: String,
    pub recorded_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrerequisiteGrantsOutcome {
    pub path: String,
    /// The records left after the action.
    pub rows: Vec<PrerequisiteGrantRow>,
    /// How many records `revoke` removed; `None` for `list`.
    pub revoked: Option<usize>,
    /// Why the file was ignored (bad permissions, malformed, …), if it was.
    pub warning: Option<String>,
}

/// `revoke`'s positional: a 1-based index as listed, or `all`.
pub fn parse_selector(raw: &str) -> Result<GrantSelector, String> {
    let raw = raw.trim();
    if raw == "all" {
        return Ok(GrantSelector::All);
    }
    match raw.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(GrantSelector::Index(n)),
        _ => Err("expected a record number from `config prerequisite-grants list`, or all".into()),
    }
}

fn describe(grant: &RememberedGrant) -> String {
    match grant {
        RememberedGrant::SshKey { key } => format!("ssh key {}", key.display()),
        RememberedGrant::SshDir => "whole ~/.ssh".to_string(),
        RememberedGrant::TokenEnv { var } => format!("token from {var}"),
        RememberedGrant::Identity { name, email } => format!("identity {name} <{email}>"),
    }
}

/// Run `list` or `revoke` against the store under the session's data home.
pub fn run_prerequisite_grants(
    session: &Session,
    action: &PrerequisiteGrantsAction,
) -> Result<PrerequisiteGrantsOutcome, CommandError> {
    let data_home = GlobalConfig::data_home_with(session.env()).map_err(CommandError::from)?;
    let mut store = PrerequisiteGrantStore::load(&data_home);
    let revoked = match action {
        PrerequisiteGrantsAction::List => None,
        PrerequisiteGrantsAction::Revoke(raw) => {
            let selector =
                &parse_selector(raw).map_err(|reason| CommandError::InvalidArgumentValue {
                    command: vec![
                        "config".into(),
                        "prerequisite-grants".into(),
                        "revoke".into(),
                    ],
                    argument: "selector".into(),
                    reason,
                })?;
            if let GrantSelector::Index(n) = selector {
                if *n > store.records().len() {
                    return Err(CommandError::Other(format!(
                        "no remembered prerequisite decision number {n}; run `awman config prerequisite-grants list`"
                    )));
                }
            }
            Some(store.revoke(selector).map_err(CommandError::from)?)
        }
    };
    let rows = store
        .records()
        .iter()
        .enumerate()
        .map(|(i, record)| PrerequisiteGrantRow {
            index: i + 1,
            git_root: record.key.git_root.display().to_string(),
            remote_url: record.key.remote_url.clone(),
            prerequisite: record.key.prerequisite,
            decision: describe(&record.grant),
            recorded_at: record.recorded_at.clone(),
        })
        .collect();
    Ok(PrerequisiteGrantsOutcome {
        path: PrerequisiteGrantStore::path_for(&data_home)
            .display()
            .to_string(),
        rows,
        revoked,
        warning: store.load_warning.clone(),
    })
}
