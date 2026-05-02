//! Per-variant CommandOutcome → user-facing string renderers.
//!
//! Each `CommandOutcome` variant gets a small, focused renderer that returns
//! the human-readable text the CLI prints to stdout on success. Renderers
//! return `None` when there is nothing additional to say beyond what the
//! engine already streamed via `report_step_status` / `report_summary` (that
//! output is already on stderr by the time the outcome is rendered).
//!
//! The whole module is pure: it never touches I/O or globals. Tests can call
//! any renderer directly with a synthesised outcome.

use crate::command::commands::auth::AuthOutcome;
use crate::command::commands::chat::ChatOutcome;
use crate::command::commands::claws::ClawsOutcome;
use crate::command::commands::config::{
    ConfigGetOutcome, ConfigOutcome, ConfigSetOutcome, ConfigShowOutcome,
};
use crate::command::commands::download::DownloadOutcome;
use crate::command::commands::exec_prompt::ExecPromptOutcome;
use crate::command::commands::exec_workflow::ExecWorkflowOutcome;
use crate::command::commands::headless::{
    HeadlessKillOutcome, HeadlessLogsOutcome, HeadlessOutcome, HeadlessStartOutcome,
    HeadlessStatusOutcome,
};
use crate::command::commands::implement::ImplementOutcome;
use crate::command::commands::init::InitOutcome;
use crate::command::commands::new::{NewOutcome, NewSkillOutcome, NewSpecOutcome, NewWorkflowOutcome};
use crate::command::commands::ready::ReadyOutcome;
use crate::command::commands::remote::{
    RemoteOutcome, RemoteRunOutcome, RemoteSessionKillOutcome, RemoteSessionStartOutcome,
};
use crate::command::commands::specs::{SpecsAmendOutcome, SpecsNewOutcome, SpecsOutcome};
use crate::command::commands::status::{StatusContainerRow, StatusOutcome};
use crate::command::CommandOutcome;

// ─── Top-level dispatcher ────────────────────────────────────────────────────

/// Format a [`CommandOutcome`] into the success-path stdout text. Returns
/// `None` when no extra output is needed (engines that stream their progress
/// to stderr already and produce no additional summary on stdout).
pub fn render(outcome: &CommandOutcome) -> Option<String> {
    match outcome {
        CommandOutcome::Empty => None,
        CommandOutcome::Status(o) => Some(render_status(o)),
        CommandOutcome::Chat(o) => render_chat(o),
        CommandOutcome::Init(o) => render_init(o),
        CommandOutcome::Ready(o) => render_ready(o),
        CommandOutcome::Claws(o) => render_claws(o),
        CommandOutcome::Implement(o) => render_implement(o),
        CommandOutcome::ExecPrompt(o) => render_exec_prompt(o),
        CommandOutcome::ExecWorkflow(o) => render_exec_workflow(o),
        CommandOutcome::Config(o) => render_config(o),
        CommandOutcome::Headless(o) => render_headless(o),
        CommandOutcome::Remote(o) => render_remote(o),
        CommandOutcome::New(o) => render_new(o),
        CommandOutcome::Specs(o) => render_specs(o),
        CommandOutcome::Auth(o) => render_auth(o),
        CommandOutcome::Download(o) => render_download(o),
    }
}

// ─── status ──────────────────────────────────────────────────────────────────

const STATUS_TIPS: &[&str] = &[
    "`amux status` shows all running agent containers.",
    "`amux status --watch` re-renders every few seconds. Press Ctrl-C to stop.",
    "`amux implement <work-item-number>` starts a code agent on a work item.",
    "`amux chat` opens an interactive chat session with your configured agent.",
    "`amux ready` checks your environment and builds the Docker image if needed.",
    "`amux claws init` sets up the nanoclaw parallel agent system for the first time.",
    "`amux new spec` guides you through creating a new work item interactively.",
    "Per-repo config lives at `<git-root>/aspec/.amux.json`.",
    "Global config lives at `~/.amux/config.json`.",
    "Agents always run inside Docker containers — never directly on the host.",
    "Only the current Git repo root is mounted into agent containers.",
];

fn select_tip() -> &'static str {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    STATUS_TIPS[(secs as usize) % STATUS_TIPS.len()]
}

pub fn render_status(o: &StatusOutcome) -> String {
    let mut out = String::new();
    out.push_str("AMUX STATUS DASHBOARD\n\n");
    out.push_str("CODE AGENTS\n");
    if o.containers.is_empty() {
        out.push_str("  No code agents running.\n");
        out.push_str("  To start one: amux implement <work-item>  or  amux chat\n");
    } else {
        let headers = ["●", "Container", "ID", "Image", "Started"];
        let rows: Vec<Vec<String>> = o
            .containers
            .iter()
            .map(|c: &StatusContainerRow| {
                let indicator = if c.stuck { "🟡" } else { "🟢" };
                vec![
                    indicator.to_string(),
                    c.name.clone(),
                    c.id.chars().take(12).collect(),
                    c.image.clone(),
                    c.started_at.clone(),
                ]
            })
            .collect();
        out.push_str(&format_table(&headers, &rows));
    }
    out.push_str(&format!("\nTip: {}\n", select_tip()));
    out
}

fn format_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let ncols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    out.push('┌');
    for (i, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < ncols { '┬' } else { '┐' });
    }
    out.push('\n');
    out.push('│');
    for (h, w) in headers.iter().zip(widths.iter()) {
        let pad = w.saturating_sub(h.chars().count());
        out.push_str(&format!(" {h}{} │", " ".repeat(pad)));
    }
    out.push('\n');
    out.push('├');
    for (i, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < ncols { '┼' } else { '┤' });
    }
    out.push('\n');
    for row in rows {
        out.push('│');
        for (cell, w) in row.iter().zip(widths.iter()) {
            let pad = w.saturating_sub(cell.chars().count());
            out.push_str(&format!(" {cell}{} │", " ".repeat(pad)));
        }
        out.push('\n');
    }
    out.push('└');
    for (i, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if i + 1 < ncols { '┴' } else { '┘' });
    }
    out.push('\n');
    out
}

// ─── chat / exec prompt / exec workflow / implement ──────────────────────────
//
// These commands stream the container's stdout/stderr directly to the host
// during the run. The success outcome is intentionally minimal — a one-line
// confirmation, only when there's something interesting to say.

fn render_chat(o: &ChatOutcome) -> Option<String> {
    match o.exit_code {
        Some(0) | None => None,
        Some(code) => Some(format!("Chat session ended with exit code {code}.")),
    }
}

fn render_exec_prompt(o: &ExecPromptOutcome) -> Option<String> {
    match o.exit_code {
        Some(0) | None => None,
        Some(code) => Some(format!("exec prompt ended with exit code {code}.")),
    }
}

fn render_exec_workflow(o: &ExecWorkflowOutcome) -> Option<String> {
    let exit = match o.exit_code {
        Some(c) if c != 0 => format!(" (exit {c})"),
        _ => String::new(),
    };
    let wt = if o.worktree_used { " in isolated worktree" } else { "" };
    Some(format!("Workflow {} completed{exit}{wt}.", o.workflow))
}

fn render_implement(o: &ImplementOutcome) -> Option<String> {
    let workflow = o
        .workflow_used
        .as_deref()
        .map(|w| format!(" (workflow {w})"))
        .unwrap_or_default();
    let wt = if o.worktree_used { " in isolated worktree" } else { "" };
    let exit = match o.exit_code {
        Some(c) if c != 0 => format!(" — exit {c}"),
        _ => String::new(),
    };
    Some(format!(
        "Implement run for work item {}{workflow}{wt}{exit}.",
        o.work_item,
    ))
}

// ─── init / ready / claws ────────────────────────────────────────────────────
//
// These engines emit their summary box via `report_summary` (replayed to
// stderr from the message queue). The success-path stdout output is None.

fn render_init(_o: &InitOutcome) -> Option<String> {
    None
}

fn render_ready(_o: &ReadyOutcome) -> Option<String> {
    None
}

fn render_claws(_o: &ClawsOutcome) -> Option<String> {
    None
}

// ─── config ──────────────────────────────────────────────────────────────────

fn render_config(o: &ConfigOutcome) -> Option<String> {
    match o {
        ConfigOutcome::Show(s) => Some(render_config_show(s)),
        ConfigOutcome::Get(g) => Some(render_config_get(g)),
        ConfigOutcome::Set(s) => Some(render_config_set(s)),
    }
}

fn render_config_show(o: &ConfigShowOutcome) -> String {
    let mut out = String::new();
    out.push_str("Global config:\n");
    out.push_str(&serde_json::to_string_pretty(&o.global).unwrap_or_else(|_| "(unavailable)".into()));
    out.push_str("\n\nRepo config:\n");
    out.push_str(&serde_json::to_string_pretty(&o.repo).unwrap_or_else(|_| "(unavailable)".into()));
    out.push('\n');
    out
}

fn render_config_get(o: &ConfigGetOutcome) -> String {
    let na = || "N/A".to_string();
    format!(
        "Field: {}\n  Global:    {}\n  Repo:      {}\n  Effective: {}",
        o.field,
        o.global_value.clone().unwrap_or_else(na),
        o.repo_value.clone().unwrap_or_else(na),
        o.effective_value.clone().unwrap_or_else(na),
    )
}

fn render_config_set(o: &ConfigSetOutcome) -> String {
    format!("Set {} ({}) = {}", o.field, o.scope, o.value)
}

// ─── headless ────────────────────────────────────────────────────────────────

fn render_headless(o: &HeadlessOutcome) -> Option<String> {
    match o {
        HeadlessOutcome::Start(s) => Some(render_headless_start(s)),
        HeadlessOutcome::Kill(k) => Some(render_headless_kill(k)),
        HeadlessOutcome::Logs(l) => Some(render_headless_logs(l)),
        HeadlessOutcome::Status(s) => Some(render_headless_status(s)),
    }
}

fn render_headless_start(o: &HeadlessStartOutcome) -> String {
    let mode = if o.background { "background" } else { "foreground" };
    let workdirs = if o.workdirs.is_empty() {
        "<none>".to_string()
    } else {
        o.workdirs.join(", ")
    };
    let key = if o.refreshed_key { " (api key refreshed)" } else { "" };
    format!(
        "Headless server started on port {} in {mode} mode.\n  workdirs: {workdirs}{key}",
        o.port
    )
}

fn render_headless_kill(o: &HeadlessKillOutcome) -> String {
    match o.stopped_pid {
        Some(pid) => format!("Headless server (PID {pid}) stopped."),
        None => "Headless server is not running.".to_string(),
    }
}

fn render_headless_logs(o: &HeadlessLogsOutcome) -> String {
    if o.log_path.is_empty() {
        "No headless server log found.".to_string()
    } else {
        format!("Tailing headless logs at {}", o.log_path)
    }
}

fn render_headless_status(o: &HeadlessStatusOutcome) -> String {
    if o.running {
        match o.pid {
            Some(pid) => format!("Headless server is running (PID {pid})."),
            None => "Headless server is running.".to_string(),
        }
    } else {
        "Headless server is not running.".to_string()
    }
}

// ─── remote ──────────────────────────────────────────────────────────────────

fn render_remote(o: &RemoteOutcome) -> Option<String> {
    match o {
        RemoteOutcome::Run(r) => Some(render_remote_run(r)),
        RemoteOutcome::SessionStart(s) => Some(render_remote_session_start(s)),
        RemoteOutcome::SessionKill(k) => Some(render_remote_session_kill(k)),
    }
}

fn render_remote_run(o: &RemoteRunOutcome) -> String {
    let cmd = o.command.join(" ");
    let session = o
        .session
        .as_deref()
        .map(|s| format!(" (session {s})"))
        .unwrap_or_default();
    let addr = o
        .remote_addr
        .as_deref()
        .map(|a| format!(" via {a}"))
        .unwrap_or_default();
    format!("Submitted remote command: {cmd}{session}{addr}")
}

fn render_remote_session_start(o: &RemoteSessionStartOutcome) -> String {
    let dir = o.dir.as_deref().unwrap_or("<cwd>");
    let addr = o
        .remote_addr
        .as_deref()
        .map(|a| format!(" via {a}"))
        .unwrap_or_default();
    format!("Remote session started for {dir}{addr}.")
}

fn render_remote_session_kill(o: &RemoteSessionKillOutcome) -> String {
    let id = o.session_id.as_deref().unwrap_or("<latest>");
    format!("Remote session {id} killed.")
}

// ─── new ─────────────────────────────────────────────────────────────────────

fn render_new(o: &NewOutcome) -> Option<String> {
    match o {
        NewOutcome::Spec(s) => Some(render_new_spec(s)),
        NewOutcome::Workflow(w) => Some(render_new_workflow(w)),
        NewOutcome::Skill(s) => Some(render_new_skill(s)),
    }
}

fn render_new_spec(o: &NewSpecOutcome) -> String {
    match &o.path {
        Some(p) => format!("Created work item: {p}"),
        None => "Work item created.".to_string(),
    }
}

fn render_new_workflow(o: &NewWorkflowOutcome) -> String {
    let scope = if o.global { "global" } else { "repo" };
    match &o.path {
        Some(p) => format!("Created workflow ({scope}, format={}): {p}", o.format),
        None => format!("Workflow created ({scope}, format={}).", o.format),
    }
}

fn render_new_skill(o: &NewSkillOutcome) -> String {
    let scope = if o.global { "global" } else { "repo" };
    match &o.path {
        Some(p) => format!("Created skill ({scope}): {p}"),
        None => format!("Skill created ({scope})."),
    }
}

// ─── specs / auth / download ─────────────────────────────────────────────────

fn render_specs(o: &SpecsOutcome) -> Option<String> {
    match o {
        SpecsOutcome::New(n) => Some(render_specs_new(n)),
        SpecsOutcome::Amend(a) => Some(render_specs_amend(a)),
    }
}

fn render_specs_new(o: &SpecsNewOutcome) -> String {
    let interview = if o.interview { " (interview)" } else { "" };
    match &o.created_path {
        Some(p) => format!("Created spec{interview}: {p}"),
        None => format!("Spec created{interview}."),
    }
}

fn render_specs_amend(o: &SpecsAmendOutcome) -> String {
    format!("Amended work item {}.", o.work_item)
}

fn render_auth(o: &AuthOutcome) -> Option<String> {
    Some(if o.accepted {
        "Agent auth consent accepted for this repo.".to_string()
    } else {
        "Agent auth consent declined for this repo.".to_string()
    })
}

fn render_download(o: &DownloadOutcome) -> Option<String> {
    Some(format!("Downloaded asset: {}", o.asset))
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::status::StatusOutcome;
    use crate::engine::step_status::StepStatus;

    #[test]
    fn render_empty_returns_none() {
        assert!(render(&CommandOutcome::Empty).is_none());
    }

    #[test]
    fn render_status_empty_state_message() {
        let o = StatusOutcome {
            containers: vec![],
            watched: false,
        };
        let s = render_status(&o);
        assert!(s.contains("AMUX STATUS DASHBOARD"));
        assert!(s.contains("No code agents running"));
        assert!(s.contains("Tip: "));
    }

    #[test]
    fn render_status_with_one_container_emits_table_and_no_json() {
        let o = StatusOutcome {
            containers: vec![StatusContainerRow {
                id: "abc1234567890".into(),
                name: "amux-1".into(),
                image: "amux/dev:latest".into(),
                started_at: "2025-01-01T00:00:00Z".into(),
                tab_number: None,
                stuck: false,
                command_label: None,
            }],
            watched: false,
        };
        let s = render_status(&o);
        assert!(s.contains("amux-1"), "{s}");
        // No JSON braces in the rendered string.
        assert!(!s.contains("\"name\""), "should not contain JSON: {s}");
        assert!(!s.contains("{"), "should not contain braces: {s}");
    }

    #[test]
    fn render_chat_clean_exit_returns_none() {
        let o = ChatOutcome {
            agent: Some("claude".into()),
            exit_code: Some(0),
        };
        assert!(render_chat(&o).is_none());
        let o2 = ChatOutcome {
            agent: None,
            exit_code: None,
        };
        assert!(render_chat(&o2).is_none());
    }

    #[test]
    fn render_chat_nonzero_exit_returns_some() {
        let o = ChatOutcome {
            agent: None,
            exit_code: Some(2),
        };
        assert_eq!(
            render_chat(&o).unwrap(),
            "Chat session ended with exit code 2."
        );
    }

    #[test]
    fn render_init_returns_none_so_summary_box_is_only_output() {
        let o = InitOutcome {
            agent: "claude".into(),
            aspec_requested: true,
            summary: crate::command::commands::init::SerializableInitSummary {
                aspec_folder: StepStatus::Done,
                dockerfile: StepStatus::Done,
                config: StepStatus::Done,
                audit: StepStatus::Skipped,
                image_build: StepStatus::Skipped,
                work_items_setup: StepStatus::Skipped,
            },
        };
        assert!(render_init(&o).is_none());
    }

    #[test]
    fn render_ready_returns_none_so_summary_box_is_only_output() {
        let o = ReadyOutcome {
            runtime: "docker".into(),
            base_image: StepStatus::Done,
            agent_image: StepStatus::Done,
            local_agent: StepStatus::Done,
            audit: StepStatus::Skipped,
            legacy_migration: StepStatus::Skipped,
        };
        assert!(render_ready(&o).is_none());
    }

    #[test]
    fn render_config_get_handles_missing_values() {
        let o = ConfigGetOutcome {
            field: "agent".into(),
            global_value: Some("claude".into()),
            repo_value: None,
            effective_value: Some("claude".into()),
        };
        let s = render_config_get(&o);
        assert!(s.contains("Field: agent"));
        assert!(s.contains("Global:    claude"));
        assert!(s.contains("Repo:      N/A"));
        assert!(s.contains("Effective: claude"));
    }

    #[test]
    fn render_headless_status_running_with_pid() {
        let s = render_headless_status(&HeadlessStatusOutcome {
            running: true,
            pid: Some(1234),
        });
        assert_eq!(s, "Headless server is running (PID 1234).");
    }

    #[test]
    fn render_headless_status_not_running() {
        let s = render_headless_status(&HeadlessStatusOutcome {
            running: false,
            pid: None,
        });
        assert_eq!(s, "Headless server is not running.");
    }

    #[test]
    fn render_remote_run_includes_session_when_present() {
        let s = render_remote_run(&RemoteRunOutcome {
            command: vec!["status".into()],
            session: Some("abc123".into()),
            remote_addr: None,
        });
        assert!(s.contains("status"));
        assert!(s.contains("abc123"));
    }

    #[test]
    fn render_auth_accepted_vs_declined() {
        assert!(render_auth(&AuthOutcome { accepted: true })
            .unwrap()
            .contains("accepted"));
        assert!(render_auth(&AuthOutcome { accepted: false })
            .unwrap()
            .contains("declined"));
    }
}
