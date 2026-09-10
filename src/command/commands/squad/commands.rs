//! The single Layer-2 squad command family.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::squad::daemon::{
    SquadDaemonCommand, SquadDaemonOutcome, SquadDaemonSubcommand, SquadLogsFlags, SquadStartFlags,
    SquadStatusFlags, SquadStopFlags,
};
use crate::command::commands::squad::env_sync::sync_env;
use crate::command::commands::squad::gateway::{
    CreateTask, DaemonStatus, EnvCoverage, TaskDetail, TaskGateway, UpdateTask,
    DEFAULT_RUN_HISTORY_LIMIT, DEFAULT_WORKSPACE_FLAG_VALUE,
};
use crate::command::commands::Command;
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::data::config::env::host_var;
use crate::data::config::global::GlobalConfig;
use crate::data::fs::task_store::{MountScope, Task, TaskStatus, TaskWorkspace};
use crate::data::fs::SquadPaths;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::git::GitEngine;
use crate::engine::squad::env_state::{coverage_digest, EnvSource, Salt};

/// The two workspace choices offered at task creation.
///
/// Deliberately distinct from [`TaskWorkspace`]: this is what the *choice
/// step* answers, before any path has been collected. The path prompt is a
/// separate step, so a user who picks the default is never shown one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskWorkspaceChoice {
    /// Bind the task to its durable `~/.awman/squad/tasks/<name>/workspace/`.
    DefaultTaskWorkspace,
    /// Bind the task to a folder or repository the user names next.
    CustomFolderOrRepo,
}

#[derive(Debug, Clone)]
pub struct SquadServeConfig {
    pub port: u16,
    pub dangerously_skip_auth: bool,
}

#[async_trait]
pub trait SquadCommandFrontend: UserMessageSink + Send + Sync {
    /// The unattended frontends the daemon's evaluator drives leader agents
    /// and workflows with. Only the host that can actually serve the daemon
    /// supplies them; every other frontend answers `None` and is refused a
    /// foreground start.
    fn squad_run_frontends(
        &self,
    ) -> Option<std::sync::Arc<dyn crate::command::commands::squad::evaluation::SquadRunFrontends>>
    {
        None
    }

    /// Serve the bootstrapped daemon until shutdown. Layer 2 has already
    /// opened the store, reconciled orphaned runs and started the scheduler;
    /// the frontend builds a router over `_handles`, binds, and serves.
    async fn serve_squad_daemon(
        &mut self,
        _handles: crate::command::commands::squad::daemon_runtime::SquadDaemonHandles,
    ) -> Result<(), CommandError> {
        Err(CommandError::NotAvailableForFrontend {
            command: "squad start".into(),
            frontend: "this".into(),
        })
    }

    /// Disclose a squad bearer key this process just minted.
    ///
    /// Called by `Dispatch` immediately after the gateway is resolved, and
    /// exactly once per key: the plaintext exists nowhere else, so a frontend
    /// that drops it has lost it. The default does nothing, which is right
    /// only for a frontend with no user in front of it (the daemon's own).
    fn show_key_setup(
        &mut self,
        _setup: &crate::command::commands::squad::supervisor::SquadKeySetup,
    ) {
    }

    // ── Task-creation interview (BLOCKER-3, §9.3) ──────────────────────
    //
    // These COLLECT input; they must not validate or reject an answer — that
    // stays in `LocalTaskGateway::validate_create`. Each defaults to an
    // error so a non-interactive frontend (the daemon's API frontend) refuses
    // an interview rather than inventing answers.

    fn ask_task_name(&mut self) -> Result<String, CommandError> {
        Err(interview_unavailable())
    }
    fn ask_task_description(&mut self) -> Result<String, CommandError> {
        Err(interview_unavailable())
    }
    /// Raw interval spec (e.g. `6h`); Layer 2 parses and Layer 1 validates it.
    fn ask_task_interval(&mut self) -> Result<String, CommandError> {
        Err(interview_unavailable())
    }
    /// Which workspace the task is bound to: the durable per-task directory,
    /// or a folder/repo the user names. Asked after the description and before
    /// mount scope, so the user is never made to type a path they did not
    /// choose to type.
    fn ask_task_workspace_choice(&mut self) -> Result<TaskWorkspaceChoice, CommandError> {
        Err(interview_unavailable())
    }

    /// The custom workspace path. Only asked when
    /// [`ask_task_workspace_choice`](Self::ask_task_workspace_choice) chose
    /// [`TaskWorkspaceChoice::Custom`].
    fn ask_task_repo(&mut self) -> Result<PathBuf, CommandError> {
        Err(interview_unavailable())
    }

    /// Warn that a chosen custom path is not the root of a git repository, and
    /// ask whether to keep it. `true` keeps the path (the task then runs
    /// directly against that folder with no worktree); `false` loops back to
    /// the path prompt. Refusing by default keeps a non-interactive frontend
    /// from silently accepting a path it never showed anyone.
    fn confirm_non_git_workspace(&mut self, _path: &Path) -> Result<bool, CommandError> {
        Ok(false)
    }

    /// Confirm mounting a custom workspace that is a parent directory of the
    /// session's current directory. This is the same confirmation every other
    /// awman mount-scope flow applies before a parent directory is mounted
    /// (`aspec/architecture/security.md`); squad's custom-folder entry point
    /// does not get to bypass it. Refusing by default is the safe answer for a
    /// frontend that cannot ask.
    fn confirm_parent_directory_workspace(
        &mut self,
        _path: &Path,
        _current_dir: &Path,
    ) -> Result<bool, CommandError> {
        Ok(false)
    }

    /// One overlay spec in `--overlay` syntax, or `None` to finish. Called
    /// repeatedly until it answers `None`. Layer 1 validates the syntax at
    /// creation; this only collects.
    fn ask_task_overlay(&mut self, _existing: &[String]) -> Result<Option<String>, CommandError> {
        Ok(None)
    }
    fn ask_task_agent(&mut self) -> Result<Option<String>, CommandError> {
        Err(interview_unavailable())
    }
    fn ask_task_model(&mut self) -> Result<Option<String>, CommandError> {
        Err(interview_unavailable())
    }
    fn ask_task_mount_scope(&mut self) -> Result<MountScope, CommandError> {
        Err(interview_unavailable())
    }

    // ── Task agent pool (WI 0110) ──────────────────────────────────────
    //
    // These collect the task-scoped `config.json`. Like the steps above they
    // only collect: the map they build is validated by `SquadConfig::validate`
    // before the daemon writes it.

    /// Whether to keep using the global `squad` settings for this task. Only
    /// asked when a global `squad` block exists; `true` skips the two questions
    /// below and writes no task config. Defaulting to `true` means a frontend
    /// that cannot ask inherits the global pool, which is what every task did
    /// before task configs existed.
    fn ask_use_global_squad_config(&mut self) -> Result<bool, CommandError> {
        Ok(true)
    }

    /// One more model for `agent`, or `None` to finish. Called repeatedly, like
    /// [`ask_task_overlay`](Self::ask_task_overlay).
    fn ask_agent_model(
        &mut self,
        _agent: &str,
        _existing: &[String],
    ) -> Result<Option<String>, CommandError> {
        Ok(None)
    }

    /// One more agent this task may use, or `None` to finish. Its models are
    /// collected by [`ask_agent_model`](Self::ask_agent_model) straight after.
    fn ask_additional_agent(
        &mut self,
        _existing: &[String],
    ) -> Result<Option<String>, CommandError> {
        Ok(None)
    }

    // ── Task edit (WI 0110) ────────────────────────────────────────────
    //
    // The edit interview asks the same fields creation does, prefilled with
    // what the task currently carries, so submitting an unchanged box is a
    // no-op rather than a silent reset.

    fn ask_edited_description(&mut self, _current: &str) -> Result<String, CommandError> {
        Err(interview_unavailable())
    }
    fn ask_edited_interval(&mut self, _current: &str) -> Result<String, CommandError> {
        Err(interview_unavailable())
    }
    /// The leader agent as edited: `None` clears it back to the squad default.
    fn ask_edited_agent(&mut self, _current: Option<&str>) -> Result<Option<String>, CommandError> {
        Err(interview_unavailable())
    }
    /// The leader model as edited: `None` clears it back to the squad default.
    fn ask_edited_model(&mut self, _current: Option<&str>) -> Result<Option<String>, CommandError> {
        Err(interview_unavailable())
    }
    /// Whether to replace the task's overlays. `false` keeps the stored list
    /// untouched; `true` starts the ordinary overlay loop from empty, so the
    /// answer collected there replaces it wholesale.
    fn ask_replace_overlays(&mut self, _current: &[String]) -> Result<bool, CommandError> {
        Ok(false)
    }
    /// Whether to replace the task's agent pool. `false` keeps whatever
    /// `config.json` the task has (or has not) got.
    fn ask_replace_agent_pool(
        &mut self,
        _current: &std::collections::BTreeMap<String, Vec<String>>,
    ) -> Result<bool, CommandError> {
        Ok(false)
    }

    /// Whether this frontend is the user's own session on the host that chose
    /// the paths in the request — and therefore whether the process's current
    /// directory is the user's and a human is there to answer a mount-scope
    /// question.
    ///
    /// `false` for the daemon's API frontend, which re-executes a `squad add`
    /// a client already authorised, from a working directory unrelated to the
    /// caller's. Mount-scope policy that compares against the current directory
    /// is applied on the client, once, not again in the daemon.
    fn is_local_user_session(&self) -> bool {
        false
    }

    /// Ask whether to delete a task's persistent directory on `remove`
    /// (BLOCKER-2, §9.2). Defaults to `Ok(false)` so the daemon's API frontend
    /// never removes a directory; only an interactive frontend answers `true`.
    fn ask_delete_task_dir(&mut self, _name: &str, _path: &Path) -> Result<bool, CommandError> {
        Ok(false)
    }
}

fn interview_unavailable() -> CommandError {
    CommandError::NotAvailableForFrontend {
        command: "squad add --interview".into(),
        frontend: "this".into(),
    }
}

/// Fields for `squad add`. In interview mode Layer 2 collects every field
/// through the frontend's `ask_task_*` methods; otherwise `prefilled`
/// carries the flag-derived task assembled by Dispatch.
pub struct SquadAddRequest {
    pub interview: bool,
    /// `-n/--non-interactive`: never put a question to the user. The one
    /// question scripted creation can still raise is the parent-directory
    /// mount-scope confirmation; under this flag it is refused outright rather
    /// than asked, so a CI invocation cannot block on a prompt or silently
    /// widen a mount.
    pub non_interactive: bool,
    pub prefilled: Option<CreateTask>,
}

pub enum SquadSubcommand {
    Start(SquadStartFlags),
    Stop(SquadStopFlags),
    Status(SquadStatusFlags),
    Logs(SquadLogsFlags),
    Add(SquadAddRequest),
    /// Change an existing task (WI 0110). `interview` collects `update`'s
    /// fields through the frontend instead of taking them from flags.
    Edit {
        name: String,
        interview: bool,
        update: UpdateTask,
    },
    List,
    Show(String),
    Remove {
        name: String,
        yes: bool,
    },
    Pause(String),
    Resume(String),
    /// Evaluate a task on the next scheduler tick, whatever its interval and
    /// backoff say. Carries only the task name: a trigger has nothing to
    /// configure, and deliberately changes no stored schedule.
    Trigger(String),
    /// The whole picture of the daemon's env coverage (WI 0116 §6d).
    ///
    /// Bare, it reports; `push` forces a push of every locally-present
    /// required name (the ordinary path only sends what actually differs);
    /// `clear` removes the persisted keychain item. Values are never printed
    /// under any combination — only whether one is present.
    Env {
        push: bool,
        clear: bool,
    },
}

/// One row of `awman squad env` — a required name and what the daemon has for
/// it. **No field here can hold a value**, which is the point: the whole
/// command reports presence, never content.
#[derive(Debug, Clone, Serialize)]
pub struct EnvReportRow {
    pub name: String,
    /// `"set"` (the daemon holds a value), `"unmet"` (it does not and the name
    /// counts), or `"optional"` (a host-side name the daemon wants but never
    /// counts as unmet — see `HOST_SIDE_ENV_NAMES`).
    pub state: String,
    /// `"this shell"`, `"pushed"`, `"keychain"`, or `"—"` when nothing is held.
    ///
    /// The three held cases look identical without this column and decide
    /// something quite different: whether restarting the daemon loses the
    /// value, and whether this shell would re-supply it if it did.
    pub source: String,
    /// `unmet_since`, so "just typo'd it" and "broken for three days" are
    /// distinguishable at a glance.
    pub unmet_since: Option<chrono::DateTime<chrono::Utc>>,
    /// Task names that declare this variable, sorted.
    pub required_by: Vec<String>,
}

/// What `awman squad env` answers with.
#[derive(Debug, Clone, Serialize)]
pub struct EnvReport {
    /// `"keychain"`, `"none"`, or `"unavailable(<reason>)"`, rendered verbatim.
    pub persistence: String,
    /// `Some` only for `--clear`: whether a stored item was actually removed.
    /// `false` means there was no keychain backend to clear, which is not an
    /// error on a headless Linux box or on Windows.
    pub cleared: Option<bool>,
    /// One row per required name, in the order the daemon reported them
    /// (sorted by name).
    pub rows: Vec<EnvReportRow>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum SquadOutcome {
    Started {
        port: u16,
        background: bool,
        refreshed_key: bool,
    },
    Stopped {
        stopped_pid: Option<u32>,
    },
    Status(DaemonStatus),
    Logs {
        log_path: String,
    },
    Task(Task),
    Detail(TaskDetail),
    Tasks(Vec<Task>),
    /// A task asked to run ahead of its schedule by `squad trigger`. It
    /// names the task so a frontend can confirm *which* one was triggered
    /// without holding onto the request, and is distinct from
    /// [`SquadOutcome::Ok`] so "triggered" is never rendered as a bare
    /// success with nothing to say.
    Triggered {
        name: String,
    },
    /// A task as it stands after `squad edit` (WI 0110). Distinct from
    /// [`SquadOutcome::Task`] so a frontend can say "updated" rather than
    /// "created" without inspecting anything.
    Updated(Task),
    Removed {
        name: String,
        /// The persistent task directory that was deleted, when one was.
        /// `None` means the directory was kept (declined) or absent.
        removed_dir: Option<PathBuf>,
    },
    /// The daemon's env coverage (WI 0116 §6d). Names, states and timestamps
    /// only — never a value.
    Env(EnvReport),
    Ok,
}

pub struct SquadCommand {
    sub: SquadSubcommand,
    gateway: Option<Box<dyn TaskGateway>>,
    engines: Engines,
}

impl SquadCommand {
    pub fn new(
        sub: SquadSubcommand,
        gateway: Option<Box<dyn TaskGateway>>,
        engines: Engines,
    ) -> Self {
        Self {
            sub,
            gateway,
            engines,
        }
    }

    pub fn subcommand(&self) -> &SquadSubcommand {
        &self.sub
    }

    /// Construct from the catalogue-resolved input (WI 0113 F-10).
    ///
    /// Every `squad` leaf shares this entry point, selected by the caller's
    /// canonical path. The gateway is whatever `Dispatch::admit` resolved for
    /// the spec's `GatewayNeed`; the leaves that declare `GatewayNeed::None`
    /// simply never read it. `--interval` (`"6h"`), `--mount-scope`
    /// (`"gitroot"`) and both `--port`s come from the catalogue.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let sub = match ctx.caller.leaf() {
            "start" => SquadSubcommand::Start(SquadStartFlags {
                port: ctx.flags.require_u16("port")?,
                background: ctx.flags.bool("background"),
                refresh_key: ctx.flags.bool("refresh-key"),
                dangerously_skip_auth: ctx.flags.bool("dangerously-skip-auth"),
            }),
            "stop" => SquadSubcommand::Stop(SquadStopFlags),
            // Carries the gateway so Layer 2 can overlay live scheduler counts
            // onto the pidfile-derived liveness (§9.4).
            "squad" | "status" => SquadSubcommand::Status(SquadStatusFlags),
            "logs" => SquadSubcommand::Logs(SquadLogsFlags {
                follow: ctx.flags.bool("follow"),
            }),
            "add" => SquadSubcommand::Add(squad_add_request(ctx)?),
            "edit" => SquadSubcommand::Edit {
                name: ctx.args.require("name")?,
                interview: ctx.flags.bool("interview"),
                update: squad_update(ctx)?,
            },
            "list" => SquadSubcommand::List,
            "show" => SquadSubcommand::Show(ctx.args.require("name")?),
            "remove" => SquadSubcommand::Remove {
                name: ctx.args.require("name")?,
                yes: ctx.flags.bool("yes"),
            },
            "pause" => SquadSubcommand::Pause(ctx.args.require("name")?),
            "resume" => SquadSubcommand::Resume(ctx.args.require("name")?),
            "trigger" => SquadSubcommand::Trigger(ctx.args.require("name")?),
            "env" => SquadSubcommand::Env {
                push: ctx.flags.bool("push"),
                clear: ctx.flags.bool("clear"),
            },
            _ => return Err(CommandError::unknown_command(&ctx.path())),
        };
        Ok(Self::new(sub, ctx.boxed_gateway(), ctx.engines.clone()))
    }
}

/// Assemble `squad add`'s request.
///
/// Interview mode collects every field in Layer 2 through the frontend trait
/// (BLOCKER-3, §9.3), so only the intent is recorded here. Non-interview keeps
/// the flag-driven behaviour: required name and description, catalogue
/// defaults for the rest.
fn squad_add_request(ctx: &BuildContext) -> Result<SquadAddRequest, CommandError> {
    // `-n` never reaches the interview (the catalogue makes the two flags
    // conflict); it governs the one confirmation scripted creation can still
    // raise — the parent-directory mount scope.
    let non_interactive = ctx.flags.bool("non-interactive");
    if ctx.flags.bool("interview") {
        return Ok(SquadAddRequest {
            interview: true,
            non_interactive,
            prefilled: None,
        });
    }
    // `--workspace` is the scripted equivalent of the interview's
    // workspace-choice step: `default` binds the task to its durable per-task
    // workspace, anything else is a custom folder or repo. `--repo` predates
    // it and is kept as the same thing said differently, so an existing
    // scripted `--repo <path>` still means "use that path"; `--workspace` wins
    // when both are given.
    let workspace = match ctx.flags.str("workspace") {
        Some(DEFAULT_WORKSPACE_FLAG_VALUE) => TaskWorkspace::Default,
        Some(path) => TaskWorkspace::Custom(PathBuf::from(path)),
        None => match ctx.flags.path("repo") {
            Some(repo) => TaskWorkspace::Custom(repo),
            None => TaskWorkspace::Default,
        },
    };
    let mount_scope = match ctx.flags.r#enum("mount-scope") {
        Some("cwd") => MountScope::Cwd,
        Some("gitroot") => MountScope::GitRoot,
        other => unreachable!("catalogue enum validation admitted {other:?}"),
    };
    Ok(SquadAddRequest {
        interview: false,
        non_interactive,
        prefilled: Some(CreateTask {
            name: ctx.flags.require_str("name")?,
            description: ctx.flags.require_str("description")?,
            workspace,
            mount_scope,
            interval_secs: crate::command::dispatch::parse_squad_interval(
                &ctx.path(),
                &ctx.flags.require_str("interval")?,
            )?,
            agent: ctx.flags.string("agent"),
            model: ctx.flags.string("model"),
            // Raw specs only; syntax is validated once, in the gateway, before
            // anything is persisted.
            overlays: ctx.flags.strs("overlay").to_vec(),
            agents_to_models: crate::command::dispatch::parse_squad_agent_models(
                &ctx.path(),
                ctx.flags.strs("agent-models"),
            )?,
        }),
    })
}

/// Assemble `squad edit`'s update.
///
/// In interview mode Layer 2 collects the fields through the frontend,
/// prefilled from the task as it stands, so nothing is assembled here.
fn squad_update(ctx: &BuildContext) -> Result<UpdateTask, CommandError> {
    if ctx.flags.bool("interview") {
        return Ok(UpdateTask::default());
    }
    // A `--clear-*` flag and its value flag conflict in the catalogue, so at
    // most one of each pair is present and "clear" and "set" can never
    // disagree here.
    let agent = match ctx.flags.string("agent") {
        Some(agent) => Some(Some(agent)),
        None if ctx.flags.bool("clear-agent") => Some(None),
        None => None,
    };
    let model = match ctx.flags.string("model") {
        Some(model) => Some(Some(model)),
        None if ctx.flags.bool("clear-model") => Some(None),
        None => None,
    };
    let overlay_specs = ctx.flags.strs("overlay");
    let overlays = if !overlay_specs.is_empty() {
        Some(overlay_specs.to_vec())
    } else if ctx.flags.bool("clear-overlays") {
        Some(Vec::new())
    } else {
        None
    };
    let pool_specs = ctx.flags.strs("agent-models");
    let agents_to_models = if !pool_specs.is_empty() {
        Some(crate::command::dispatch::parse_squad_agent_models(
            &ctx.path(),
            pool_specs,
        )?)
    } else if ctx.flags.bool("clear-agent-models") {
        Some(Default::default())
    } else {
        None
    };
    Ok(UpdateTask {
        description: ctx.flags.string("description"),
        interval_secs: ctx
            .flags
            .str("interval")
            .map(|raw| crate::command::dispatch::parse_squad_interval(&ctx.path(), raw))
            .transpose()?,
        agent,
        model,
        overlays,
        agents_to_models,
    })
}

#[async_trait]
impl Command for SquadCommand {
    type Frontend = Box<dyn SquadCommandFrontend>;
    type Outcome = SquadOutcome;
    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let SquadCommand {
            sub,
            gateway,
            engines,
        } = self;
        match sub {
            SquadSubcommand::Start(flags) => daemon_outcome(
                SquadDaemonCommand::new(SquadDaemonSubcommand::Start(flags), engines)
                    .run_with_frontend(frontend)
                    .await?,
            ),
            SquadSubcommand::Stop(flags) => daemon_outcome(
                SquadDaemonCommand::new(SquadDaemonSubcommand::Stop(flags), engines)
                    .run_with_frontend(frontend)
                    .await?,
            ),
            SquadSubcommand::Status(flags) => {
                // Liveness, PID and bound address come from the pidfile/sidecar
                // (correct even when the daemon is down). A gateway is injected
                // only when the daemon has published its endpoint, so its
                // presence is the "daemon reachable" signal: overlay the live
                // scheduler counts from `gateway.status()`. A stopped daemon
                // means no gateway (no HTTP call); a present-but-failing gateway
                // degrades to the pidfile-only answer rather than failing (§9.4).
                let outcome =
                    SquadDaemonCommand::new(SquadDaemonSubcommand::Status(flags), engines)
                        .run_with_frontend(frontend)
                        .await?;
                let SquadDaemonOutcome::Status(mut status) = outcome else {
                    unreachable!("status subcommand yields a status outcome");
                };
                if let Some(gateway) = &gateway {
                    if let Ok(live) = gateway.status().await {
                        status.running = live.running;
                        status.task_count = live.task_count;
                        status.active_count = live.active_count;
                        status.last_tick = live.last_tick;
                        status.in_flight = live.in_flight;
                        // Both are daemon-only facts (WI 0116 §5a, §6b): the
                        // pidfile sidecar cannot know either, and leaving them
                        // at their defaults would silently report full
                        // coverage for a daemon that just said otherwise.
                        status.env_persistence = live.env_persistence;
                        status.unmet_env = live.unmet_env;
                    }
                }
                Ok(SquadOutcome::Status(status))
            }
            SquadSubcommand::Logs(flags) => daemon_outcome(
                SquadDaemonCommand::new(SquadDaemonSubcommand::Logs(flags), engines)
                    .run_with_frontend(frontend)
                    .await?,
            ),
            sub => {
                let gateway = gateway.ok_or_else(|| CommandError::Other("squad tasks are served by the squad daemon; start it with `awman squad start`".into()))?;
                match sub {
                    SquadSubcommand::Add(request) => {
                        // In interview mode Layer 2 collects every field through
                        // the frontend; otherwise Dispatch already assembled the
                        // task from flags. Validation stays in Layer 1's
                        // `LocalTaskGateway::validate_create`.
                        let req = match request.prefilled {
                            Some(req) => {
                                // The interview asks this inside its own path
                                // loop (so a refusal can offer a different
                                // path); the scripted path has no loop, so the
                                // same policy is applied here. Both entry
                                // points therefore go through
                                // `workspace_is_parent_of`, and neither can
                                // mount a parent directory unconfirmed.
                                confirm_scripted_workspace_scope(
                                    frontend.as_mut(),
                                    &req.workspace,
                                    std::env::current_dir().ok().as_deref(),
                                    request.non_interactive,
                                )?;
                                req
                            }
                            None => collect_task_interview(
                                frontend.as_mut(),
                                engines.git_engine.as_ref(),
                            )?,
                        };
                        let task = gateway.create(req).await?;
                        warn_unmet_env(frontend.as_mut(), &task, TaskChange::Created);
                        Ok(SquadOutcome::Task(task))
                    }
                    SquadSubcommand::Edit {
                        name,
                        interview,
                        update,
                    } => {
                        let update = if interview {
                            // The interview needs the task as it stands to
                            // prefill its prompts, so it starts from a read
                            // rather than from the flags.
                            let current = gateway.get(&name).await?;
                            let pool = read_task_agent_pool(&name)?;
                            collect_task_edit_interview(frontend.as_mut(), &current, &pool)?
                        } else {
                            update
                        };
                        // Refusing an empty edit is the point: a request that
                        // changes nothing would otherwise report success while
                        // only moving `updated_at`.
                        if update.is_empty() {
                            return Err(CommandError::Other(format!(
                                "squad edit {name}: nothing to change — pass at least one field \
                                 flag (--description/--interval/--agent/--model/--overlay/\
                                 --agent-models or a --clear-* flag), or use --interview"
                            )));
                        }
                        let task = gateway.update(&name, update).await?;
                        warn_unmet_env(frontend.as_mut(), &task, TaskChange::Updated);
                        Ok(SquadOutcome::Updated(task))
                    }
                    SquadSubcommand::List => Ok(SquadOutcome::Tasks(gateway.list().await?)),
                    SquadSubcommand::Show(name) => {
                        // One response shape for both gateways: the task
                        // and its recent runs travel together, so the remote
                        // façade never has to guess which type came back.
                        let task = gateway.get(&name).await?;
                        let runs = gateway.runs(&name, DEFAULT_RUN_HISTORY_LIMIT).await?;
                        Ok(SquadOutcome::Detail(TaskDetail { task, runs }))
                    }
                    SquadSubcommand::Remove { name, yes } => {
                        gateway.delete(&name).await?;
                        // The persistent directory removal is a filesystem
                        // concern that lives here, in Layer 2, guarded by the
                        // frontend's confirmation answer (or `-y`). The path is
                        // resolved through `SquadPaths::task_dir`, which is
                        // `validate_under_root`-guarded, so a crafted name can
                        // never escape the tasks root.
                        let removed_dir = remove_task_dir(frontend.as_mut(), &name, yes)?;
                        Ok(SquadOutcome::Removed { name, removed_dir })
                    }
                    SquadSubcommand::Pause(name) => {
                        gateway.set_status(&name, TaskStatus::Paused).await?;
                        Ok(SquadOutcome::Ok)
                    }
                    SquadSubcommand::Resume(name) => {
                        gateway.set_status(&name, TaskStatus::Active).await?;
                        Ok(SquadOutcome::Ok)
                    }
                    SquadSubcommand::Trigger(name) => {
                        gateway.trigger(&name).await?;
                        Ok(SquadOutcome::Triggered { name })
                    }
                    SquadSubcommand::Env { push, clear } => {
                        // `--clear` first: the report that follows then shows
                        // the state the user is left in rather than the one
                        // they asked to leave. Clearing removes only what is
                        // *stored* — the running daemon keeps its overlay, so
                        // opting out of persistence never disarms a daemon
                        // that is working fine.
                        let cleared = if clear {
                            Some(gateway.clear_env_store().await?)
                        } else {
                            None
                        };
                        // The bare form needs no push of its own: every keyed
                        // gateway has already run `sync_env(force = false)` in
                        // `SquadGatewayResolver`, so the digests are current by
                        // the time this command body runs. `--push` is the one
                        // case that re-sends regardless of digest.
                        if push {
                            sync_env(gateway.as_ref(), true).await;
                        }
                        let coverage = gateway.env_coverage().await?;
                        Ok(SquadOutcome::Env(env_report(&coverage, cleared)))
                    }
                    _ => unreachable!("daemon commands handled above"),
                }
            }
        }
    }
}

/// Collect a full `CreateTask` from the frontend's interview answers.
/// Every field is asked; the frontend supplies the values and Layer 1 validates
/// them, so this reproduces the same task from CLI and TUI given the same
/// answers.
///
/// Nothing is persisted here, and nothing reaches the store until every step —
/// including the workspace choice and the overlay loop — has been answered. An
/// interview abandoned partway (Ctrl-C, a dismissed dialog) propagates its
/// error out of this function, so a partial task can never be written.
fn collect_task_interview(
    frontend: &mut dyn SquadCommandFrontend,
    git_engine: &GitEngine,
) -> Result<CreateTask, CommandError> {
    let name = frontend.ask_task_name()?;
    let description = frontend.ask_task_description()?;
    let interval_raw = frontend.ask_task_interval()?;
    let interval_secs =
        crate::command::dispatch::parse_squad_interval(&["squad", "add"], &interval_raw)?;
    let workspace = collect_workspace_choice(frontend, git_engine)?;
    let overlays = collect_overlays(frontend)?;
    let agent = frontend.ask_task_agent()?;
    let model = frontend.ask_task_model()?;
    let agents_to_models = collect_agent_pool(frontend, agent.as_deref())?;
    // The mount scope only distinguishes anything inside a git repository. A
    // default or non-repo custom workspace has one possible answer, so asking
    // would be a prompt with no alternative; the gateway overrides it with
    // `MountScope::Directory` in that case regardless.
    let mount_scope = match &workspace {
        TaskWorkspace::Default => MountScope::Directory,
        TaskWorkspace::Custom(_) => frontend.ask_task_mount_scope()?,
    };
    Ok(CreateTask {
        name,
        description,
        workspace,
        mount_scope,
        interval_secs,
        agent,
        model,
        overlays,
        agents_to_models,
    })
}

/// Collect the task's own agent pool (WI 0110).
///
/// Asked in three steps, and only the ones that can matter:
///
/// 1. When a global `squad` block already exists, the user is asked whether to
///    use it. Answering yes returns an empty map — "no task config" — so the
///    task inherits that block whole, exactly as every task did before task
///    configs existed. With no global block there is nothing to inherit, so the
///    question is skipped rather than offered as a choice between one option.
/// 2. Extra models for the leader agent, whichever agent the task chose (or the
///    one `squad.defaultLeader` names when the task chose none). This is the
///    "additional model options for the default agent" half.
/// 3. Extra agents, each followed by its own models.
///
/// A run of "no" answers yields an empty map and no file is written, so the
/// interview is only as long as the user makes it.
fn collect_agent_pool(
    frontend: &mut dyn SquadCommandFrontend,
    task_agent: Option<&str>,
) -> Result<BTreeMap<String, Vec<String>>, CommandError> {
    let global = GlobalConfig::load().unwrap_or_default().squad;
    if global.is_some() && frontend.ask_use_global_squad_config()? {
        return Ok(BTreeMap::new());
    }

    let mut pool: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Which agent the "default agent" questions are about: the task's own, else
    // whatever `squad.defaultLeader` names. With neither, there is no agent to
    // add models *to*, so step 2 is skipped and only extra agents are asked for.
    let leader = task_agent.map(str::to_string).or_else(|| {
        global
            .as_ref()
            .and_then(|cfg| cfg.default_leader.as_deref())
            .map(|spec| {
                spec.split_once("::")
                    .map(|(agent, _)| agent)
                    .unwrap_or(spec)
                    .to_string()
            })
    });
    if let Some(leader) = leader {
        let models = collect_models_for_agent(frontend, &leader)?;
        if !models.is_empty() {
            pool.insert(leader, models);
        }
    }

    loop {
        let known: Vec<String> = pool.keys().cloned().collect();
        let Some(agent) = frontend.ask_additional_agent(&known)? else {
            break;
        };
        let models = collect_models_for_agent(frontend, &agent)?;
        // An agent with no models still belongs in the pool: it names an agent
        // the leader may use, with its model left to the agent's own default.
        pool.entry(agent).or_default().extend(models);
    }
    Ok(pool)
}

/// The model loop for one agent: ask until a blank answer ends it.
fn collect_models_for_agent(
    frontend: &mut dyn SquadCommandFrontend,
    agent: &str,
) -> Result<Vec<String>, CommandError> {
    let mut models: Vec<String> = Vec::new();
    while let Some(model) = frontend.ask_agent_model(agent, &models)? {
        if !models.iter().any(|existing| existing == &model) {
            models.push(model);
        }
    }
    Ok(models)
}

/// The agent pool a task currently carries, read from its own `config.json`.
/// Empty when the task has none (it inherits the global block) — the same
/// answer the interview's "use the global settings" branch produces.
fn read_task_agent_pool(name: &str) -> Result<BTreeMap<String, Vec<String>>, CommandError> {
    let paths = SquadPaths::from_process_env()?;
    let document = GlobalConfig::load_path(&paths.task_config_file(name)?)?;
    Ok(document
        .squad
        .and_then(|squad| squad.agents_to_models)
        .map(|map| map.into_iter().collect())
        .unwrap_or_default())
}

/// Collect an edit interactively, prefilled from the task as it stands.
///
/// Every step returns the current value unchanged when the user submits the box
/// as-is, and only fields that actually differ reach the [`UpdateTask`] — so an
/// interview the user walks through without changing anything is refused as an
/// empty edit rather than silently rewriting the task with its own values.
fn collect_task_edit_interview(
    frontend: &mut dyn SquadCommandFrontend,
    current: &Task,
    current_pool: &BTreeMap<String, Vec<String>>,
) -> Result<UpdateTask, CommandError> {
    let mut update = UpdateTask::default();

    let description = frontend.ask_edited_description(&current.description)?;
    if description != current.description {
        update.description = Some(description);
    }

    let interval_raw = frontend.ask_edited_interval(&format!("{}s", current.interval_secs))?;
    let interval_secs =
        crate::command::dispatch::parse_squad_interval(&["squad", "edit"], &interval_raw)?;
    if interval_secs != current.interval_secs {
        update.interval_secs = Some(interval_secs);
    }

    let agent = frontend.ask_edited_agent(current.agent.as_deref())?;
    if agent != current.agent {
        update.agent = Some(agent);
    }
    let model = frontend.ask_edited_model(current.model.as_deref())?;
    if model != current.model {
        update.model = Some(model);
    }

    // Overlays and the agent pool are list-valued, so they are replace-or-keep
    // rather than prefilled: there is no sensible "edit this list in place"
    // prompt, and a blank answer in the collection loops already means "done".
    if frontend.ask_replace_overlays(&current.overlays)? {
        let overlays = collect_overlays(frontend)?;
        if overlays != current.overlays {
            update.overlays = Some(overlays);
        }
    }
    if frontend.ask_replace_agent_pool(current_pool)? {
        let pool = collect_agent_pool(frontend, update_agent_or(&update, current))?;
        if &pool != current_pool {
            update.agents_to_models = Some(pool);
        }
    }

    Ok(update)
}

/// The leader agent an in-progress edit will end up with: the edited value
/// where the edit set one, else what the task already carries. The agent-pool
/// questions are about that agent, so they must see the edit's answer rather
/// than the stale stored one.
fn update_agent_or<'a>(update: &'a UpdateTask, current: &'a Task) -> Option<&'a str> {
    match &update.agent {
        Some(edited) => edited.as_deref(),
        None => current.agent.as_deref(),
    }
}

/// The two-choice workspace step, plus the custom path's warn/keep loop.
///
/// A path that does not exist is a hard error (there is nothing to mount, and
/// an arbitrary user-entered path is never silently created — unlike the
/// default workspace, which awman owns). A path that exists but is not a git
/// root only warns, and the user chooses to keep it or name a different one. A
/// path that would mount a parent directory of the session's current directory
/// goes through the same confirmation every other awman mount-scope flow uses.
fn collect_workspace_choice(
    frontend: &mut dyn SquadCommandFrontend,
    git_engine: &GitEngine,
) -> Result<TaskWorkspace, CommandError> {
    if matches!(
        frontend.ask_task_workspace_choice()?,
        TaskWorkspaceChoice::DefaultTaskWorkspace
    ) {
        return Ok(TaskWorkspace::Default);
    }
    let current_dir = std::env::current_dir().ok();
    loop {
        let path = frontend.ask_task_repo()?;
        let canonical = match std::fs::canonicalize(&path) {
            Ok(canonical) => canonical,
            // Not "warn and offer to keep": there is nothing at this path to
            // mount at all, so the task could never run.
            Err(error) => {
                return Err(CommandError::Other(format!(
                    "task workspace {} does not exist: {error}",
                    path.display()
                )));
            }
        };
        if !canonical.is_dir() {
            return Err(CommandError::Other(format!(
                "task workspace {} is not a directory",
                canonical.display()
            )));
        }
        if let Some(cwd) = current_dir.as_deref() {
            if workspace_is_parent_of(&canonical, cwd)
                && !frontend.confirm_parent_directory_workspace(&canonical, cwd)?
            {
                continue;
            }
        }
        // The repository detector is `GitEngine::resolve_root` — the same one
        // the gateway's workspace resolution and every run's session opening
        // use — so the interview's warning, the stored `MountScope`, and what
        // actually happens at launch can never disagree about what counts as a
        // repository root.
        let is_git_root =
            resolved_git_root(git_engine, &canonical).as_deref() == Some(canonical.as_path());
        if is_git_root || frontend.confirm_non_git_workspace(&canonical)? {
            return Ok(TaskWorkspace::Custom(canonical));
        }
        // Declined: loop back to the path prompt.
    }
}

/// Whether `workspace` strictly contains `current_dir` — i.e. mounting it would
/// widen the container's view to a parent of where the user is standing.
///
/// The one place this comparison is made, so the interview's in-loop prompt and
/// the scripted gate below cannot drift apart.
pub(crate) fn workspace_is_parent_of(workspace: &Path, current_dir: &Path) -> bool {
    let cwd = std::fs::canonicalize(current_dir).unwrap_or_else(|_| current_dir.to_path_buf());
    cwd != workspace && cwd.starts_with(workspace)
}

/// Apply the parent-directory mount policy to a scripted (`--workspace <path>`)
/// creation, which has no interview loop to ask inside.
///
/// Only a frontend that actually represents the user's own shell session runs
/// this: the daemon re-executes the very same `squad add` from its own working
/// directory, which has nothing to do with the caller's, so asking there would
/// compare the wrong two paths and (with a frontend that cannot prompt) refuse
/// a request the user already authorised on the client.
fn confirm_scripted_workspace_scope(
    frontend: &mut dyn SquadCommandFrontend,
    workspace: &TaskWorkspace,
    current_dir: Option<&Path>,
    non_interactive: bool,
) -> Result<(), CommandError> {
    if !frontend.is_local_user_session() {
        return Ok(());
    }
    let TaskWorkspace::Custom(path) = workspace else {
        return Ok(());
    };
    let Some(current_dir) = current_dir else {
        return Ok(());
    };
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
    if !workspace_is_parent_of(&canonical, current_dir) {
        return Ok(());
    }
    // `-n` means never ask: the widening is refused rather than prompted for,
    // so a scripted run neither blocks on a prompt nor widens a mount silently.
    if !non_interactive && frontend.confirm_parent_directory_workspace(&canonical, current_dir)? {
        return Ok(());
    }
    Err(CommandError::Other(format!(
        "task workspace {} is a parent directory of {}; \
         re-run with a workspace inside it, or confirm the wider mount scope",
        canonical.display(),
        current_dir.display()
    )))
}

/// The git root enclosing `path`, canonicalised so it is comparable with the
/// canonical path the caller resolved. `None` when `path` is not in a
/// repository.
pub(crate) fn resolved_git_root(git_engine: &GitEngine, path: &Path) -> Option<PathBuf> {
    let root = git_engine.resolve_root(path).ok()?;
    Some(std::fs::canonicalize(&root).unwrap_or(root))
}

/// The optional, repeating overlay step. Loops until the frontend answers
/// `None` (a blank submission). Blank entries are skipped rather than stored,
/// and syntax is validated once, in Layer 1, before anything is persisted.
fn collect_overlays(frontend: &mut dyn SquadCommandFrontend) -> Result<Vec<String>, CommandError> {
    let mut overlays: Vec<String> = Vec::new();
    while let Some(spec) = frontend.ask_task_overlay(&overlays)? {
        let spec = spec.trim().to_string();
        if spec.is_empty() {
            break;
        }
        overlays.push(spec);
    }
    Ok(overlays)
}

#[cfg(test)]
mod workspace_choice_tests {
    use super::*;
    use std::collections::VecDeque;

    use crate::data::message::{UserMessage, UserMessageSink};

    struct WorkspaceFrontend {
        paths: VecDeque<PathBuf>,
        keep_non_git: VecDeque<bool>,
        non_git_prompts: usize,
    }

    impl WorkspaceFrontend {
        fn custom(paths: impl IntoIterator<Item = PathBuf>, keep_non_git: Vec<bool>) -> Self {
            Self {
                paths: paths.into_iter().collect(),
                keep_non_git: keep_non_git.into(),
                non_git_prompts: 0,
            }
        }
    }

    impl UserMessageSink for WorkspaceFrontend {
        fn write_message(&mut self, _message: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    #[async_trait]
    impl SquadCommandFrontend for WorkspaceFrontend {
        fn ask_task_workspace_choice(&mut self) -> Result<TaskWorkspaceChoice, CommandError> {
            Ok(TaskWorkspaceChoice::CustomFolderOrRepo)
        }

        fn ask_task_repo(&mut self) -> Result<PathBuf, CommandError> {
            self.paths.pop_front().ok_or_else(|| {
                CommandError::Other(
                    "test frontend was asked for an unexpected replacement workspace".into(),
                )
            })
        }

        fn confirm_non_git_workspace(&mut self, _path: &Path) -> Result<bool, CommandError> {
            self.non_git_prompts += 1;
            self.keep_non_git.pop_front().ok_or_else(|| {
                CommandError::Other(
                    "test frontend was asked for an unexpected non-git confirmation".into(),
                )
            })
        }
    }

    fn git_init(dir: &Path) {
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .status()
            .expect("git must be available to exercise repository detection");
        assert!(status.success(), "git init failed in {}", dir.display());
    }

    #[test]
    fn a_real_git_repository_root_is_accepted_without_a_non_git_warning() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        let mut frontend = WorkspaceFrontend::custom([tmp.path().to_path_buf()], vec![]);

        let workspace = collect_workspace_choice(&mut frontend, &GitEngine::new()).unwrap();
        assert_eq!(
            workspace,
            TaskWorkspace::Custom(tmp.path().canonicalize().unwrap())
        );
        assert_eq!(frontend.non_git_prompts, 0);
    }

    /// A bare `.git` directory with no repository inside it is what an
    /// ancestor-walking `.git`-exists probe would call a repository. The real
    /// detector (`GitEngine::resolve_root`, the same one the run path uses)
    /// rejects it, so creation must warn rather than silently capturing a
    /// worktree-isolated scope that would fail at launch.
    #[test]
    fn a_malformed_git_marker_is_not_treated_as_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let mut frontend = WorkspaceFrontend::custom([tmp.path().to_path_buf()], vec![true]);

        let workspace = collect_workspace_choice(&mut frontend, &GitEngine::new()).unwrap();
        assert_eq!(
            workspace,
            TaskWorkspace::Custom(tmp.path().canonicalize().unwrap())
        );
        assert_eq!(
            frontend.non_git_prompts, 1,
            "a malformed .git marker must still raise the not-a-repository warning"
        );
    }

    #[test]
    fn non_git_custom_workspace_warns_and_loops_when_user_changes_their_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let mut frontend =
            WorkspaceFrontend::custom([first.clone(), second.clone()], vec![false, true]);

        let workspace = collect_workspace_choice(&mut frontend, &GitEngine::new()).unwrap();
        assert_eq!(
            workspace,
            TaskWorkspace::Custom(second.canonicalize().unwrap())
        );
        assert_eq!(
            frontend.non_git_prompts, 2,
            "the declined warning must return to the path prompt"
        );
    }

    #[test]
    fn nonexistent_custom_workspace_is_rejected_without_a_keep_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let mut frontend = WorkspaceFrontend::custom([missing.clone()], vec![]);

        let error = collect_workspace_choice(&mut frontend, &GitEngine::new()).unwrap_err();
        assert!(error.to_string().contains("does not exist"), "{error}");
        assert_eq!(frontend.non_git_prompts, 0);
    }
}

#[cfg(test)]
mod scripted_workspace_scope_tests {
    use super::*;

    use crate::data::message::{UserMessage, UserMessageSink};

    /// A frontend that answers like a user's own shell session: it is asked,
    /// and it says no.
    struct RefusingLocalFrontend {
        asked: usize,
    }

    impl UserMessageSink for RefusingLocalFrontend {
        fn write_message(&mut self, _message: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    #[async_trait]
    impl SquadCommandFrontend for RefusingLocalFrontend {
        fn is_local_user_session(&self) -> bool {
            true
        }
        fn confirm_parent_directory_workspace(
            &mut self,
            _path: &Path,
            _current_dir: &Path,
        ) -> Result<bool, CommandError> {
            self.asked += 1;
            Ok(false)
        }
    }

    /// The daemon's frontend: it cannot ask, and its working directory has
    /// nothing to do with the caller's, so it must not re-apply the policy.
    struct DaemonFrontend;

    impl UserMessageSink for DaemonFrontend {
        fn write_message(&mut self, _message: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    #[async_trait]
    impl SquadCommandFrontend for DaemonFrontend {}

    #[test]
    fn a_scripted_parent_directory_workspace_is_refused_when_the_user_declines() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().canonicalize().unwrap();
        let child = parent.join("child");
        std::fs::create_dir(&child).unwrap();

        let mut frontend = RefusingLocalFrontend { asked: 0 };
        let error = confirm_scripted_workspace_scope(
            &mut frontend,
            &TaskWorkspace::Custom(parent.clone()),
            Some(&child),
            false,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("parent directory"),
            "scripted creation must refuse an unconfirmed parent mount: {error}"
        );
        assert_eq!(frontend.asked, 1);
    }

    #[test]
    fn a_scripted_workspace_inside_the_current_directory_is_never_questioned() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let child = root.join("child");
        std::fs::create_dir(&child).unwrap();
        let mut frontend = RefusingLocalFrontend { asked: 0 };
        confirm_scripted_workspace_scope(
            &mut frontend,
            &TaskWorkspace::Custom(child.clone()),
            Some(&child),
            false,
        )
        .unwrap();
        assert_eq!(frontend.asked, 0);
        confirm_scripted_workspace_scope(
            &mut frontend,
            &TaskWorkspace::Default,
            Some(&child),
            false,
        )
        .unwrap();
        assert_eq!(frontend.asked, 0);
    }

    /// `-n` means never ask. The widening must be refused outright rather than
    /// prompted for, so a scripted run neither blocks nor widens silently.
    #[test]
    fn non_interactive_refuses_a_parent_directory_workspace_without_asking() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().canonicalize().unwrap();
        let child = parent.join("child");
        std::fs::create_dir(&child).unwrap();

        let mut frontend = RefusingLocalFrontend { asked: 0 };
        let error = confirm_scripted_workspace_scope(
            &mut frontend,
            &TaskWorkspace::Custom(parent),
            Some(&child),
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains("parent directory"), "{error}");
        assert_eq!(
            frontend.asked, 0,
            "--non-interactive must refuse without putting a question to anyone"
        );
    }

    #[test]
    fn the_daemon_does_not_re_apply_the_clients_mount_scope_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().canonicalize().unwrap();
        let child = parent.join("child");
        std::fs::create_dir(&child).unwrap();

        confirm_scripted_workspace_scope(
            &mut DaemonFrontend,
            &TaskWorkspace::Custom(parent),
            Some(&child),
            false,
        )
        .expect("the daemon must accept what the client already authorised");
    }
}

/// Remove a task's persistent directory when confirmed. Resolves the path
/// through the `validate_under_root`-guarded `SquadPaths::task_dir`, asks
/// the frontend (unless `-y`), and deletes only on a `true` answer. A missing
/// directory is not an error. Returns the directory actually removed, if any.
///
/// Removing the task is the *only* thing that may remove its durable workspace
/// (WI 0106 §6a) — nothing on the run path ever deletes it. What is removed
/// here is the whole `tasks/<name>/` tree, not just its `workspace/` leaf, so
/// the task's per-run log directories go with it rather than being orphaned.
fn remove_task_dir(
    frontend: &mut dyn SquadCommandFrontend,
    name: &str,
    yes: bool,
) -> Result<Option<PathBuf>, CommandError> {
    let paths = SquadPaths::from_process_env()?;
    // `task_dir` is the guarded `.../tasks/<name>/workspace`; its parent is the
    // task's whole tree. Deriving it this way keeps the single path guard.
    let workspace = paths.task_dir(name)?;
    let dir = workspace
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(workspace);
    // Deletion requires an explicit `true`: `-y`, or a frontend that confirms.
    // A declined prompt (or one no frontend can answer — the daemon's API
    // frontend, an aborted dialog) keeps the directory rather than failing the
    // remove, whose gateway delete has already succeeded.
    let confirmed = yes || matches!(frontend.ask_delete_task_dir(name, &dir), Ok(true));
    if !confirmed {
        return Ok(None);
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(Some(dir)),
        // A task with no persistent directory is a no-op, not a failure.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(crate::data::error::DataError::io(&dir, error).into()),
    }
}

/// Which half of §6a's warning applies: `squad add` or `squad edit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskChange {
    Created,
    Updated,
}

impl TaskChange {
    fn verb(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
        }
    }
}

/// §6a — the one moment of truth, at the point of action.
///
/// The gateway runs in the daemon, so `task.unmet_env` is an authoritative
/// answer by the time the task comes back: these are the `env(NAME)` names the
/// daemon has no usable value for. One `Warning` is written to the frontend's
/// message sink, so the CLI and the TUI render the same text without either
/// composing its own.
///
/// **The task is created.** This is a warning, never a rejection: the value may
/// legitimately arrive later, and refusing to create a task because the current
/// shell is under-equipped would lose the user their task — its own kind of
/// silent failure. Nothing here repeats on a timer and nothing prompts (§6f).
fn warn_unmet_env(frontend: &mut dyn SquadCommandFrontend, task: &Task, change: TaskChange) {
    let Some(first) = task.unmet_env.first() else {
        return;
    };
    let names = task.unmet_env.join(", ");
    frontend.write_message(UserMessage {
        level: MessageLevel::Warning,
        text: format!(
            "\u{26a0} Task \"{name}\" was {verb}, but the squad daemon has no value for {names}.\n\
             \n  \
             Its containers will start without it until it is supplied. To fix:\n      \
             read -rs {first} && export {first}      # in any shell; keeps it out of shell history\n      \
             awman squad env --push       # or just run any awman squad command\n\
             \n  \
             Check state at any time with `awman squad env`.",
            name = task.name,
            verb = change.verb(),
        ),
    });
}

/// Turn the daemon's coverage into §6d's report.
///
/// Pure over the coverage plus this process's own environment, so the whole
/// table is decided in one place and neither frontend has to reason about
/// digests. The only thing this shell's values are used for is the `SOURCE`
/// column's `this shell` case — a digest comparison, never a printed value.
fn env_report(coverage: &EnvCoverage, cleared: Option<bool>) -> EnvReport {
    let salt = Salt::from_hex(&coverage.salt);
    let rows = coverage
        .required
        .iter()
        .map(|entry| {
            let held = entry.source.is_some();
            let state = if held {
                "set"
            } else if entry.optional {
                // Wanted, but never counted against anyone: a machine with no
                // GITHUB_TOKEN must not read as permanently broken.
                "optional"
            } else {
                "unmet"
            };
            // "this shell" outranks the daemon's own answer for a held value:
            // it says the value here matches the value there, so a restart
            // would be re-armed by the next command from this terminal. That
            // is what the user needs to know, and the comparison never moves
            // a value anywhere — only a digest of one.
            let from_this_shell = match (&salt, &entry.digest) {
                (Some(salt), Some(digest)) => host_var(&entry.name)
                    .filter(|value| !value.is_empty())
                    .is_some_and(|value| &coverage_digest(salt, &entry.name, &value) == digest),
                _ => false,
            };
            let source = if from_this_shell {
                "this shell"
            } else {
                match entry.source {
                    Some(EnvSource::Pushed) => "pushed",
                    Some(EnvSource::Keychain) => "keychain",
                    None => "\u{2014}",
                }
            };
            EnvReportRow {
                name: entry.name.clone(),
                state: state.to_string(),
                source: source.to_string(),
                unmet_since: entry.unmet_since,
                required_by: entry.required_by.clone(),
            }
        })
        .collect();
    EnvReport {
        persistence: coverage.persistence.clone(),
        cleared,
        rows,
    }
}

fn daemon_outcome(value: SquadDaemonOutcome) -> Result<SquadOutcome, CommandError> {
    Ok(match value {
        SquadDaemonOutcome::Started {
            port,
            background,
            refreshed_key,
        } => SquadOutcome::Started {
            port,
            background,
            refreshed_key,
        },
        SquadDaemonOutcome::Stopped { stopped_pid } => SquadOutcome::Stopped { stopped_pid },
        SquadDaemonOutcome::Status(status) => SquadOutcome::Status(status),
        SquadDaemonOutcome::Logs { log_path } => SquadOutcome::Logs { log_path },
    })
}

#[cfg(test)]
mod unmet_env_warning_tests {
    use super::*;
    use chrono::Utc;

    use std::sync::{Arc, Mutex};

    use crate::data::fs::task_store::{MountScope, TaskStatus};
    use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};

    /// Records everything the command writes to the frontend's message sink.
    /// Both the CLI and the TUI implement this sink, which is exactly why the
    /// warning is composed here and not in either frontend: they render the
    /// same bytes.
    #[derive(Clone, Default)]
    struct RecordingFrontend {
        messages: Arc<Mutex<Vec<UserMessage>>>,
    }

    impl UserMessageSink for RecordingFrontend {
        fn write_message(&mut self, message: UserMessage) {
            self.messages.lock().unwrap().push(message);
        }
        fn replay_queued(&mut self) {}
    }

    #[async_trait]
    impl SquadCommandFrontend for RecordingFrontend {}

    fn task(name: &str, unmet: &[&str]) -> Task {
        let now = Utc::now();
        Task {
            id: name.into(),
            name: name.into(),
            description: "a task".into(),
            repo_scope: PathBuf::from("/repo"),
            mount_scope: MountScope::GitRoot,
            overlays: vec!["env(ANTHROPIC_KEY)".into()],
            interval_secs: 600,
            status: TaskStatus::Active,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            unmet_env: unmet.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn warn(task: &Task, change: TaskChange) -> Vec<UserMessage> {
        let mut frontend = RecordingFrontend::default();
        let recorded = frontend.messages.clone();
        warn_unmet_env(&mut frontend, task, change);
        let messages = recorded.lock().unwrap().clone();
        messages
    }

    /// §6a, `squad add`. The exact template, including its indentation — the
    /// work item's own spacing, kept verbatim so the CLI and the TUI cannot
    /// drift apart.
    #[test]
    fn creating_a_task_that_names_an_unset_variable_warns_with_the_exact_text() {
        let messages = warn(
            &task("nightly-triage", &["ANTHROPIC_KEY"]),
            TaskChange::Created,
        );

        assert_eq!(messages.len(), 1, "exactly one message, never a repetition");
        assert_eq!(messages[0].level, MessageLevel::Warning);
        assert_eq!(
            messages[0].text,
            "\u{26a0} Task \"nightly-triage\" was created, but the squad daemon has no value \
             for ANTHROPIC_KEY.\n\n  Its containers will start without it until it is \
             supplied. To fix:\n      read -rs ANTHROPIC_KEY && export ANTHROPIC_KEY      # in any shell; \
             keeps it out of shell history\n      \
             awman squad env --push       # or just run any awman squad command\n\n  \
             Check state at any time with `awman squad env`."
        );
    }

    /// The same warning with the `squad edit` verb, and `{first_name}` is the
    /// first of the joined names.
    #[test]
    fn editing_a_task_warns_with_the_updated_verb_and_joins_several_names() {
        let messages = warn(
            &task("nightly-triage", &["NPM_TOKEN", "AWS_PROFILE"]),
            TaskChange::Updated,
        );

        assert_eq!(messages.len(), 1);
        assert!(
            messages[0].text.starts_with(
                "\u{26a0} Task \"nightly-triage\" was updated, but the squad \
                              daemon has no value for NPM_TOKEN, AWS_PROFILE."
            ),
            "{:?}",
            messages[0].text
        );
        assert!(
            messages[0]
                .text
                .contains("read -rs NPM_TOKEN && export NPM_TOKEN"),
            "the supply line names the first of them: {:?}",
            messages[0].text
        );
    }

    /// **The warning is a warning, never a rejection.** `warn_unmet_env`
    /// returns nothing and can fail nothing: the caller has already created or
    /// updated the task by the time it runs, and goes on to return it. Refusing
    /// to create a task because the current shell is under-equipped would lose
    /// the user their task — its own kind of silent failure.
    #[test]
    fn the_warning_returns_nothing_and_can_refuse_nothing() {
        let task = task("nightly-triage", &["ANTHROPIC_KEY"]);
        let messages = warn(&task, TaskChange::Created);
        // The only effect is one message; the task is untouched and is what
        // `SquadOutcome::Task` carries back to the frontend.
        assert_eq!(messages.len(), 1);
        assert_eq!(task.unmet_env, vec!["ANTHROPIC_KEY".to_string()]);
        let outcome = SquadOutcome::Task(task.clone());
        match outcome {
            SquadOutcome::Task(returned) => assert_eq!(returned.name, "nightly-triage"),
            other => panic!("creation yields the created task: {other:?}"),
        }
    }

    /// A fully covered task says nothing at all — and neither does an
    /// `optional` host-side name, because the daemon never puts one on
    /// `unmet_env` in the first place.
    #[test]
    fn nothing_is_written_when_the_returned_task_has_no_unmet_name() {
        assert!(warn(&task("nightly-triage", &[]), TaskChange::Created).is_empty());
        assert!(warn(&task("nightly-triage", &[]), TaskChange::Updated).is_empty());
    }
}
