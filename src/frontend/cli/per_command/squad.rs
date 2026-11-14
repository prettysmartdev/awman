//! CLI presentation for the squad command family.

use crate::data::fs::daemon_env::{EnvPersistence, EnvVarState};
use chrono::{DateTime, Utc};

use crate::command::commands::squad::commands::{EnvReport, SquadOutcome};
use crate::data::fs::task_store::Task;

use crate::frontend::render_helpers::format_table;

/// Render a squad outcome through the CLI's common table/JSON conventions.
pub(crate) fn render_squad(outcome: &SquadOutcome, json: bool) -> Option<String> {
    if json {
        let value = serde_json::to_value(outcome).map(|value| match outcome {
            SquadOutcome::Task(_)
            | SquadOutcome::Updated(_)
            | SquadOutcome::Tasks(_)
            | SquadOutcome::Detail(_) => value["payload"].clone(),
            _ => value,
        });
        return Some(match value {
            Ok(value) => serde_json::to_string_pretty(&value).expect("JSON value serializes"),
            Err(error) => format!("failed to serialize squad outcome: {error}"),
        });
    }

    match outcome {
        SquadOutcome::Tasks(tasks) => {
            let rows = tasks
                .iter()
                .map(|task| {
                    let status = format!("{:?}", task.status).to_lowercase();
                    let last_run = task
                        .last_run_at
                        .map(|time| time.to_rfc3339())
                        .unwrap_or_else(|| "—".into());
                    let next = if status == "paused" {
                        "paused".into()
                    } else {
                        task.backoff_until
                            .or_else(|| {
                                task.last_run_at.map(|time| {
                                    time + chrono::Duration::seconds(task.interval_secs as i64)
                                })
                            })
                            .map(|time| time.to_rfc3339())
                            .unwrap_or_else(|| "now".into())
                    };
                    vec![marked_task_name(task), status, last_run, next]
                })
                .collect::<Vec<_>>();
            let table = format_table(&["Name", "Status", "Last run", "Next evaluation"], &rows);
            Some(match env_footer(tasks) {
                Some(footer) => format!("{table}{footer}\n"),
                None => table,
            })
        }
        SquadOutcome::Detail(detail) => {
            let task = &detail.task;
            // §6e — the unmet names recorded on each run row, so a run that
            // behaved oddly last Tuesday can still be explained. The column
            // appears only when some run in the window actually carries one:
            // an always-present column of dashes would be noise on every
            // healthy task, and this is the only place the information is
            // retained historically.
            let show_unmet = detail.runs.iter().any(|run| !run.unmet_env.is_empty());
            let rows = detail
                .runs
                .iter()
                .map(|run| {
                    let mut row = vec![
                        run.started_at.to_rfc3339(),
                        format!("{:?}", run.status).to_lowercase(),
                        run.reason.clone().unwrap_or_else(|| "—".into()),
                        run.finished_at
                            .map(|time| time.to_rfc3339())
                            .unwrap_or_else(|| "—".into()),
                        run.error.clone().unwrap_or_else(|| "—".into()),
                    ];
                    if show_unmet {
                        row.push(if run.unmet_env.is_empty() {
                            "—".into()
                        } else {
                            run.unmet_env.join(", ")
                        });
                    }
                    row
                })
                .collect::<Vec<_>>();
            let run_headers: &[&str] = if show_unmet {
                &[
                    "Started",
                    "Status",
                    "Reason",
                    "Finished",
                    "Error",
                    "Unmet env",
                ]
            } else {
                &["Started", "Status", "Reason", "Finished", "Error"]
            };
            let workspace = if task.uses_worktree() {
                format!("{} (worktree-isolated)", task.repo_scope.display())
            } else {
                format!(
                    "{} (mounted directly, no worktree)",
                    task.repo_scope.display()
                )
            };
            let overlays = if task.overlays.is_empty() {
                "(none)".to_string()
            } else {
                task.overlays.join(", ")
            };
            // The task's *current* unmet names are a separate fact from the
            // per-run history below: this is "what is missing now", the run
            // column is "what was missing then". Omitted entirely when there
            // is nothing to say.
            let env_line = if task.unmet_env.is_empty() {
                String::new()
            } else {
                format!(
                    "Env: \u{26a0} {} unmet (see `awman squad env`)\n",
                    task.unmet_env.join(", ")
                )
            };
            let prerequisites_line = match &task.prerequisites {
                Some(policy) => format!(
                    "Workflow prerequisites:\n  {}\n",
                    crate::command::commands::workflow_prerequisites::policy_lines(policy)
                        .join("\n  ")
                ),
                None => "Workflow prerequisites: workflow prerequisite policy not set; run `awman squad edit <task>` to configure\n".to_string(),
            };
            Some(format!(
                "Task: {}\nDescription: {}\nWorkspace: {}\nOverlays: {}\nInterval: {}s\nAgent: {}\nModel: {}\n{}{}\n{}",
                task.name,
                task.description,
                workspace,
                overlays,
                task.interval_secs,
                task.agent.as_deref().unwrap_or("default"),
                task.model.as_deref().unwrap_or("default"),
                prerequisites_line,
                env_line,
                format_table(run_headers, &rows),
            ))
        }
        SquadOutcome::Task(task) => Some(format!("Created task {}.", task.name)),
        SquadOutcome::Updated(task) => Some(format!("Updated task {}.", task.name)),
        SquadOutcome::Removed { name, removed_dir } => Some(match removed_dir {
            Some(path) => format!("Removed task {name} (deleted {}).", path.display()),
            None => format!("Removed task {name}."),
        }),
        // "on its next tick", not "now": the daemon evaluates on a fixed
        // cadence, so promising an immediate start would be a promise the
        // scheduler does not make.
        SquadOutcome::Triggered { name } => Some(format!(
            "Triggered task {name}; it will be evaluated on the next scheduler tick."
        )),
        // The daemon records the run as `canceled` straight away; stopping its
        // containers takes a few seconds each, so say that it is under way.
        SquadOutcome::Canceled { name } => Some(format!(
            "Canceled the in-progress run of task {name}; its containers are being stopped."
        )),
        SquadOutcome::Ok => None,
        SquadOutcome::Status(status) => {
            if !status.running {
                return Some(
                    "the awman daemon is not running (start it with `awman daemon start`).".into(),
                );
            }
            let pid = status
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "unknown".into());
            let address = status.bound_addr.as_deref().unwrap_or("unknown address");
            let last_tick = status
                .last_tick
                .map(|time| time.to_rfc3339())
                .unwrap_or_else(|| "never".into());
            // §6b — one extra clause, and only when there is one to add. A
            // daemon with full coverage prints exactly what it printed before
            // WI 0116, byte for byte, so the common case gained no noise.
            let unmet = match status.unmet_env.len() {
                0 => String::new(),
                1 => "; 1 env value unmet".to_string(),
                n => format!("; {n} env values unmet"),
            };
            // Same only-when-it-matters rule. Persistence is on by default, so
            // `keychain` is the healthy case and says nothing; `none` is the
            // user's own explicit opt-out and says nothing either. What has to
            // reach a user who never runs `awman squad env` is the degraded
            // case, which otherwise lives only in the daemon log.
            let persistence = match &status.env_persistence {
                None | Some(EnvPersistence::Keychain) | Some(EnvPersistence::None) => String::new(),
                Some(other) => format!("; env persistence {other}"),
            };
            let config = status
                .config_error
                .as_ref()
                .map(|error| format!("; config reload failed: {error}"))
                .unwrap_or_default();
            Some(format!(
                "squad on the awman daemon (PID {pid}) at {address}; {} tasks ({} active); last tick {last_tick}{unmet}{persistence}{config}",
                status.task_count, status.active_count
            ))
        }
        SquadOutcome::Env(report) => Some(render_env_report(report)),
    }
}

/// §6b — a task name with the standing `⚠ env` marker appended when the daemon
/// has no value for one of its `env()` names.
///
/// A suffix on the name rather than a column: the marker is a property of the
/// task, it costs nothing on the common path, and adding a fifth column would
/// widen every `squad list` on every machine to carry a flag almost none of
/// them raise.
fn marked_task_name(task: &Task) -> String {
    if task.unmet_env.is_empty() {
        task.name.clone()
    } else {
        format!("{} \u{26a0} env", task.name)
    }
}

/// The footer that expands the row markers into a count and a next step.
/// `None` — so nothing is printed at all — when no task is affected.
fn env_footer(tasks: &[Task]) -> Option<String> {
    let affected = tasks
        .iter()
        .filter(|task| !task.unmet_env.is_empty())
        .count();
    match affected {
        0 => None,
        1 => Some("\u{26a0} 1 task is missing an env value; see awman squad env".to_string()),
        n => Some(format!(
            "\u{26a0} {n} tasks are missing an env value; see awman squad env"
        )),
    }
}

/// §6d — the whole picture, as an aligned four-column table.
///
/// Deliberately not [`format_table`]'s box drawing: this view is read as a
/// checklist of names rather than as a record set, and the work item specifies
/// this shape. **No column can carry a value** — `STATE` says only whether one
/// is present.
fn render_env_report(report: &EnvReport) -> String {
    let mut out = String::new();
    // `None` means "asked something with no daemon to answer"
    // (`awman squad daemon status`); say nothing rather than guess.
    match &report.persistence {
        None => out.push_str("Daemon env coverage\n"),
        Some(persistence) => out.push_str(&format!(
            "Daemon env coverage (persistence: {persistence})\n"
        )),
    }
    if let Some(cleared) = report.cleared {
        out.push_str(if cleared {
            "\n  Stored env item removed. The running daemon keeps the values it already holds.\n"
        } else {
            "\n  No stored env item to remove (this daemon persists nothing).\n"
        });
    }
    if report.rows.is_empty() {
        out.push_str("\n  No task declares an env() overlay, so the daemon needs nothing.\n");
        return out;
    }

    let now = Utc::now();
    let cells: Vec<[String; 4]> = report
        .rows
        .iter()
        .map(|row| {
            let state = match row.state {
                EnvVarState::Set => "\u{2713} set".to_string(),
                EnvVarState::Unmet => "\u{26a0} unmet".to_string(),
            };
            [
                row.name.clone(),
                state,
                row.source.clone(),
                relative_age(row.unmet_since, now),
            ]
        })
        .collect();
    const HEADERS: [&str; 4] = ["NAME", "STATE", "SOURCE", "SINCE"];
    let mut widths = HEADERS.map(|header| header.chars().count());
    for row in &cells {
        for (width, cell) in widths.iter_mut().zip(row.iter()) {
            *width = (*width).max(cell.chars().count());
        }
    }
    out.push('\n');
    out.push_str(&format_env_row(&HEADERS.map(String::from), &widths));
    for row in &cells {
        out.push_str(&format_env_row(row, &widths));
    }

    // The trailing line: which task is actually affected, and the one command
    // that fixes it. Named rather than counted — "AWS_PROFILE is required by
    // task X" is actionable in a way that "1 unmet" is not.
    let mut unmet = report
        .rows
        .iter()
        .filter(|row| row.state == EnvVarState::Unmet)
        .peekable();
    if unmet.peek().is_some() {
        out.push('\n');
        for row in unmet {
            let by = match row.required_by.as_slice() {
                [] => "the daemon's own configuration".to_string(),
                [one] => format!("task \"{one}\""),
                many => format!(
                    "tasks {}",
                    many.iter()
                        .map(|name| format!("\"{name}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            out.push_str(&format!("  {} is required by {by}.\n", row.name));
        }
        out.push_str("  Export it and run `awman squad env --push`.\n");
    }
    out
}

/// One padded row of the `squad env` table, two-space indented. The last
/// column is not padded, so no line carries trailing whitespace.
fn format_env_row(cells: &[String; 4], widths: &[usize; 4]) -> String {
    let mut line = String::from("  ");
    for (index, (cell, width)) in cells.iter().zip(widths.iter()).enumerate() {
        line.push_str(cell);
        if index + 1 < cells.len() {
            let pad = width.saturating_sub(cell.chars().count()) + 2;
            line.push_str(&" ".repeat(pad));
        }
    }
    line.push('\n');
    line
}

/// How long ago `since` was, coarsely: `3d ago`, `2h ago`, `5m ago`. `—` when
/// there is no timestamp, which is the covered case.
///
/// Coarse on purpose — the question this answers is "did I just typo this, or
/// has it been broken for three days", and a precise duration would answer it
/// no better while reading as a measurement.
fn relative_age(since: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(since) = since else {
        return "\u{2014}".to_string();
    };
    let seconds = (now - since).num_seconds().max(0);
    match seconds {
        s if s < 60 => "just now".to_string(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}
