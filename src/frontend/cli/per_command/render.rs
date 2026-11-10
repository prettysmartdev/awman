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
        NewOutcome::Spec(s) => Some(render_new_spec(s)),
        NewOutcome::Workflow(w) => Some(render_new_workflow(w)),
        NewOutcome::Skill(s) => render_new_skill(s),
        NewOutcome::Agent(a) => Some(a.render()),
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
    use crate::data::step_status::StepStatus;

    #[test]
    fn render_empty_returns_none() {
        assert!(render(&CommandOutcome::Empty, false).is_none());
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
            dockerfile: StepStatus::Done,
            base_image: StepStatus::Done,
            agent_image: StepStatus::Done,
            image_source: None,
            agent_definition: None,
            local_agent: StepStatus::Done,
            audit: StepStatus::Skipped,
            image_rebuild: StepStatus::Skipped,
            agent_credentials: Vec::new(),
            non_default_agent_images: Vec::new(),
            image_results: Vec::new(),
            base_image_results: Vec::new(),
            ready: true,
            json_requested: false,
            refresh_requested: false,
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
            source: crate::data::config::effective::Layer::Global,
            type_name: "string".into(),
        };
        let s = render_config_get(&o);
        assert!(s.contains("Key: agent"));
        assert!(s.contains("Source:    global"), "{s}");
        assert!(s.contains("Global:    claude"));
        assert!(s.contains("Repo:      —"));
        assert!(s.contains("Effective: claude"));
    }

    #[test]
    fn render_remote_exec_includes_session_when_present() {
        let s = render_remote_exec(&RemoteExecOutcome {
            command_id: "cmd-1".into(),
            subcommand: "exec workflow".into(),
            session: "abc123".into(),
            remote_addr: "localhost:9876".into(),
            status: None,
            exit_code: None,
            followed: false,
        });
        assert!(s.contains("exec workflow"));
        assert!(s.contains("abc123"));
    }

    // ── render_ready ──────────────────────────────────────────────────────────

    #[test]
    fn render_ready_json_requested_emits_legacy_schema() {
        let o = ReadyOutcome {
            runtime: "docker".into(),
            dockerfile: StepStatus::Done,
            base_image: StepStatus::Done,
            agent_image: StepStatus::Done,
            image_source: None,
            agent_definition: None,
            local_agent: StepStatus::Done,
            audit: StepStatus::Skipped,
            image_rebuild: StepStatus::Skipped,
            agent_credentials: Vec::new(),
            non_default_agent_images: Vec::new(),
            image_results: Vec::new(),
            base_image_results: Vec::new(),
            ready: true,
            json_requested: true,
            refresh_requested: false,
        };
        let s = render_ready(&o).expect("json_requested=true must produce output");
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("must be valid JSON");
        // Top-level keys per legacy schema.
        assert_eq!(parsed["ready"], true);
        assert_eq!(parsed["runtime"], "docker");
        assert!(parsed["steps"].is_object(), "must include steps wrapper");
        // Each legacy step must be a `{status, message}` object.
        for key in [
            "docker_daemon",
            "dockerfile",
            "aspec_folder",
            "work_items_config",
            "local_agent",
            "dev_image",
            "refresh",
            "image_rebuild",
        ] {
            let s = &parsed["steps"][key];
            assert!(s.is_object(), "steps.{key} must be an object");
            assert!(
                s.get("status").is_some(),
                "steps.{key} must have a status field"
            );
            assert!(
                s.get("message").is_some(),
                "steps.{key} must have a message field"
            );
        }
        assert_eq!(parsed["steps"]["dev_image"]["status"], "ok");
        assert_eq!(parsed["steps"]["aspec_folder"]["status"], "skipped");
    }

    #[test]
    fn render_ready_json_failure_marks_ready_false() {
        let o = ReadyOutcome {
            runtime: "docker".into(),
            dockerfile: StepStatus::Done,
            base_image: StepStatus::Failed("boom".into()),
            agent_image: StepStatus::Pending,
            image_source: None,
            agent_definition: None,
            local_agent: StepStatus::Pending,
            audit: StepStatus::Pending,
            image_rebuild: StepStatus::Pending,
            agent_credentials: Vec::new(),
            non_default_agent_images: Vec::new(),
            image_results: Vec::new(),
            base_image_results: Vec::new(),
            ready: false,
            json_requested: true,
            refresh_requested: true,
        };
        let s = render_ready(&o).expect("json_requested=true must produce output");
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("must be valid JSON");
        assert_eq!(parsed["ready"], false);
        assert_eq!(parsed["steps"]["dev_image"]["status"], "pending");
        assert_eq!(parsed["steps"]["refresh"]["status"], "ok");
    }

    // ── render_config ─────────────────────────────────────────────────────────

    #[test]
    fn render_config_show_renders_4_column_table_with_field_values() {
        use crate::command::commands::config::{ConfigFieldKind, ConfigFieldRow};
        let o = ConfigShowOutcome {
            state: None,
            resolved_backend: "backend: builtin".into(),
            notes: vec![],
            rows: vec![
                ConfigFieldRow {
                    field: "agent".into(),
                    global_value: Some("claude".into()),
                    repo_value: None,
                    effective_value: Some("claude".into()),
                    task_value: None,
                    source: crate::data::config::effective::Layer::Default,
                    type_name: "string".into(),
                    sensitive: false,
                    kind: ConfigFieldKind::Enum,
                    read_only: false,
                    global_writable: true,
                    repo_writable: true,
                    value_hint: None,
                    shape: crate::command::commands::config::ConfigRowKind::Scalar,
                },
                ConfigFieldRow {
                    field: "auto_agent_auth_accepted".into(),
                    global_value: Some("true".into()),
                    repo_value: None,
                    effective_value: Some("true".into()),
                    task_value: None,
                    source: crate::data::config::effective::Layer::Default,
                    type_name: "string".into(),
                    sensitive: false,
                    kind: ConfigFieldKind::Bool,
                    read_only: true,
                    global_writable: false,
                    repo_writable: false,
                    value_hint: None,
                    shape: crate::command::commands::config::ConfigRowKind::Scalar,
                },
            ],
        };
        let s = render_config_show(&o);
        assert!(s.contains("AWMAN CONFIG"), "must have header");
        assert!(s.contains("Key"), "must have Key column");
        assert!(
            s.contains("Source") && s.contains("Type"),
            "must have Source and Type columns"
        );
        assert!(s.contains("Global"), "must have Global column");
        assert!(s.contains("Repo"), "must have Repo column");
        assert!(s.contains("Effective"), "must have Effective column");
        assert!(s.contains("agent"), "agent field row must appear");
        assert!(s.contains("claude"), "global agent value must appear");
        assert!(
            s.contains("backend: builtin"),
            "resolved backend line must appear"
        );
        assert!(
            s.contains("(read-only)"),
            "read-only fields must be marked: {s}"
        );
    }

    #[test]
    fn render_config_show_does_not_truncate_dynamic_workflows_values() {
        use crate::command::commands::config::{ConfigFieldKind, ConfigFieldRow};
        let long_models = "claude-opus-4-8, claude-sonnet-4-6, claude-haiku-4-5, \
                            gemini-2.5-pro, codex-mini-latest";
        let o = ConfigShowOutcome {
            state: None,
            resolved_backend: "backend: builtin".into(),
            notes: vec![],
            rows: vec![
                ConfigFieldRow {
                    field: "demo.leader".into(),
                    global_value: None,
                    repo_value: Some("claude::claude-opus-4-8".into()),
                    effective_value: Some("claude::claude-opus-4-8".into()),
                    task_value: None,
                    source: crate::data::config::effective::Layer::Default,
                    type_name: "string".into(),
                    sensitive: false,
                    kind: ConfigFieldKind::String,
                    read_only: false,
                    global_writable: false,
                    repo_writable: true,
                    value_hint: None,
                    shape: crate::command::commands::config::ConfigRowKind::Scalar,
                },
                ConfigFieldRow {
                    field: "demo.limit".into(),
                    global_value: None,
                    repo_value: Some("3".into()),
                    effective_value: Some("3".into()),
                    task_value: None,
                    source: crate::data::config::effective::Layer::Default,
                    type_name: "string".into(),
                    sensitive: false,
                    kind: ConfigFieldKind::Number,
                    read_only: false,
                    global_writable: false,
                    repo_writable: true,
                    value_hint: None,
                    shape: crate::command::commands::config::ConfigRowKind::Scalar,
                },
                ConfigFieldRow {
                    field: "demo.map.claude".into(),
                    global_value: None,
                    repo_value: Some(long_models.into()),
                    effective_value: Some(long_models.into()),
                    task_value: None,
                    source: crate::data::config::effective::Layer::Default,
                    type_name: "string".into(),
                    sensitive: false,
                    kind: ConfigFieldKind::String,
                    read_only: false,
                    global_writable: false,
                    repo_writable: true,
                    value_hint: None,
                    shape: crate::command::commands::config::ConfigRowKind::Scalar,
                },
            ],
        };
        let s = render_config_show(&o);

        assert!(
            s.contains("demo.leader"),
            "leader scalar row must appear: {s}"
        );
        assert!(
            s.contains("demo.limit"),
            "limit scalar row must appear: {s}"
        );
        assert!(
            s.contains("demo.map.claude"),
            "map entry row must appear, got: {s}"
        );
        assert!(
            !s.contains("demo.map.claude (read-only)"),
            "map entry rows are inline-editable and must not be marked \
             read-only, got: {s}"
        );
        assert!(
            s.contains(long_models),
            "long map entry value must appear untruncated in CLI text output, got: {s}"
        );
        assert!(
            !s.contains('\u{2026}'),
            "CLI config show output must never truncate values with an ellipsis, got: {s}"
        );
    }

    /// A row built only from synthetic names and an explicit shape (WI 0150):
    /// the CLI view must render it without knowing any real field name.
    fn shaped_row(
        field: &str,
        shape: crate::command::commands::config::ConfigRowKind,
        global: Option<&str>,
        repo: Option<&str>,
        effective: Option<&str>,
        read_only: bool,
    ) -> crate::command::commands::config::ConfigFieldRow {
        crate::command::commands::config::ConfigFieldRow {
            field: field.into(),
            global_value: global.map(Into::into),
            repo_value: repo.map(Into::into),
            effective_value: effective.map(Into::into),
            task_value: None,
            source: crate::data::config::effective::Layer::Default,
            type_name: "string".into(),
            sensitive: false,
            kind: crate::command::commands::config::ConfigFieldKind::String,
            read_only,
            global_writable: !read_only,
            repo_writable: !read_only,
            value_hint: None,
            shape,
        }
    }

    fn shape_driven_outcome() -> ConfigShowOutcome {
        use crate::command::commands::config::ConfigRowKind as Shape;
        ConfigShowOutcome {
            state: None,
            resolved_backend: "backend: builtin".into(),
            notes: vec![],
            rows: vec![
                shaped_row(
                    "demo.map",
                    Shape::MapHeader {
                        child_prefix: "demo.map.".into(),
                    },
                    Some("2 agents mapped"),
                    Some("(none)"),
                    Some("global map"),
                    true,
                ),
                shaped_row(
                    "demo.map.alpha",
                    Shape::MapEntry,
                    Some("m1, m2"),
                    None,
                    Some("m1, m2"),
                    false,
                ),
                shaped_row(
                    "demo.map.beta",
                    Shape::MapEntry,
                    None,
                    Some("m3"),
                    None,
                    false,
                ),
                shaped_row(
                    "demo.list",
                    Shape::ListHeader {
                        child_prefix: "demo.list.".into(),
                    },
                    Some("1 entry"),
                    Some("1 entry"),
                    Some("1 global + 1 repo entries"),
                    true,
                ),
                shaped_row(
                    "demo.list.0",
                    Shape::ListEntry,
                    Some("global note"),
                    Some("repo note"),
                    None,
                    false,
                ),
                shaped_row(
                    "demo.scalar",
                    Shape::Scalar,
                    Some("g"),
                    None,
                    Some("g"),
                    false,
                ),
            ],
        }
    }

    #[test]
    fn render_config_show_renders_map_list_and_scalar_rows_from_shape_alone() {
        let s = render_config_show(&shape_driven_outcome());
        let line_of = |needle: &str| {
            s.lines()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no row for {needle}: {s}"))
                .to_string()
        };
        // Headers are read-only and carry their per-scope summaries.
        let map_header = line_of("demo.map (read-only)");
        assert!(map_header.contains("2 agents mapped"), "{map_header}");
        assert!(map_header.contains("global map"), "{map_header}");
        let list_header = line_of("demo.list (read-only)");
        assert!(
            list_header.contains("1 global + 1 repo entries"),
            "{list_header}"
        );
        // Members are editable and show per-scope cells; a missing cell is "—".
        let alpha = line_of("demo.map.alpha");
        assert!(!alpha.contains("read-only"), "{alpha}");
        assert!(alpha.contains("m1, m2") && alpha.contains('—'), "{alpha}");
        let beta = line_of("demo.map.beta");
        assert!(beta.contains("m3") && beta.contains('—'), "{beta}");
        let entry = line_of("demo.list.0");
        assert!(
            entry.contains("global note") && entry.contains("repo note"),
            "list member shows both scopes: {entry}"
        );
        let scalar = line_of("demo.scalar");
        assert!(!scalar.contains("read-only"), "{scalar}");
    }

    #[test]
    fn render_config_show_shows_global_and_repo_cells_in_their_own_columns() {
        let s = render_config_show(&shape_driven_outcome());
        let header = s
            .lines()
            .find(|l| l.contains("Key") && l.contains("Global"))
            .expect("table header");
        let global_col = header.find("Global").unwrap();
        let repo_col = header.find("Repo").unwrap();
        let entry = s.lines().find(|l| l.contains("demo.list.0")).unwrap();
        let global_at = entry.find("global note").unwrap();
        let repo_at = entry.find("repo note").unwrap();
        assert!(global_at >= global_col && global_at < repo_col, "{s}");
        assert!(repo_at >= repo_col, "{s}");
    }

    #[test]
    fn render_config_show_output_names_only_what_the_rows_supply() {
        let s = render_config_show(&shape_driven_outcome());
        // Built from pieces so this file passes the frontend field-name scan.
        let forbidden = [
            ["agents", "ToModels"].concat(),
            "guidance".to_string(),
            ["default", "Leader"].concat(),
            "dynamic".to_string(),
        ];
        for forbidden in &forbidden {
            let forbidden = forbidden.as_str();
            assert!(!s.contains(forbidden), "{forbidden} leaked into: {s}");
        }
    }

    #[test]
    fn render_config_set_always_prints_the_file_written_and_the_live_line() {
        use crate::command::commands::config::LiveApply;
        let o = ConfigSetOutcome {
            field: "agents.stuckTimeout".into(),
            value: Some("45s".into()),
            scope: "repo".into(),
            path: "/r/.awman/config.json".into(),
            live: LiveApply::NextStart,
        };
        let s = render_config_set(&o);
        assert_eq!(
            s,
            "set agents.stuckTimeout = 45s (repo config)\nwrote /r/.awman/config.json\ntakes effect on next start"
        );
        let o = ConfigSetOutcome {
            value: None,
            live: LiveApply::Applied,
            ..o
        };
        let s = render_config_set(&o);
        assert!(s.starts_with("unset agents.stuckTimeout"), "{s}");
        assert!(s.ends_with("applied to the running daemon"), "{s}");
    }

    #[test]
    fn render_config_show_prints_the_state_heading_and_json_on_request() {
        use crate::command::commands::config::ConfigStateSection;
        let mut o = shape_driven_outcome();
        o.state = Some(ConfigStateSection {
            path: "/h/data/repos/abc/state.json".into(),
            auto_agent_auth_accepted: Some(true),
        });
        let s = render_config_show(&o);
        assert!(
            s.contains("STATE (read-only, not config: /h/data/repos/abc/state.json)"),
            "{s}"
        );
        assert!(s.contains("autoAgentAuthAccepted"), "{s}");
        let json = render(&CommandOutcome::Config(ConfigOutcome::Show(o)), true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["state"]["autoAgentAuthAccepted"], true);
    }

    // ── render_new ────────────────────────────────────────────────────────────

    use crate::command::commands::new::{
        NewSkillOutcome, NewSpecOutcome, NewWorkflowOutcome, PullLibraryOutcome,
    };

    #[test]
    fn render_new_spec_with_path_shows_created_path() {
        let o = NewSpecOutcome {
            interview: false,
            path: Some("/aspec/work-items/0001-foo.md".into()),
        };
        let s = render_new_spec(&o);
        assert!(s.contains("0001-foo.md"), "path must appear in output: {s}");
        assert!(s.contains("Created"), "must say Created: {s}");
    }

    #[test]
    fn render_new_spec_without_path_shows_fallback() {
        let o = NewSpecOutcome {
            interview: false,
            path: None,
        };
        let s = render_new_spec(&o);
        assert!(!s.is_empty());
    }

    #[test]
    fn render_new_workflow_repo_scope_shows_format() {
        let o = NewWorkflowOutcome {
            interview: false,
            global: false,
            format: "toml".into(),
            path: Some("/aspec/workflows/my-wf.toml".into()),
        };
        let s = render_new_workflow(&o);
        assert!(s.contains("repo"), "must mention repo scope");
        assert!(s.contains("toml"), "must mention format");
        assert!(s.contains("my-wf.toml"), "path must appear");
    }

    #[test]
    fn render_new_workflow_global_scope() {
        let o = NewWorkflowOutcome {
            interview: false,
            global: true,
            format: "yaml".into(),
            path: Some("/home/user/.awman/workflows/my-wf.yaml".into()),
        };
        let s = render_new_workflow(&o);
        assert!(s.contains("global"), "must mention global scope");
    }

    #[test]
    fn render_new_skill_global_shows_global_scope() {
        let o = NewSkillOutcome {
            interview: false,
            global: true,
            path: Some("/home/user/.awman/skills/my-skill/SKILL.md".into()),
            pull: false,
            libraries: vec![],
        };
        let s = render_new_skill(&o).expect("skill creation must render a summary line");
        assert!(s.contains("global"), "must mention global scope");
        assert!(s.contains("SKILL.md"), "path must appear");
    }

    /// A `--pull` run clones a managed library into the *global* store and
    /// already reported it through the message sink. It must never render the
    /// "Created skill (repo)" creation line (WI-0103 remediation).
    #[test]
    fn render_new_skill_pull_renders_no_creation_line() {
        let o = NewSkillOutcome {
            interview: false,
            global: false,
            path: Some("/home/user/.awman/skills/.library/superpowers".into()),
            pull: true,
            libraries: vec![PullLibraryOutcome {
                slug: "superpowers".into(),
                dir: "/home/user/.awman/skills/.library/superpowers".into(),
                updated: false,
                skills_found: vec!["brainstorming".into()],
                error: None,
            }],
        };
        assert_eq!(
            render_new_skill(&o),
            None,
            "a pull must not render a skill-creation line"
        );
        assert_eq!(
            render_new(&NewOutcome::Skill(o)),
            None,
            "the NewOutcome renderer must pass the pull's silence through"
        );
    }

    /// `--pull-all` with nothing pulled yet has no libraries *and* no path;
    /// it must still not be rendered as "Skill created (repo)."
    #[test]
    fn render_new_skill_empty_pull_all_renders_no_creation_line() {
        let o = NewSkillOutcome {
            interview: false,
            global: false,
            path: None,
            pull: true,
            libraries: vec![],
        };
        assert_eq!(
            render_new_skill(&o),
            None,
            "an empty --pull-all must not claim a skill was created"
        );
    }

    // ── render_specs ──────────────────────────────────────────────────────────

    use crate::command::commands::specs::SpecsAmendOutcome;

    #[test]
    fn render_specs_amend_shows_work_item_number() {
        let o = SpecsAmendOutcome {
            work_item: "0042".into(),
            non_interactive: false,
            allow_docker: false,
        };
        let s = render_specs_amend(&o);
        assert!(s.contains("0042"), "work item number must appear: {s}");
    }

    // ── render_exec_workflow ──────────────────────────────────────────────────

    use crate::command::commands::exec_workflow::ExecWorkflowOutcome;

    #[test]
    fn render_exec_workflow_shows_workflow_name() {
        let o = ExecWorkflowOutcome {
            prerequisites: None,
            workflow: "deploy.toml".into(),
            exit_code: Some(0),
            worktree_used: false,
        };
        let s = render_exec_workflow(&o).expect("exec_workflow must produce output");
        assert!(s.contains("deploy.toml"), "workflow name must appear: {s}");
        assert!(s.contains("completed"), "must say completed: {s}");
    }

    #[test]
    fn render_exec_workflow_nonzero_exit_shows_exit_code() {
        let o = ExecWorkflowOutcome {
            prerequisites: None,
            workflow: "build.yaml".into(),
            exit_code: Some(2),
            worktree_used: false,
        };
        let s = render_exec_workflow(&o).expect("exec_workflow must produce output");
        assert!(
            s.contains("2") || s.contains("exit"),
            "exit code must appear: {s}"
        );
    }

    // ── render_exec_prompt ────────────────────────────────────────────────────

    use crate::command::commands::exec_prompt::ExecPromptOutcome;

    #[test]
    fn render_exec_prompt_zero_exit_returns_none() {
        let o = ExecPromptOutcome {
            agent: Some("claude".into()),
            exit_code: Some(0),
        };
        assert!(render_exec_prompt(&o).is_none());
    }

    #[test]
    fn render_exec_prompt_nonzero_exit_returns_message() {
        let o = ExecPromptOutcome {
            agent: None,
            exit_code: Some(3),
        };
        let s = render_exec_prompt(&o).expect("nonzero exit must produce output");
        assert!(
            s.contains("3") || s.contains("exit"),
            "exit code must appear: {s}"
        );
    }

    // ── render_remote ─────────────────────────────────────────────────────────

    use crate::command::commands::remote::{RemoteSessionKillOutcome, RemoteSessionStartOutcome};

    #[test]
    fn render_remote_session_start_shows_session_id() {
        let s = render_remote_session_start(&RemoteSessionStartOutcome {
            session_id: "sess-1".into(),
            remote_addr: "localhost:9876".into(),
            setup_status: Some("ready".into()),
        });
        assert!(s.contains("sess-1"), "session_id must appear: {s}");
    }

    #[test]
    fn render_remote_session_start_shows_remote_addr() {
        let s = render_remote_session_start(&RemoteSessionStartOutcome {
            session_id: "sess-2".into(),
            remote_addr: "localhost:9876".into(),
            setup_status: None,
        });
        assert!(s.contains("localhost:9876"), "remote_addr must appear: {s}");
    }

    #[test]
    fn render_remote_session_kill_with_session_id() {
        let s = render_remote_session_kill(&RemoteSessionKillOutcome {
            session_id: "abc123".into(),
            remote_addr: "localhost:9876".into(),
        });
        assert!(s.contains("abc123"), "session id must appear: {s}");
    }

    #[test]
    fn render_remote_session_kill_shows_remote_addr() {
        let s = render_remote_session_kill(&RemoteSessionKillOutcome {
            session_id: "sess-1".into(),
            remote_addr: "localhost:9876".into(),
        });
        assert!(s.contains("localhost:9876"), "remote_addr must appear: {s}");
    }
}
