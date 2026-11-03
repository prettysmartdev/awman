//! Dynamic-workflow resume (WI-0115 §2) beyond the discovery tests in
//! `tests.rs`: every answer `offer_dynamic_resume` can give and what each
//! leaves on disk, the "cannot be resumed" notices, the prompt it asks with,
//! and the save/clear helpers that bracket a run.
//!
//! Everything is files in a temp dir; no leader, container or network runs.

use super::*;
use crate::data::fs::WorkflowDirs;
use crate::data::workflow_state_store::WorkflowStateStore;

const WI: u32 = 12;

fn three_step_toml(title: &str) -> String {
    format!(
        "title = \"{title}\"\nagent = \"claude\"\n\n\
         [[step]]\nname = \"a\"\nprompt = \"go\"\n\n\
         [[step]]\nname = \"b\"\nprompt = \"go\"\ndepends_on = [\"a\"]\n\n\
         [[step]]\nname = \"c\"\nprompt = \"go\"\ndepends_on = [\"b\"]\n"
    )
}

struct Run {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    cmd: ExecWorkflowCommand,
}

fn run_fixture() -> Run {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("checkout");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let session = Session::for_tests_isolated(&root, &tmp.path().join("config-home"));
    let cmd = ExecWorkflowCommand::new(
        make_dynamic_flags(true, None, Some("0012"), None, false, None),
        Engines::for_tests(&tmp.path().join("home")),
        session,
    );
    Run {
        _tmp: tmp,
        root,
        cmd,
    }
}

fn workflow_path(root: &Path, wi: u32) -> PathBuf {
    WorkflowDirs::dynamic_workflow_path(root, wi)
}

fn write_workflow(root: &Path, wi: u32, toml: &str) -> PathBuf {
    let path = workflow_path(root, wi);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, toml).unwrap();
    path
}

fn save_state(root: &Path, wi: Option<u32>, name: &str, statuses: &[StepState]) -> PathBuf {
    let store = WorkflowStateStore::at_git_root(root);
    let (mut state, _) = resume_fixture(&["a", "b", "c"], statuses);
    state.workflow_name = name.into();
    state.work_item = wi;
    store.save(&state).unwrap();
    store.state_path(wi, name)
}

fn offer(run: &Run, frontend: &mut FakeExecWorkflowFrontend) -> DynamicResumeOutcome {
    run.cmd
        .offer_dynamic_resume(&run.root, WI, frontend)
        .unwrap()
}

fn interrupted() -> [StepState; 3] {
    [StepState::Succeeded, failed(1), StepState::Cancelled]
}

// ── offer_dynamic_resume ─────────────────────────────────────────────────

#[test]
fn a_missing_checkout_starts_fresh_without_asking() {
    let run = run_fixture();
    let mut frontend = FakeExecWorkflowFrontend::new();
    let outcome = run
        .cmd
        .offer_dynamic_resume(&run.root.join("not-there"), WI, &mut frontend)
        .unwrap();
    assert!(matches!(outcome, DynamicResumeOutcome::Fresh));
    assert!(frontend.resume_prompts.is_empty());
    assert!(frontend.resume_unavailable.is_empty());
}

#[test]
fn an_empty_checkout_starts_fresh_silently() {
    let run = run_fixture();
    let mut frontend = FakeExecWorkflowFrontend::new();
    assert!(matches!(
        offer(&run, &mut frontend),
        DynamicResumeOutcome::Fresh
    ));
    assert!(frontend.resume_prompts.is_empty());
    assert!(
        frontend.resume_unavailable.is_empty(),
        "no previous run is not worth a notice"
    );
}

#[test]
fn the_prompt_describes_the_interrupted_run_and_its_start_points() {
    let run = run_fixture();
    write_workflow(&run.root, WI, &three_step_toml("saved"));
    save_state(&run.root, Some(WI), "saved", &interrupted());
    let mut frontend =
        FakeExecWorkflowFrontend::new().answering_resume(WorkflowResumeDecision::Cancel);
    offer(&run, &mut frontend);

    assert_eq!(frontend.resume_prompts.len(), 1);
    let prompt = &frontend.resume_prompts[0];
    assert_eq!(prompt.workflow_name, "saved");
    assert_eq!(prompt.work_item, Some(WI));
    assert!(prompt.dynamic, "accepting skips the leader-design pass");
    assert_eq!(prompt.worktree_path.as_deref(), Some(run.root.as_path()));
    assert_eq!(prompt.completed_steps, 1);
    assert_eq!(prompt.total_steps, 3);
    let names: Vec<&str> = prompt
        .start_points
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(names, ["b", "a", "c"]);
    assert!(frontend.resume_unavailable.is_empty());
}

#[test]
fn resuming_keeps_both_halves_for_the_run_to_rewrite() {
    let run = run_fixture();
    let toml = write_workflow(&run.root, WI, &three_step_toml("saved"));
    let state = save_state(&run.root, Some(WI), "saved", &interrupted());
    let mut frontend = FakeExecWorkflowFrontend::new()
        .answering_resume(WorkflowResumeDecision::ResumeFrom("b".into()));
    assert!(matches!(
        offer(&run, &mut frontend),
        DynamicResumeOutcome::Resume(_)
    ));
    assert!(toml.exists());
    assert!(state.exists());
}

#[test]
fn starting_fresh_deletes_both_halves() {
    let run = run_fixture();
    let toml = write_workflow(&run.root, WI, &three_step_toml("saved"));
    let state = save_state(&run.root, Some(WI), "saved", &interrupted());
    let mut frontend =
        FakeExecWorkflowFrontend::new().answering_resume(WorkflowResumeDecision::Fresh);
    assert!(matches!(
        offer(&run, &mut frontend),
        DynamicResumeOutcome::Fresh
    ));
    assert!(!toml.exists(), "the abandoned workflow copy is removed");
    assert!(!state.exists(), "the abandoned state is removed");
    // And so the next invocation finds nothing at all.
    assert_eq!(
        discover_previous_dynamic_run(&run.root, WI).unwrap_err(),
        DynamicDiscoveryMiss::NothingToResume
    );
}

#[test]
fn cancelling_leaves_both_halves_byte_for_byte() {
    let run = run_fixture();
    let toml = write_workflow(&run.root, WI, &three_step_toml("saved"));
    let state = save_state(&run.root, Some(WI), "saved", &interrupted());
    let before = (
        std::fs::read(&toml).unwrap(),
        std::fs::read(&state).unwrap(),
    );
    let mut frontend =
        FakeExecWorkflowFrontend::new().answering_resume(WorkflowResumeDecision::Cancel);
    assert!(matches!(
        offer(&run, &mut frontend),
        DynamicResumeOutcome::Cancelled
    ));
    assert_eq!(
        (
            std::fs::read(&toml).unwrap(),
            std::fs::read(&state).unwrap()
        ),
        before
    );
}

#[test]
fn half_a_run_is_reported_and_then_designed_afresh() {
    let run = run_fixture();
    let state = save_state(&run.root, Some(WI), "saved", &interrupted());
    let mut frontend = FakeExecWorkflowFrontend::new();
    assert!(matches!(
        offer(&run, &mut frontend),
        DynamicResumeOutcome::Fresh
    ));
    assert!(frontend.resume_prompts.is_empty());
    assert_eq!(frontend.resume_unavailable.len(), 1);
    let (wi, reason) = &frontend.resume_unavailable[0];
    assert_eq!(*wi, WI);
    assert!(reason.contains("dynamic-0012.toml"), "{reason}");
    assert!(state.exists(), "the notice deletes nothing");
}

#[test]
fn an_unparsable_saved_workflow_is_reported() {
    let run = run_fixture();
    write_workflow(&run.root, WI, "this is = = not toml [[");
    save_state(&run.root, Some(WI), "saved", &interrupted());
    let mut frontend = FakeExecWorkflowFrontend::new();
    assert!(matches!(
        offer(&run, &mut frontend),
        DynamicResumeOutcome::Fresh
    ));
    assert!(frontend.resume_prompts.is_empty());
    assert_eq!(frontend.resume_unavailable.len(), 1);
    assert!(
        frontend.resume_unavailable[0]
            .1
            .contains("no longer parses"),
        "{:?}",
        frontend.resume_unavailable
    );
}

#[test]
fn a_saved_workflow_without_matching_state_is_reported() {
    let run = run_fixture();
    write_workflow(&run.root, WI, &three_step_toml("renamed"));
    // State exists, but under the old title: the pair no longer matches.
    save_state(&run.root, Some(WI), "saved", &interrupted());
    let mut frontend = FakeExecWorkflowFrontend::new();
    assert!(matches!(
        offer(&run, &mut frontend),
        DynamicResumeOutcome::Fresh
    ));
    assert!(frontend.resume_unavailable[0]
        .1
        .contains("no saved workflow state for 'renamed'"));
}

#[test]
fn a_finished_run_whose_copy_survived_offers_nothing() {
    let run = run_fixture();
    write_workflow(&run.root, WI, &three_step_toml("saved"));
    save_state(
        &run.root,
        Some(WI),
        "saved",
        &[
            StepState::Succeeded,
            StepState::Succeeded,
            StepState::Succeeded,
        ],
    );
    let mut frontend = FakeExecWorkflowFrontend::new();
    assert!(matches!(
        offer(&run, &mut frontend),
        DynamicResumeOutcome::Fresh
    ));
    assert!(frontend.resume_prompts.is_empty());
    assert!(frontend.resume_unavailable[0]
        .1
        .contains("ran every step to completion"));
}

#[test]
fn another_work_items_run_is_never_offered() {
    let run = run_fixture();
    write_workflow(&run.root, 13, &three_step_toml("saved"));
    save_state(&run.root, Some(13), "saved", &interrupted());
    let mut frontend = FakeExecWorkflowFrontend::new();
    assert!(matches!(
        offer(&run, &mut frontend),
        DynamicResumeOutcome::Fresh
    ));
    assert!(frontend.resume_prompts.is_empty());
    assert!(frontend.resume_unavailable.is_empty());
}

// ── unresumed_dynamic_state_exists ───────────────────────────────────────

#[test]
fn only_this_work_items_unfinished_state_counts_as_unresumed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert!(
        !unresumed_dynamic_state_exists(root, WI),
        "no workflows dir"
    );

    save_state(
        root,
        Some(WI),
        "done",
        &[
            StepState::Succeeded,
            StepState::Skipped,
            StepState::Succeeded,
        ],
    );
    assert!(!unresumed_dynamic_state_exists(root, WI));

    save_state(root, Some(13), "other", &interrupted());
    assert!(
        !unresumed_dynamic_state_exists(root, WI),
        "work item 13's run is not work item 12's"
    );
    assert!(unresumed_dynamic_state_exists(root, 13));

    // A failed (terminal) run still has work left to resume.
    save_state(
        root,
        Some(WI),
        "failed",
        &[StepState::Succeeded, failed(2), StepState::Cancelled],
    );
    assert!(unresumed_dynamic_state_exists(root, WI));
}

#[test]
fn unreadable_or_foreign_files_in_the_workflows_dir_are_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = WorkflowDirs::repo_dir_for(tmp.path());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("x-0012-garbage.json"), "{not json").unwrap();
    std::fs::write(dir.join("x-0012-notes.txt"), "text").unwrap();
    assert!(!unresumed_dynamic_state_exists(tmp.path(), WI));
}

// ── save / clear around a run ────────────────────────────────────────────

#[test]
fn saving_then_clearing_round_trips_through_discovery() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("run");
    std::fs::create_dir_all(&root).unwrap();
    let generated = tmp.path().join("leader-output.toml");
    std::fs::write(&generated, three_step_toml("saved")).unwrap();

    save_dynamic_workflow_copy(&root, WI, &generated);
    let copy = workflow_path(&root, WI);
    assert_eq!(
        std::fs::read_to_string(&copy).unwrap(),
        three_step_toml("saved")
    );
    save_state(&root, Some(WI), "saved", &interrupted());
    let found = discover_previous_dynamic_run(&root, WI).unwrap();
    assert_eq!(found.workflow_path, copy);
    assert_eq!(found.workflow.steps.len(), 3);

    // Another work item's artifacts survive the clear.
    write_workflow(&root, 13, &three_step_toml("saved"));
    let other_state = save_state(&root, Some(13), "saved", &interrupted());

    clear_dynamic_resume_artifacts(&root, WI, "saved");
    assert!(!copy.exists());
    assert_eq!(
        discover_previous_dynamic_run(&root, WI).unwrap_err(),
        DynamicDiscoveryMiss::NothingToResume
    );
    assert!(workflow_path(&root, 13).exists());
    assert!(other_state.exists());

    // Clearing again (nothing left) is harmless.
    clear_dynamic_resume_artifacts(&root, WI, "saved");
}

#[test]
fn saving_a_missing_generated_workflow_is_best_effort() {
    let tmp = tempfile::tempdir().unwrap();
    save_dynamic_workflow_copy(tmp.path(), WI, &tmp.path().join("absent.toml"));
    assert!(!workflow_path(tmp.path(), WI).exists());
}

#[test]
fn rewinding_a_resumed_state_reruns_from_the_chosen_step() {
    // What the resume path does with the plan before handing it to the
    // engine: everything from the start step on runs again, everything
    // before it that did not finish is skipped.
    let (mut state, dag) = resume_fixture(
        &["a", "b", "c"],
        &[failed(1), StepState::Cancelled, StepState::Cancelled],
    );
    state.rewind_to(&dag, "b");
    assert_eq!(state.status_of("a"), Some(&StepState::Skipped));
    assert_eq!(state.status_of("b"), Some(&StepState::Pending));
    assert_eq!(state.status_of("c"), Some(&StepState::Pending));

    // An unknown start step changes nothing.
    let before = state.clone();
    state.rewind_to(&dag, "zz");
    assert_eq!(
        format!("{:?}", state.step_states),
        format!("{:?}", before.step_states)
    );
}
