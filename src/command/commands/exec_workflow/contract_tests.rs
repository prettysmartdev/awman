//! Decision contracts re-derived for WI 0144, independent of legacy helpers.
use super::prepare::{completed_step_count, workflow_resume_start_points};
use super::{WorkflowResumeDecision, WorkflowResumePrompt};
use crate::data::workflow_dag::WorkflowDag;
use crate::data::workflow_definition::WorkflowStep;
use crate::data::workflow_state::{StepState, WorkflowState};
use proptest::test_runner::{Config, FileFailurePersistence, RngSeed, TestRunner};

fn recover(codes: &[u8]) {
    let steps: Vec<_> = codes
        .iter()
        .enumerate()
        .map(|(index, _)| WorkflowStep {
            name: format!("step-{index}"),
            prompt_template: "literal".into(),
            depends_on: if index == 0 {
                vec![]
            } else {
                vec![format!("step-{}", index - 1)]
            },
            agent: None,
            model: None,
            overlays: None,
            abort_on_failure: false,
        })
        .collect();
    let dag = WorkflowDag::build(&steps).unwrap();
    let mut state = WorkflowState::new("model".into(), &steps, "stable".into(), Some(144));
    for (index, code) in codes.iter().enumerate() {
        let status = match code {
            0 => StepState::Pending,
            1 => StepState::Running { container_id: None },
            2 => StepState::Succeeded,
            3 => StepState::Skipped,
            4 => StepState::Failed {
                exit_code: 17,
                error_message: Some("failed".into()),
            },
            5 => StepState::Cancelled,
            _ => unreachable!(),
        };
        state.set_status(&format!("step-{index}"), status);
    }
    // Model has no calls to resume_stop_point or to rewind_to. A saved run
    // first offers the earliest failed/cancelled step, otherwise unfinished.
    let expected = codes
        .iter()
        .position(|c| *c >= 4)
        .or_else(|| codes.iter().position(|c| *c < 2));
    let done = codes.iter().filter(|c| matches!(c, 2 | 3)).count();
    assert_eq!(completed_step_count(&state), done);
    let bytes = serde_json::to_vec(&state).unwrap();
    let reloaded: WorkflowState = serde_json::from_slice(&bytes).unwrap();
    let points = workflow_resume_start_points(&dag, &reloaded);
    assert_eq!(points.is_empty(), expected.is_none());
    if let Some(index) = expected {
        let mut expected_points = vec![format!("step-{index}")];
        if index > 0 {
            expected_points.push(format!("step-{}", index - 1));
        }
        if index + 1 < codes.len() {
            expected_points.push(format!("step-{}", index + 1));
        }
        assert_eq!(
            points.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
            expected_points
        );
        let prompt = WorkflowResumePrompt::new(
            "model".into(),
            Some(144),
            None,
            false,
            done,
            codes.len(),
            points,
        );
        assert_eq!(
            prompt.resume_from_stop_point(),
            WorkflowResumeDecision::ResumeFrom(format!("step-{index}"))
        );
        let mut resumed = reloaded;
        resumed.rewind_to(&dag, &format!("step-{index}"));
        for (i, old) in codes.iter().enumerate() {
            let expected = if i >= index {
                StepState::Pending
            } else if *old == 2 {
                StepState::Succeeded
            } else {
                StepState::Skipped
            };
            assert_eq!(resumed.status_of(&format!("step-{i}")), Some(&expected));
        }
    }
}

#[test]
fn workflow_recovery_and_resume_planning_follow_the_state_model() {
    let seed = 144_640_115;
    awman_test_support::evidence::note("seed", &seed.to_string());
    let mut runner = TestRunner::new(Config {
        cases: 256,
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/command/commands/exec_workflow/contract_tests.proptest-regressions"
        )))),
        ..Config::default()
    });
    runner
        .run(&proptest::collection::vec(0u8..6, 1..24), |codes| {
            recover(&codes);
            Ok(())
        })
        .unwrap();
}

#[test]
fn minimal_recovery_traces_prioritize_failure_over_unfinished_work() {
    // Minimal independent corpus: failure after pending; failed/cancelled
    // terminal run; complete run; crash with a running step.
    for trace in [&[0, 4][..], &[2, 4, 5][..], &[2, 3][..], &[2, 1, 0][..]] {
        recover(trace);
    }
}

#[test]
fn work_item_numbers_reject_overflow_and_keep_hostile_text_as_data() {
    use crate::command::work_item::parse_work_item_number;
    for (input, expected) in [
        ("WI-0144", Some(144)),
        ("144; codex", Some(144)),
        ("$(codex)", None),
        ("4294967296", None),
        ("0000", Some(0)),
    ] {
        assert_eq!(parse_work_item_number(input), expected);
    }
}

#[test]
fn work_item_lookup_uses_session_scope_without_executing_content() {
    use crate::command::work_item::find_work_item_file;
    use crate::data::session::{Session, SessionOpenOptions};
    use awman_test_support::TestSandbox;
    let s = TestSandbox::named("work_item_lookup");
    let dirs = s.work().join("aspec/work-items");
    std::fs::create_dir_all(&dirs).unwrap();
    let item = dirs.join("0144-hostile.md");
    std::fs::write(&item, "# Task\n$(codex) `claude` ; gemini\n").unwrap();
    let env = crate::data::config::env::EnvSnapshot::from_full_env(
        s.env()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect(),
    );
    let session = Session::open_at_git_root(
        s.work(),
        s.work(),
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        find_work_item_file(&session, &s.work(), 144).unwrap(),
        item.clone()
    );
    assert!(find_work_item_file(&session, &s.work(), 145).is_err());
    assert_eq!(
        std::fs::read_to_string(item).unwrap(),
        "# Task\n$(codex) `claude` ; gemini\n"
    );
    s.finish();
}
