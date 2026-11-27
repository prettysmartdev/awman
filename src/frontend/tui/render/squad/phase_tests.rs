//! How a run's building phase and final outcome are drawn (WI 0105): the task
//! card, the task detail, the history row and the run detail.

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use super::*;
use crate::data::fs::task_store::{MountScope, Run};
use crate::data::ids::{RunId, TaskId};
use crate::frontend::tui::dialogs::{
    Dialog, SquadDetailState, SquadHistoryState, SquadRunDetailState,
};

const FINISHED: [(RunStatus, &str, &str); 5] = [
    (RunStatus::NotTriggered, "not triggered", "not triggered"),
    (RunStatus::WorkflowExecuted, "workflow executed", "executed"),
    (RunStatus::Failed, "failed", "failed"),
    (RunStatus::Interrupted, "interrupted", "interrupted"),
    (RunStatus::Canceled, "canceled", "canceled"),
];

fn at() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_767_225_600, 0).unwrap()
}

fn task(status: Option<RunStatus>, phase: Option<RunPhase>) -> Task {
    Task {
        prerequisites: None,
        id: TaskId::new(),
        name: "alpha".into(),
        description: "d".into(),
        repo_scope: "workspace".into(),
        mount_scope: MountScope::Directory,
        overlays: vec![],
        interval_secs: 60,
        status: TaskStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: at(),
        updated_at: at(),
        last_run_at: status.map(|_| at()),
        trigger_requested_at: None,
        last_run_status: status,
        last_run_phase: phase,
        unmet_env: vec![],
    }
}

fn run(status: RunStatus, phase: Option<RunPhase>) -> Run {
    Run {
        id: RunId::new(),
        task_id: TaskId::new(),
        status,
        workflow_path: None,
        workflow_state_path: None,
        session_id: None,
        started_at: at(),
        finished_at: (status != RunStatus::Running).then(at),
        error: None,
        reason: None,
        unmet_env: vec![],
        phase,
        image_warning: None,
    }
}

/// The drawn screen, one line per row.
fn screen(width: u16, height: u16, draw: impl FnOnce(Rect, &mut Frame)) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame.area(), frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn dialog(dialog: &Dialog) -> String {
    screen(120, 40, |area, frame| {
        super::super::dialog::render_dialog(dialog, area, frame)
    })
}

fn card(task: &Task) -> String {
    screen(40, 7, |area, frame| {
        render_task_card(task, false, area, frame)
    })
}

fn detail(task: &Task) -> String {
    dialog(&Dialog::SquadTaskDetail(SquadDetailState {
        name: task.name.clone(),
        task: task.clone(),
    }))
}

fn history(runs: Vec<Run>, open: Option<Run>) -> String {
    dialog(&Dialog::SquadTaskHistory(SquadHistoryState {
        name: "alpha".into(),
        runs,
        selected: 0,
        from_detail: false,
        run_detail: open.map(|run| SquadRunDetailState {
            run,
            scroll: 0,
            max_scroll: Default::default(),
            workflow: crate::frontend::tui::dialogs::RunWorkflowLoad::Loading,
            hscroll: 0,
            max_hscroll: Default::default(),
        }),
    }))
}

fn history_workflow(workflow: crate::frontend::tui::dialogs::RunWorkflowLoad) -> String {
    let run = run(RunStatus::Failed, None);
    dialog(&Dialog::SquadTaskHistory(SquadHistoryState {
        name: "alpha".into(),
        runs: vec![run.clone()],
        selected: 0,
        from_detail: false,
        run_detail: Some(SquadRunDetailState {
            run,
            scroll: 0,
            max_scroll: Default::default(),
            workflow,
            hscroll: 0,
            max_hscroll: Default::default(),
        }),
    }))
}

/// The row of the history table for the run that started at [`at`].
fn row(screen: &str) -> String {
    screen
        .lines()
        .find(|line| line.contains("2026-01-01 00:00"))
        .unwrap_or_else(|| panic!("no run row:\n{screen}"))
        .to_string()
}

#[test]
fn the_label_is_the_phase_only_while_the_run_is_running() {
    let building = Some(RunPhase::Building);
    assert_eq!(run_status_label(RunStatus::Running, None), "running");
    assert_eq!(run_status_label(RunStatus::Running, building), "building");
    for (status, label, _) in FINISHED {
        assert_eq!(run_status_label(status, None), label);
        assert_eq!(run_status_label(status, building), label);
    }
}

#[test]
fn empty_and_unavailable_workflow_states_keep_the_run_detail_sections() {
    use crate::data::fs::{RunWorkflow, RunWorkflowUnavailable};
    let no_workflow = history_workflow(crate::frontend::tui::dialogs::RunWorkflowLoad::Loaded(
        RunWorkflow::NoWorkflow,
    ));
    assert!(no_workflow.contains("This run did not execute a workflow."));
    assert!(no_workflow.contains("Reason:"));
    assert!(no_workflow.contains("Error:"));
    let unavailable = history_workflow(crate::frontend::tui::dialogs::RunWorkflowLoad::Loaded(
        RunWorkflow::Unavailable {
            reason: RunWorkflowUnavailable::Missing,
        },
    ));
    assert!(unavailable.contains("Workflow state is no longer available for this run."));
    assert!(unavailable.contains("Reason:"));
    assert!(unavailable.contains("Error:"));
}

#[test]
fn building_ranks_where_running_does() {
    let mut building = task(Some(RunStatus::Running), Some(RunPhase::Building));
    building.trigger_requested_at = Some(at());
    assert_eq!(card_status(&building), CardStatus::Building);
    assert_ne!(CardStatus::Building.color(), CardStatus::Running.color());
    building.status = TaskStatus::Paused;
    assert_eq!(card_status(&building), CardStatus::Paused);
    assert_eq!(
        card_status(&task(Some(RunStatus::Running), None)),
        CardStatus::Running
    );
    assert_eq!(
        card_status(&task(Some(RunStatus::Failed), Some(RunPhase::Building))),
        CardStatus::Failed
    );
}

#[test]
fn the_card_and_detail_show_building_then_running_then_the_outcome() {
    let building = task(Some(RunStatus::Running), Some(RunPhase::Building));
    assert!(card(&building).contains("Outcome: building"));
    assert!(detail(&building).contains("Last run: building"));

    let running = task(Some(RunStatus::Running), None);
    assert!(card(&running).contains("Outcome: running"));
    assert!(detail(&running).contains("Last run: running"));

    for (status, label, _) in FINISHED {
        let finished = task(Some(status), None);
        assert!(card(&finished).contains(&format!("Outcome: {label}")));
        assert!(detail(&finished).contains(&format!("Last run: {label}")));
    }

    let never = task(None, None);
    assert!(card(&never).contains("Outcome: never run"));
    assert!(!detail(&never).contains("Last run"));
}

#[test]
fn the_history_row_and_run_detail_follow_the_live_run() {
    let building = run(RunStatus::Running, Some(RunPhase::Building));
    assert!(row(&history(vec![building.clone()], None)).contains("building"));
    assert!(history(vec![building.clone()], Some(building)).contains("Status: building"));

    let running = run(RunStatus::Running, None);
    let drawn = row(&history(vec![running], None));
    assert!(drawn.contains("running") && !drawn.contains("building"));

    for (status, _, short) in FINISHED {
        let finished = run(status, None);
        assert!(row(&history(vec![finished.clone()], None)).contains(short));
        assert!(
            history(vec![finished.clone()], Some(finished)).contains(&format!("Status: {short}"))
        );
    }
}

#[test]
fn a_stale_image_warning_marks_the_row_and_is_explained_in_the_run_detail() {
    let mut stale = run(RunStatus::NotTriggered, None);
    stale.image_warning = Some("squad-alpha is out of date; rebuild failed: exit code 1".into());
    // The longest status keeps its marker.
    assert!(row(&history(vec![stale.clone()], None)).contains("not triggered \u{26a0}"));

    let drawn = history(vec![stale.clone()], Some(stale));
    assert!(drawn.contains("Image warning:"), "{drawn}");
    assert!(drawn.contains("squad-alpha is out of date"), "{drawn}");
    assert!(drawn.contains("Reason:") && drawn.contains("Error:"));

    let clean = run(RunStatus::NotTriggered, None);
    let drawn = history(vec![clean.clone()], Some(clean));
    assert!(!drawn.contains("Image warning") && !drawn.contains('\u{26a0}'));
}

fn dag_detail(dag: crate::data::fs::RunDag, width: u16, hscroll: usize) -> ratatui::buffer::Buffer {
    let run = run(RunStatus::WorkflowExecuted, None);
    let state = Dialog::SquadTaskHistory(SquadHistoryState {
        name: "alpha".into(),
        runs: vec![run.clone()],
        selected: 0,
        from_detail: false,
        run_detail: Some(SquadRunDetailState {
            run,
            scroll: 0,
            max_scroll: Default::default(),
            workflow: crate::frontend::tui::dialogs::RunWorkflowLoad::Loaded(
                crate::data::fs::RunWorkflow::Dag(dag),
            ),
            hscroll,
            max_hscroll: Default::default(),
        }),
    });
    let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
    terminal
        .draw(|frame| super::super::dialog::render_dialog(&state, frame.area(), frame))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn node(name: &str, stage: usize, deps: &[&str]) -> crate::data::fs::RunDagNode {
    crate::data::fs::RunDagNode {
        name: name.into(),
        kind: crate::data::fs::RunDagNodeKind::Step,
        outcome: crate::data::fs::RunDagOutcome::Succeeded,
        depends_on: deps.iter().map(|d| d.to_string()).collect(),
        agent: None,
        model: None,
        stage,
    }
}

fn rows(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn unordered_stages_render_one_header_per_stage() {
    let dag = crate::data::fs::RunDag {
        run_id: RunId::new(),
        workflow_name: "flow".into(),
        // Deliberately not in stage order, as an older daemon could send it.
        nodes: vec![
            node("plan", 0, &[]),
            node("build", 1, &["plan"]),
            node("lint", 0, &[]),
        ],
    };
    let text = rows(&dag_detail(dag, 120, 0)).join("\n");
    assert_eq!(text.matches("stage 1").count(), 1, "{text}");
    assert_eq!(text.matches("stage 2").count(), 1, "{text}");
    let lint = text.find("lint").unwrap();
    let stage2 = text.find("stage 2").unwrap();
    assert!(lint < stage2, "lint must be listed in stage 1:\n{text}");
}

#[test]
fn a_panned_wide_dag_keeps_its_outcome_colours() {
    let long = "x".repeat(200);
    for hscroll in [0, 8] {
        let dag = crate::data::fs::RunDag {
            run_id: RunId::new(),
            workflow_name: "flow".into(),
            nodes: vec![
                node("plan-step-with-a-readable-name", 0, &[]),
                node(&long, 1, &["plan-step-with-a-readable-name"]),
            ],
        };
        let buffer = dag_detail(dag, 80, hscroll);
        let lines = rows(&buffer);
        let y = lines
            .iter()
            .position(|line| line.contains("readable-name") && !line.contains('←'))
            .unwrap_or_else(|| panic!("no step row at hscroll {hscroll}:\n{}", lines.join("\n")));
        let coloured = (0..buffer.area.width).any(|x| {
            let cell = &buffer[(x, y as u16)];
            !cell.symbol().trim().is_empty()
                && cell.fg != ratatui::style::Color::Reset
                && !cell.symbol().chars().all(|c| c.is_ascii_alphanumeric())
                && cell.symbol() != "‹"
                && cell.symbol() != "│"
        });
        assert!(
            coloured,
            "glyph lost its colour at hscroll {hscroll}:\n{}",
            lines.join("\n")
        );
        if hscroll > 0 {
            assert!(lines[y].contains('‹'), "{}", lines[y]);
        }
    }
}
