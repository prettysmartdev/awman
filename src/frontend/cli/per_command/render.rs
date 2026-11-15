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

use crate::command::commands::chat::ChatOutcome;
use crate::command::commands::config::{
    ConfigGetOutcome, ConfigOutcome, ConfigSetOutcome, ConfigShowOutcome,
};
use crate::command::commands::exec_prompt::ExecPromptOutcome;
use crate::command::commands::exec_workflow::ExecWorkflowOutcome;
use crate::command::commands::init::InitOutcome;
use crate::command::commands::new::{
    NewOutcome, NewSkillOutcome, NewSpecOutcome, NewWorkflowOutcome,
};
use crate::command::commands::ready::ReadyOutcome;
use crate::command::commands::remote::{
    RemoteExecOutcome, RemoteOutcome, RemoteSessionKillOutcome, RemoteSessionStartOutcome,
};
use crate::command::commands::specs::{SpecsAmendOutcome, SpecsOutcome};
use crate::command::CommandOutcome;
use crate::frontend::render_helpers::format_table;

// ─── Top-level dispatcher ────────────────────────────────────────────────────

/// Format a [`CommandOutcome`] into the success-path stdout text. Returns
/// `None` when no extra output is needed (engines that stream their progress
/// to stderr already and produce no additional summary on stdout).
pub fn render(outcome: &CommandOutcome, json: bool) -> Option<String> {
    match outcome {
        CommandOutcome::Relayed(result) => result
            .outcome
            .as_ref()
            .filter(|_| json)
            .map(|value| value.to_string()),
        CommandOutcome::Attach(_) => None,
        CommandOutcome::Empty => None,
        CommandOutcome::Link(s) => Some(s.clone()),
        CommandOutcome::Agents(o) => super::agents::render_agents(o, json),
        CommandOutcome::Chat(o) => render_chat(o),
        CommandOutcome::Init(o) => render_init(o),
        CommandOutcome::Ready(o) => render_ready(o),
        CommandOutcome::ExecPrompt(o) => render_exec_prompt(o),
        CommandOutcome::ExecWorkflow(o) if json => {
            Some(serde_json::to_string(o).expect("workflow outcome serializes"))
        }
        CommandOutcome::ExecWorkflow(o) => render_exec_workflow(o),
        CommandOutcome::Config(o) => render_config(o, json),
        CommandOutcome::WorkerLink(o) => Some(if json {
            to_json(o)
        } else {
            format!("Linked {} at {}", o.name, o.url)
        }),
        CommandOutcome::Queue(o) => Some(if json {
            to_json(o)
        } else if o.items.is_empty() {
            "No eligible work items.".into()
        } else {
            o.items
                .iter()
                .map(|row| {
                    format!(
                        "{}  {}  [{}]  priority {}",
                        row.id, row.title, row.server, row.priority
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }),
        CommandOutcome::QueuePick(o) => Some(if json {
            to_json(o)
        } else {
            format!(
                "Work item finished with exit code {}.",
                o.exit_code.unwrap_or(1)
            )
        }),
        CommandOutcome::Daemon(o) => super::daemon::render(o, json),
        CommandOutcome::DaemonApi(o) => Some(super::daemon::render_api(o, json)),
        CommandOutcome::Remote(o) => render_remote(o),
        CommandOutcome::New(o) => render_new(o),
        CommandOutcome::Specs(o) => render_specs(o),
        CommandOutcome::Clean(o) => render_clean(o),
        CommandOutcome::Squad(o) => super::squad::render_squad(o, json),
        CommandOutcome::SquadAttach(_) => None,
    }
}

// ─── clean ─────────────────────────────────────────────────────────────────────
//
// `awman clean` streams its itemized summary, confirmation, and result
// messages through the frontend during the run (CLI: stdout + message queue).
// The success outcome adds nothing further on stdout.

fn render_clean(_o: &crate::command::commands::clean::CleanOutcome) -> Option<String> {
    None
}

// ─── chat / exec prompt / exec workflow ──────────────────────────────────────
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
    // `--check-prerequisites` ran no workflow: the table already went to
    // stderr when it was shown, so stdout carries the verdict.
    if let Some(report) = &o.prerequisites {
        let verdict = if o.exit_code == Some(0) {
            "ok"
        } else {
            "needs attention"
        };
        return Some(format!(
            "Workflow {} prerequisites: {verdict} — {}.",
            o.workflow,
            report.summary_line()
        ));
    }
    let exit = match o.exit_code {
        Some(c) if c != 0 => format!(" (exit {c})"),
        _ => String::new(),
    };
    let wt = if o.worktree_used {
        " in isolated worktree"
    } else {
        ""
    };
    Some(format!("Workflow {} completed{exit}{wt}.", o.workflow))
}

// ─── init / ready ────────────────────────────────────────────────────────────
//
// These engines emit their summary box via `report_summary` (replayed to
// stderr from the message queue). The success-path stdout output is None.

fn render_init(_o: &InitOutcome) -> Option<String> {
    None
}

fn render_ready(o: &ReadyOutcome) -> Option<String> {
    if o.json_requested {
        Some(serde_json::to_string_pretty(&o.to_legacy_json()).unwrap_or_else(|_| "{}".into()))
    } else {
        None
    }
}

// ─── config ──────────────────────────────────────────────────────────────────

fn render_config(o: &ConfigOutcome, json: bool) -> Option<String> {
    match o {
        ConfigOutcome::Show(s) if json => Some(to_json(s)),
        ConfigOutcome::Get(g) if json => Some(to_json(g)),
        ConfigOutcome::Show(s) => Some(render_config_show(s)),
        ConfigOutcome::Get(g) => Some(render_config_get(g)),
        ConfigOutcome::Set(s) | ConfigOutcome::Unset(s) => Some(render_config_set(s)),
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).expect("config outcome serializes")
}

/// The `source` column: which layer the effective value comes from.
fn layer_label(layer: crate::data::config::effective::Layer) -> String {
    serde_json::to_value(layer)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn render_config_show(o: &ConfigShowOutcome) -> String {
    let na = "—";
    let headers = ["Key", "Global", "Repo", "Effective", "Source", "Type"];
    let rows: Vec<Vec<String>> = o
        .rows
        .iter()
        .map(|r| {
            let label = if r.read_only {
                format!("{} (read-only)", r.field)
            } else {
                r.field.clone()
            };
            let (source, kind) = if r.shape.is_header() {
                (String::new(), String::new())
            } else {
                (layer_label(r.source), r.type_name.clone())
            };
            vec![
                label,
                r.global_value.clone().unwrap_or_else(|| na.to_string()),
                r.repo_value.clone().unwrap_or_else(|| na.to_string()),
                r.effective_value.clone().unwrap_or_else(|| na.to_string()),
                source,
                kind,
            ]
        })
        .collect();
    let mut out = String::from("AWMAN CONFIG\n\n");
    out.push_str(&format_table(&headers, &rows));
    out.push_str(&format!("\n\n{}", o.resolved_backend));
    if let Some(state) = &o.state {
        let accepted = state
            .auto_agent_auth_accepted
            .map_or_else(|| na.to_string(), |b| b.to_string());
        out.push_str(&format!(
            "\n\nSTATE (read-only, not config: {})\n\n",
            state.path
        ));
        out.push_str(&format_table(
            &["Item", "Value"],
            &[vec!["autoAgentAuthAccepted".to_string(), accepted]],
        ));
    }
    for note in &o.notes {
        out.push_str(&format!("\n\nNote: {note}"));
    }
    out
}

fn render_config_get(o: &ConfigGetOutcome) -> String {
    let na = || "—".to_string();
    format!(
        "Key: {}\n  Global:    {}\n  Repo:      {}\n  Effective: {}\n  Source:    {}\n  Type:      {}",
        o.field,
        o.global_value.clone().unwrap_or_else(na),
        o.repo_value.clone().unwrap_or_else(na),
        o.effective_value.clone().unwrap_or_else(na),
        layer_label(o.source),
        o.type_name,
    )
}

/// `wrote <path>` first, always, then the live-apply line.
fn render_config_set(o: &ConfigSetOutcome) -> String {
    let what = match &o.value {
        Some(value) => format!("set {} = {value}", o.field),
        None => format!("unset {}", o.field),
    };
    format!(
        "{what} ({} config)\nwrote {}\n{}",
        o.scope,
        o.path,
        o.live.message()
    )
}

// ─── api ────────────────────────────────────────────────────────────────

// ─── remote ──────────────────────────────────────────────────────────────────

fn render_remote(o: &RemoteOutcome) -> Option<String> {
    match o {
        RemoteOutcome::ExecWorkflow(r) => Some(render_remote_exec(r)),
        RemoteOutcome::ExecPrompt(r) => Some(render_remote_exec(r)),
        RemoteOutcome::SessionStart(s) => Some(render_remote_session_start(s)),
        RemoteOutcome::SessionKill(k) => Some(render_remote_session_kill(k)),
    }
}

fn render_remote_exec(o: &RemoteExecOutcome) -> String {
    let status_part = o
        .status
        .as_deref()
        .map(|s| format!(" [{s}]"))
        .unwrap_or_default();
    format!(
        "Command {}: {} (session {}) via {}{status_part}",
        o.command_id, o.subcommand, o.session, o.remote_addr,
    )
}

fn render_remote_session_start(o: &RemoteSessionStartOutcome) -> String {
    let status_part = o
        .setup_status
        .as_deref()
        .map(|s| format!(" [{s}]"))
        .unwrap_or_default();
    format!(
        "Session {} created via {}{status_part}.",
        o.session_id, o.remote_addr,
    )
}

fn render_remote_session_kill(o: &RemoteSessionKillOutcome) -> String {
    format!("Session {} killed via {}.", o.session_id, o.remote_addr)
}

// ─── new ─────────────────────────────────────────────────────────────────────

fn render_new(o: &NewOutcome) -> Option<String> {
    match o {
        NewOutcome::Spec(s) => render_new_spec(s),
        NewOutcome::Workflow(w) => Some(render_new_workflow(w)),
        NewOutcome::Skill(s) => render_new_skill(s),
        NewOutcome::Agent(a) => Some(a.render()),
    }
}

fn render_new_spec(o: &NewSpecOutcome) -> Option<String> {
    match (&o.path, &o.url) {
        (Some(p), _) => Some(format!("Created work item: {p}")),
        // The command already announced the URL the moment the provider
        // confirmed it (possibly while the agent was still running); the JSON
        // outcome keeps it, the human summary does not print it twice.
        (None, Some(_)) => None,
        // A remote interview whose draft was never edited creates nothing.
        (None, None) if o.interview => Some("No work item was created.".to_string()),
        (None, None) => Some("Work item created.".to_string()),
    }
}

fn render_new_workflow(o: &NewWorkflowOutcome) -> String {
    let scope = if o.global { "global" } else { "repo" };
    match &o.path {
        Some(p) => format!("Created workflow ({scope}, format={}): {p}", o.format),
        None => format!("Workflow created ({scope}, format={}).", o.format),
    }
}

fn render_new_skill(o: &NewSkillOutcome) -> Option<String> {
    // `--pull` / `--pull-all` create nothing and are not repo-scoped: they
    // clone or refresh a managed library under the global skills store, and
    // already reported each library through the message sink. Rendering a
    // "Created skill (repo)" summary here would contradict both.
    if o.pull {
        return None;
    }
    let scope = if o.global { "global" } else { "repo" };
    Some(match &o.path {
        Some(p) => format!("Created skill ({scope}): {p}"),
        None => format!("Skill created ({scope})."),
    })
}

// ─── specs / auth / download ─────────────────────────────────────────────────

fn render_specs(o: &SpecsOutcome) -> Option<String> {
    match o {
        SpecsOutcome::Amend(a) => Some(render_specs_amend(a)),
    }
}

fn render_specs_amend(o: &SpecsAmendOutcome) -> String {
    format!("Amended work item {}.", o.work_item)
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_new_spec_url_is_not_rendered_a_second_time() {
        let spec = |path: Option<&str>, url: Option<&str>, interview| NewSpecOutcome {
            interview,
            path: path.map(Into::into),
            url: url.map(Into::into),
            number: Some(73),
        };
        // The command announced the URL when the provider confirmed it.
        assert_eq!(
            render_new_spec(&spec(None, Some("https://x/issues/73"), false)),
            None
        );
        assert_eq!(
            render_new_spec(&spec(Some("aspec/0007-x.md"), None, false)).as_deref(),
            Some("Created work item: aspec/0007-x.md")
        );
        assert_eq!(
            render_new_spec(&spec(None, None, true)).as_deref(),
            Some("No work item was created.")
        );
    }
}
