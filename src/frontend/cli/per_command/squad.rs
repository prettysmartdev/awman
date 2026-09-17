//! CLI presentation for the squad command family.

use chrono::{DateTime, Utc};
use clap::ArgMatches;

use crate::command::commands::squad::commands::{EnvReport, SquadOutcome};
use crate::data::fs::task_store::Task;

use super::render::format_table;

/// Render a squad outcome through the CLI's common table/JSON conventions.
pub(crate) fn render_squad(outcome: &SquadOutcome, json: bool) -> Option<String> {
    if json {
        return Some(
            serde_json::to_string_pretty(outcome)
                .unwrap_or_else(|error| format!("failed to serialize squad outcome: {error}")),
        );
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
            Some(format!(
                "Task: {}\nDescription: {}\nWorkspace: {}\nOverlays: {}\nInterval: {}s\nAgent: {}\nModel: {}\n{}\n{}",
                task.name,
                task.description,
                workspace,
                overlays,
                task.interval_secs,
                task.agent.as_deref().unwrap_or("default"),
                task.model.as_deref().unwrap_or("default"),
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
                return Some("squad daemon is not running.".into());
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
            let persistence = match status.env_persistence.as_str() {
                "" | "keychain" | "none" => String::new(),
                other => format!("; env persistence {other}"),
            };
            Some(format!(
                "squad daemon running (PID {pid}) at {address}; {} tasks ({} active); last tick {last_tick}{unmet}{persistence}",
                status.task_count, status.active_count
            ))
        }
        SquadOutcome::Env(report) => Some(render_env_report(report)),
        SquadOutcome::Logs { log_path } => Some(format!("Tailing squad logs at {log_path}")),
        // `--refresh-key` returns before anything is started, so saying
        // "started" here would contradict the key snippet just printed.
        SquadOutcome::Started {
            refreshed_key: true,
            ..
        } => Some("squad API key regenerated. Start the daemon with `awman squad start`.".into()),
        SquadOutcome::Started {
            port, background, ..
        } => Some(if *background {
            format!("squad daemon started in the background on port {port}.")
        } else {
            format!("squad daemon started on port {port}.")
        }),
        SquadOutcome::Stopped { stopped_pid } => Some(match stopped_pid {
            Some(pid) => format!("squad daemon (PID {pid}) stopped."),
            None => "squad daemon is not running.".into(),
        }),
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
    // An empty persistence string means "asked something with no daemon to
    // answer" (`awman squad daemon status`); say nothing rather than guess.
    if report.persistence.is_empty() {
        out.push_str("Daemon env coverage\n");
    } else {
        out.push_str(&format!(
            "Daemon env coverage (persistence: {})\n",
            report.persistence
        ));
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
            let state = match row.state.as_str() {
                "set" => "\u{2713} set".to_string(),
                "unmet" => "\u{26a0} unmet".to_string(),
                other => format!("\u{b7} {other}"),
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
        .filter(|row| row.state == "unmet")
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

pub(crate) fn squad_flag(matches: &ArgMatches, flag: &str) -> bool {
    matches
        .subcommand_matches("squad")
        .and_then(|squad| squad.try_get_one::<bool>(flag).ok().flatten().copied())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone;

    use crate::command::commands::squad::commands::{EnvReportRow, SquadOutcome};
    use crate::command::commands::squad::gateway::{DaemonStatus, TaskDetail};
    use crate::data::fs::task_store::{MountScope, Run, RunStatus, Task, TaskStatus};

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
    }

    fn task(name: &str, unmet: &[&str]) -> Task {
        Task {
            id: name.into(),
            name: name.into(),
            description: "a task".into(),
            repo_scope: std::path::PathBuf::from("/repo"),
            mount_scope: MountScope::GitRoot,
            overlays: vec!["env(AWS_PROFILE)".into()],
            interval_secs: 600,
            status: TaskStatus::Active,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: at(0),
            updated_at: at(0),
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            unmet_env: unmet.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn run(unmet: &[&str]) -> Run {
        Run {
            id: "run-1".into(),
            task_id: "t".into(),
            status: RunStatus::Failed,
            workflow_path: None,
            workflow_state_path: None,
            session_id: None,
            started_at: at(10),
            finished_at: Some(at(20)),
            error: Some("boom".into()),
            reason: Some("3 new issues".into()),
            unmet_env: unmet.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn status(unmet: &[&str]) -> DaemonStatus {
        DaemonStatus {
            running: true,
            pid: Some(4242),
            bound_addr: Some("http://127.0.0.1:45791".into()),
            task_count: 2,
            active_count: 2,
            last_tick: Some(at(0)),
            in_flight: 0,
            env_persistence: "keychain".into(),
            unmet_env: unmet.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn report(rows: Vec<EnvReportRow>, persistence: &str, cleared: Option<bool>) -> EnvReport {
        EnvReport {
            persistence: persistence.into(),
            cleared,
            rows,
        }
    }

    fn row(
        name: &str,
        state: &str,
        source: &str,
        unmet_since: Option<DateTime<Utc>>,
    ) -> EnvReportRow {
        EnvReportRow {
            name: name.into(),
            state: state.into(),
            source: source.into(),
            unmet_since,
            required_by: vec!["deploy-preview".into()],
        }
    }

    fn text(outcome: &SquadOutcome) -> String {
        render_squad(outcome, false).expect("this outcome renders text")
    }

    // ─── §6b: the `squad status` clause ─────────────────────────────────────

    /// A daemon with full coverage prints exactly what it printed before
    /// WI 0116 — the clause is absent, not empty, so the common case gained no
    /// noise at all.
    #[test]
    fn squad_status_says_nothing_about_env_when_nothing_is_unmet() {
        let line = text(&SquadOutcome::Status(status(&[])));
        assert!(
            line.ends_with(&at(0).to_rfc3339()),
            "the line still ends at the last tick: {line:?}"
        );
        assert!(!line.contains("env value"), "{line:?}");
    }

    /// The clause is count-sensitive and is appended after the `last tick`
    /// value, never inserted mid-line.
    #[test]
    fn squad_status_appends_a_count_sensitive_env_clause() {
        assert!(
            text(&SquadOutcome::Status(status(&["AWS_PROFILE"]))).ends_with("; 1 env value unmet")
        );
        assert!(
            text(&SquadOutcome::Status(status(&["AWS_PROFILE", "NPM_TOKEN"])))
                .ends_with("; 2 env values unmet")
        );
    }

    /// Remediation of review-security F7 / review-adversarial F10.
    ///
    /// Persistence is on by default and its fallback warning goes only to the
    /// daemon log, so a user who never runs `awman squad env` was never told
    /// their `env()` values had stopped being persisted. The healthy states —
    /// `keychain` and the explicit `none` opt-out — stay silent, so the common
    /// line is unchanged.
    #[test]
    fn squad_status_names_a_degraded_env_persistence_and_stays_silent_otherwise() {
        for healthy in ["keychain", "none", ""] {
            let mut ok = status(&[]);
            ok.env_persistence = healthy.into();
            let line = text(&SquadOutcome::Status(ok));
            assert!(
                !line.contains("env persistence"),
                "{healthy:?} is not a problem to report: {line:?}"
            );
        }

        let mut degraded = status(&[]);
        degraded.env_persistence = "unavailable(secret-tool not found)".into();
        assert!(
            text(&SquadOutcome::Status(degraded))
                .ends_with("; env persistence unavailable(secret-tool not found)"),
            "the degraded state is appended after the last tick, reason and all"
        );

        // Both clauses can appear, unmet first.
        let mut both = status(&["AWS_PROFILE"]);
        both.env_persistence = "unavailable(store failed: timed out)".into();
        assert!(text(&SquadOutcome::Status(both)).ends_with(
            "; 1 env value unmet; env persistence unavailable(store failed: timed out)"
        ));
    }

    /// A daemon that is down says so and nothing else: there is no coverage to
    /// report and the clause would be a guess.
    #[test]
    fn a_stopped_daemon_reports_no_env_clause() {
        let mut down = status(&["AWS_PROFILE"]);
        down.running = false;
        assert_eq!(
            text(&SquadOutcome::Status(down)),
            "squad daemon is not running."
        );
    }

    // ─── §6b: the `squad list` marker and footer ────────────────────────────

    #[test]
    fn squad_list_marks_the_affected_row_and_footers_the_count() {
        let rendered = text(&SquadOutcome::Tasks(vec![
            task("nightly-triage", &["ANTHROPIC_KEY"]),
            task("deploy-preview", &["AWS_PROFILE", "NPM_TOKEN"]),
        ]));
        assert!(
            rendered.contains("nightly-triage \u{26a0} env"),
            "the marker is a suffix on the name cell: {rendered}"
        );
        assert!(rendered.contains("deploy-preview \u{26a0} env"));
        assert!(
            rendered.contains("\u{26a0} 2 tasks are missing an env value; see awman squad env"),
            "{rendered}"
        );
        // No fifth column: the marker rides the existing four headers.
        assert!(!rendered.contains("Unmet"), "{rendered}");
    }

    #[test]
    fn one_affected_task_uses_the_singular_footer() {
        let rendered = text(&SquadOutcome::Tasks(vec![
            task("nightly-triage", &["ANTHROPIC_KEY"]),
            task("deploy-preview", &[]),
        ]));
        assert!(
            rendered.contains("\u{26a0} 1 task is missing an env value; see awman squad env"),
            "{rendered}"
        );
    }

    /// With nothing unmet the output is byte-identical to the pre-WI-0116
    /// table: no marker, and **no footer line at all**.
    #[test]
    fn squad_list_with_full_coverage_renders_exactly_as_before_wi_0116() {
        let clean = text(&SquadOutcome::Tasks(vec![
            task("nightly-triage", &[]),
            task("deploy-preview", &[]),
        ]));
        assert!(!clean.contains('\u{26a0}'), "{clean}");
        assert!(!clean.contains("missing an env value"), "{clean}");
        assert_eq!(
            clean,
            format_table(
                &["Name", "Status", "Last run", "Next evaluation"],
                &[
                    vec![
                        "nightly-triage".to_string(),
                        "active".to_string(),
                        "\u{2014}".to_string(),
                        "now".to_string()
                    ],
                    vec![
                        "deploy-preview".to_string(),
                        "active".to_string(),
                        "\u{2014}".to_string(),
                        "now".to_string()
                    ],
                ]
            ),
            "the unaffected table is the plain one, with nothing appended"
        );
    }

    // ─── §6b + §6e: `squad show` ────────────────────────────────────────────

    /// Two separate facts: the header line is what is missing **now**, the run
    /// column is what was missing **then**.
    #[test]
    fn squad_show_carries_the_env_header_line_and_the_historical_run_column() {
        let rendered = text(&SquadOutcome::Detail(TaskDetail {
            task: task("deploy-preview", &["AWS_PROFILE"]),
            runs: vec![run(&["AWS_PROFILE"]), run(&[])],
        }));
        assert!(
            rendered.contains("Env: \u{26a0} AWS_PROFILE unmet (see `awman squad env`)"),
            "{rendered}"
        );
        assert!(rendered.contains("Unmet env"), "{rendered}");
        assert!(
            rendered.contains("\u{2014}"),
            "a run in the window that had none renders an em dash: {rendered}"
        );
    }

    /// Several names are `, `-joined on one line.
    #[test]
    fn several_unmet_names_join_with_commas_in_the_show_header() {
        let rendered = text(&SquadOutcome::Detail(TaskDetail {
            task: task("deploy-preview", &["AWS_PROFILE", "NPM_TOKEN"]),
            runs: Vec::new(),
        }));
        assert!(
            rendered.contains("Env: \u{26a0} AWS_PROFILE, NPM_TOKEN unmet"),
            "{rendered}"
        );
    }

    /// A healthy task's detail view is the pre-WI-0116 one: no `Env:` line and
    /// the run table keeps its four original columns.
    #[test]
    fn a_task_with_full_coverage_shows_no_env_line_and_a_four_column_run_table() {
        let rendered = text(&SquadOutcome::Detail(TaskDetail {
            task: task("deploy-preview", &[]),
            runs: vec![run(&[])],
        }));
        assert!(!rendered.contains("Env:"), "{rendered}");
        assert!(
            !rendered.contains("Unmet env"),
            "the column appears only when some run in the window carries one: {rendered}"
        );
    }

    /// The leader verdict's reason is a column right after the run status.
    #[test]
    fn squad_show_renders_the_verdict_reason_after_the_status() {
        let mut quiet = run(&[]);
        quiet.reason = None;
        let rendered = text(&SquadOutcome::Detail(TaskDetail {
            task: task("deploy-preview", &[]),
            runs: vec![run(&[]), quiet],
        }));
        let header = rendered
            .lines()
            .find(|line| line.contains("Started"))
            .expect("run table header");
        let status_at = header.find("Status").unwrap();
        let reason_at = header.find("Reason").expect("a Reason column");
        let error_at = header.find("Error").unwrap();
        assert!(status_at < reason_at && reason_at < error_at, "{header}");
        assert!(rendered.contains("3 new issues"), "{rendered}");
    }

    // ─── §6d: the `awman squad env` table ───────────────────────────────────

    #[test]
    fn the_env_table_renders_the_header_both_states_and_the_next_step() {
        let rendered = text(&SquadOutcome::Env(report(
            vec![
                row("ANTHROPIC_KEY", "set", "this shell", None),
                row("AWS_PROFILE", "unmet", "\u{2014}", Some(Utc::now())),
                // A task-declared GITHUB_TOKEN is an ordinary unmet row. It
                // used to render as `· optional`, which is why the host-side
                // exemption was removed.
                row("GITHUB_TOKEN", "unmet", "\u{2014}", Some(Utc::now())),
            ],
            "unavailable(secret-tool not found)",
            None,
        )));

        assert!(
            rendered.starts_with(
                "Daemon env coverage (persistence: unavailable(secret-tool not found))\n"
            ),
            "the persistence string is rendered verbatim: {rendered}"
        );
        assert!(rendered.contains("  NAME "), "{rendered}");
        assert!(rendered.contains("\u{2713} set"), "{rendered}");
        assert!(rendered.contains("\u{26a0} unmet"), "{rendered}");
        assert!(
            !rendered.contains("optional"),
            "no name is exempt from unmet reporting any more: {rendered}"
        );
        assert!(rendered.contains("this shell"), "{rendered}");
        // `unmet_since` reaches the SINCE column, which is the whole point of
        // stamping it: "just typo'd" and "broken for days" must look different.
        assert!(rendered.contains("just now"), "{rendered}");
        assert!(
            rendered.contains("  AWS_PROFILE is required by task \"deploy-preview\".\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("  Export it and run `awman squad env --push`.\n"),
            "{rendered}"
        );
        // Every table line is two-space indented and none carries trailing
        // whitespace (the last column is deliberately unpadded).
        for line in rendered.lines().filter(|l| !l.is_empty()) {
            assert_eq!(
                line.trim_end(),
                line,
                "no rendered line may carry trailing whitespace: {line:?}"
            );
        }
    }

    /// The SINCE column is coarse on purpose: the question is "did I just
    /// typo this, or has it been broken for three days".
    #[test]
    fn the_since_column_is_a_coarse_relative_age() {
        let now = at(0);
        assert_eq!(relative_age(None, now), "\u{2014}");
        assert_eq!(relative_age(Some(at(-30)), now), "just now");
        assert_eq!(relative_age(Some(at(-600)), now), "10m ago");
        assert_eq!(relative_age(Some(at(-7200)), now), "2h ago");
        assert_eq!(relative_age(Some(at(-3 * 86_400)), now), "3d ago");
    }

    /// With nothing unmet the trailing "is required by" block is absent
    /// entirely, not an empty paragraph.
    #[test]
    fn a_fully_covered_report_prints_no_trailing_block() {
        let rendered = text(&SquadOutcome::Env(report(
            vec![row("ANTHROPIC_KEY", "set", "pushed", None)],
            "keychain",
            None,
        )));
        assert!(!rendered.contains("is required by"), "{rendered}");
        assert!(!rendered.contains("--push"), "{rendered}");
    }

    /// `--clear`'s two forms, keyed on the boolean the daemon returns. Both sit
    /// between the header and the table.
    #[test]
    fn the_clear_line_reports_what_actually_happened() {
        let removed = text(&SquadOutcome::Env(report(
            vec![row("ANTHROPIC_KEY", "set", "pushed", None)],
            "keychain",
            Some(true),
        )));
        assert!(
            removed.contains(
                "\n  Stored env item removed. The running daemon keeps the values it already holds.\n"
            ),
            "{removed}"
        );
        let nothing = text(&SquadOutcome::Env(report(
            vec![row("ANTHROPIC_KEY", "set", "pushed", None)],
            "none",
            Some(false),
        )));
        assert!(
            nothing.contains("\n  No stored env item to remove (this daemon persists nothing).\n"),
            "{nothing}"
        );
    }

    /// `awman squad daemon status` has no daemon to ask, so `persistence` is
    /// `""`. Render nothing rather than guess.
    #[test]
    fn an_empty_persistence_string_degrades_to_a_bare_header() {
        let rendered = text(&SquadOutcome::Env(report(Vec::new(), "", None)));
        assert!(rendered.starts_with("Daemon env coverage\n"), "{rendered}");
        assert!(!rendered.contains("persistence"), "{rendered}");
        assert!(
            rendered.contains("No task declares an env() overlay, so the daemon needs nothing."),
            "{rendered}"
        );
    }

    /// The JSON form carries the ordinary `kind`/`payload` envelope, and
    /// `state` is the bare word — the glyphs are a text-table detail only. **No
    /// field in the payload can hold a value**, which is why the assertion is
    /// on the serialised text.
    #[test]
    fn the_json_env_report_uses_bare_state_words_and_carries_no_value() {
        let outcome = SquadOutcome::Env(report(
            vec![
                row("AWS_PROFILE", "unmet", "\u{2014}", Some(at(0))),
                row("NPM_TOKEN", "set", "pushed", None),
            ],
            "none",
            Some(true),
        ));
        let json = render_squad(&outcome, true).expect("json renders");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["kind"], "Env");
        assert_eq!(parsed["payload"]["persistence"], "none");
        assert_eq!(parsed["payload"]["cleared"], true);
        assert_eq!(parsed["payload"]["rows"][0]["state"], "unmet");
        assert_eq!(parsed["payload"]["rows"][1]["state"], "set");
        assert!(
            !json.contains('\u{26a0}') && !json.contains('\u{2713}'),
            "the glyphs belong to the text table only: {json}"
        );
        // The whole row inventory (alphabetical, as `serde_json::Value`
        // stores object keys), so a value-carrying field cannot be added
        // without this failing.
        let keys: Vec<&str> = parsed["payload"]["rows"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec!["name", "required_by", "source", "state", "unmet_since"],
            "no field of an env row may ever hold a value"
        );
    }

    /// `squad list --json` is untouched by the marker: the data rides on each
    /// task's `unmet_env` array exactly as the daemon sent it.
    #[test]
    fn squad_list_json_carries_unmet_env_and_no_marker() {
        let json = render_squad(
            &SquadOutcome::Tasks(vec![task("nightly-triage", &["ANTHROPIC_KEY"])]),
            true,
        )
        .unwrap();
        assert!(!json.contains("\u{26a0} env"), "{json}");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["payload"][0]["name"], "nightly-triage");
        assert_eq!(
            parsed["payload"][0]["unmet_env"],
            serde_json::json!(["ANTHROPIC_KEY"])
        );
    }
}
