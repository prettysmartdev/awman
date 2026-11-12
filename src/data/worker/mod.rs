//! Layer 0 worker state and the shared pushed/pulled job description.
//!
//! A [`Job`] describes *scheduling* only: where a command came from
//! ([`JobOrigin`]), what the executor needs to place it ([`JobSpec`]) and how
//! far it got ([`JobState`]). The command itself is an ordinary command row
//! (`SqliteSessionStore`) created through the Layer 2 Dispatch path; a job is
//! never a second command or session model. Pulled jobs additionally own a
//! [`LeaseRecord`] (`lease_…`) carrying the work server's own lease ID.
pub mod job_store;
pub mod labels;
pub mod lease_store;
pub mod link_store;
pub mod paths;
pub mod wire;

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::data::command_status::CommandStatus;
use crate::data::config::daemon::Interaction;
use crate::data::ids::LeaseId;

/// Where a job came from. The wire shape (`kind` tag) is the one 0143 fixes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobOrigin {
    Push {
        session_id: String,
        command_id: String,
    },
    Pull {
        server: String,
        lease_id: String,
        epoch: u64,
    },
}

impl JobOrigin {
    /// The executor class this origin schedules under.
    pub fn class(&self) -> JobClass {
        match self {
            Self::Push { .. } => JobClass::Push,
            Self::Pull { .. } => JobClass::Pull,
        }
    }
}

/// Scheduling class inside the worker feature's executor share. Pushed and
/// pulled jobs alternate so neither queue starves the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobClass {
    Push,
    Pull,
}

impl JobClass {
    pub const ALL: [Self; 2] = [Self::Push, Self::Pull];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Pull => "pull",
        }
    }
    pub fn other(self) -> Self {
        match self {
            Self::Push => Self::Pull,
            Self::Pull => Self::Push,
        }
    }
}

/// How far a job got. Push clients and work servers see the same states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Preparing,
    Running,
    AwaitingInput,
    Pushing,
    Done,
    Error,
    Cancelled,
}

impl JobState {
    /// Project a command row's status. A running row may be refined by the
    /// caller's live phase (`hint`): preparing, awaiting input or pushing.
    pub fn from_command(status: CommandStatus, hint: Option<JobState>) -> Self {
        match status {
            CommandStatus::Queued | CommandStatus::Pending => Self::Queued,
            CommandStatus::Running => match hint {
                Some(state @ (Self::Preparing | Self::AwaitingInput | Self::Pushing)) => state,
                _ => Self::Running,
            },
            CommandStatus::Done => Self::Done,
            CommandStatus::Error => Self::Error,
            CommandStatus::Cancelled => Self::Cancelled,
        }
    }
}

/// Where a job's command runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobWorkspace {
    /// An existing work directory (pushed local sessions).
    Local { workdir: PathBuf },
    /// A fresh clone (remote sessions and pulled leases).
    Clone {
        repo_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_ref: Option<String>,
        branch: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        merge_refs: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        push_branch: Option<String>,
        #[serde(default)]
        read_only: bool,
        work_root: PathBuf,
    },
}

/// What the executor needs to place a job. The command path and argv are
/// the catalogue command the Dispatch layer instantiates; this is a
/// description of that command, not a replacement for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    /// Catalogue path, e.g. `["exec", "workflow"]`.
    pub command: Vec<String>,
    /// Argument vector as stored on the command row.
    #[serde(default)]
    pub args: Vec<String>,
    /// Resolved interaction for the run.
    pub interaction: Interaction,
    pub workspace: JobWorkspace,
}

/// One job of the worker feature: a pushed command or a pulled lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// The command row ID (`cmd_…`).
    pub id: String,
    pub origin: JobOrigin,
    pub spec: JobSpec,
    pub state: JobState,
}

impl Job {
    pub fn class(&self) -> JobClass {
        self.origin.class()
    }
}

/// Lifecycle of an awman lease resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    Claimed,
    Preparing,
    Running,
    AwaitingInput,
    Pushing,
    Reporting,
    Done,
    Released,
    Lost,
}

impl LeaseState {
    pub const ACTIVE: &'static [LeaseState] = &[
        Self::Claimed,
        Self::Preparing,
        Self::Running,
        Self::AwaitingInput,
        Self::Pushing,
        Self::Reporting,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::AwaitingInput => "awaiting_input",
            Self::Pushing => "pushing",
            Self::Reporting => "reporting",
            Self::Done => "done",
            Self::Released => "released",
            Self::Lost => "lost",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Claimed,
            Self::Preparing,
            Self::Running,
            Self::AwaitingInput,
            Self::Pushing,
            Self::Reporting,
            Self::Done,
            Self::Released,
            Self::Lost,
        ]
        .into_iter()
        .find(|state| state.as_str() == s)
    }
    pub fn is_active(self) -> bool {
        Self::ACTIVE.contains(&self)
    }
}

/// Which process holds a lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeaseHost {
    Daemon,
    Tui,
    /// Consumed by command::worker TUI/CLI hosting through the singleton engine (0142).
    EngineInstance {
        instance_id: String,
    },
}

/// Why a lease was handed back, when it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRelease {
    pub reason: awman_wsp::ReleaseReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// awman's own lease resource. `item` is the claim-time snapshot with
/// `repo.credentials` removed: repository credentials are never persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub id: LeaseId,
    /// `workServers` key.
    pub server: String,
    pub server_lease_id: String,
    pub epoch: u64,
    pub item_id: String,
    pub item: awman_wsp::WorkItem,
    /// The command row (`cmd_…`) once admitted.
    pub job_id: Option<String>,
    pub state: LeaseState,
    pub host: LeaseHost,
    pub claimed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub terminal: bool,
    pub remote_input: bool,
    pub release: Option<LeaseRelease>,
}

impl LeaseRecord {
    /// A new `claimed` record for `lease`, minting a fresh `lease_…` ID and
    /// stripping the item's repository credentials.
    pub fn claimed(
        server: &str,
        lease: &awman_wsp::Lease,
        host: LeaseHost,
        now: DateTime<Utc>,
    ) -> Self {
        let mut item = lease.item.clone();
        item.repo.credentials = None;
        Self {
            id: LeaseId::new(),
            server: server.to_owned(),
            server_lease_id: lease.id.clone(),
            epoch: lease.epoch,
            item_id: item.id.clone(),
            item,
            job_id: None,
            state: LeaseState::Claimed,
            host,
            claimed_at: now,
            expires_at: lease.expires_at,
            ended_at: None,
            terminal: lease.terminal,
            remote_input: lease.remote_input,
            release: None,
        }
    }

    /// The pull origin of this lease's job.
    pub fn origin(&self) -> JobOrigin {
        JobOrigin::Pull {
            server: self.server.clone(),
            lease_id: self.id.to_string(),
            epoch: self.epoch,
        }
    }
}

pub mod queue;
