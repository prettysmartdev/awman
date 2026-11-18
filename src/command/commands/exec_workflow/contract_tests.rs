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
            read_only_workspace: false,
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
fn resume_rewind_follows_dependencies_in_any_definition_order() {
    let steps: Vec<_> = [
        ("plan-a", vec![]),
        ("work-a", vec!["plan-a"]),
        ("plan-b", vec![]),
        ("work-b", vec!["plan-b"]),
        ("join", vec!["work-a", "work-b"]),
        ("finish", vec!["join"]),
        ("independent", vec![]),
    ]
    .into_iter()
    .map(|(name, deps)| WorkflowStep {
        name: name.into(),
        depends_on: deps.into_iter().map(str::to_string).collect(),
        prompt_template: "literal".into(),
        agent: None,
        model: None,
        overlays: None,
        abort_on_failure: false,
        read_only_workspace: false,
    })
    .collect();
    for reverse in [false, true] {
        for rotation in 0..steps.len() {
            let mut steps = steps.clone();
            if reverse {
                steps.reverse();
            }
            steps.rotate_left(rotation);
            let dag = WorkflowDag::build(&steps).unwrap();
            let mut saved = WorkflowState::new("branches".into(), &steps, "stable".into(), None);
            for step in &steps {
                saved.set_status(&step.name, StepState::Succeeded);
            }
            saved.set_status(
                "work-a",
                StepState::Failed {
                    exit_code: 1,
                    error_message: None,
                },
            );
            saved.set_status("work-b", StepState::Skipped);
            saved.set_status("independent", StepState::Pending);

            for start in ["plan-a", "work-a", "join"] {
                let mut resumed = saved.clone();
                resumed.rewind_to(&dag, start);
                let expected_a = match start {
                    "join" => StepState::Skipped,
                    _ => StepState::Pending,
                };
                assert_eq!(resumed.status_of("work-a"), Some(&expected_a));
                assert_eq!(
                    resumed.status_of("plan-a"),
                    Some(&if start == "plan-a" {
                        StepState::Pending
                    } else {
                        StepState::Succeeded
                    })
                );
                // A join downstream of the chosen step must be invalidated,
                // even if it had completed; its other branch must be preserved.
                assert_eq!(resumed.status_of("join"), Some(&StepState::Pending));
                assert_eq!(resumed.status_of("finish"), Some(&StepState::Pending));
                assert_eq!(resumed.status_of("plan-b"), Some(&StepState::Succeeded));
                assert_eq!(resumed.status_of("work-b"), Some(&StepState::Skipped));
                assert_eq!(resumed.status_of("independent"), Some(&StepState::Pending));
                let ready: std::collections::HashSet<_> =
                    resumed.next_ready(&dag).into_iter().collect();
                assert_eq!(
                    ready,
                    [start.to_string(), "independent".into()]
                        .into_iter()
                        .collect()
                );
                for (name, status) in &resumed.step_states {
                    assert_eq!(
                        resumed.completed_steps.contains(name),
                        matches!(status, StepState::Succeeded | StepState::Skipped)
                    );
                }
            }
            let original = saved.clone();
            saved.rewind_to(&dag, "unknown");
            assert_eq!(saved, original);
        }
    }
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
fn generated_prerequisites_preserve_declines_and_cannot_add_grants() {
    use crate::data::workflow_prerequisites::*;
    let token = WorkflowPrerequisite::GitHubToken {
        host: "github.com".into(),
    };
    let identity = WorkflowPrerequisite::GitIdentity;
    let report = WorkflowPrerequisiteReport {
        phase: PrerequisitePhase::Start,
        origin: PrerequisiteOrigin::AssumedAll,
        checks: vec![
            PrerequisiteCheck {
                prerequisite: token.clone(),
                status: PrerequisiteStatus::Missing,
                explanation: "declined".into(),
                needed_by: vec![],
                remediations: vec![],
            },
            PrerequisiteCheck {
                prerequisite: identity.clone(),
                status: PrerequisiteStatus::Satisfied,
                explanation: "approved".into(),
                needed_by: vec![],
                remediations: vec![],
            },
        ],
        advisories: vec![],
        notes: vec![],
    };
    let unreviewed = WorkflowPrerequisite::PushAuthSsh {
        remote: "another".into(),
        host: "example.org".into(),
    };
    let needs = WorkflowPrerequisites {
        origin: PrerequisiteOrigin::Static,
        needs: vec![
            PrerequisiteNeed {
                prerequisite: token.clone(),
                needed_by: vec![],
            },
            PrerequisiteNeed {
                prerequisite: unreviewed.clone(),
                needed_by: vec![],
            },
        ],
        advisories: vec![],
    };
    let narrowed = report.narrow_to(&needs);
    assert_eq!(narrowed.find(&token).unwrap().explanation, "declined");
    assert_eq!(
        narrowed.find(&token).unwrap().status,
        PrerequisiteStatus::Missing
    );
    assert_eq!(
        narrowed.find(&unreviewed).unwrap().status,
        PrerequisiteStatus::Missing
    );
    assert!(
        narrowed.find(&identity).is_none(),
        "unused grant must not remain in the report"
    );
}

#[test]
fn prerequisite_review_describes_the_whole_workflow_and_image_preparation() {
    use crate::command::commands::workflow_prerequisites::PrerequisiteReportNext;
    use crate::data::workflow_prerequisites::*;
    let mut report = WorkflowPrerequisiteReport {
        phase: PrerequisitePhase::Start,
        origin: PrerequisiteOrigin::AssumedAll,
        checks: vec![PrerequisiteCheck {
            prerequisite: WorkflowPrerequisite::AgentImage {
                agent: "codex".into(),
                tag: "codex:latest".into(),
            },
            status: PrerequisiteStatus::Missing,
            explanation: "missing image".into(),
            needed_by: vec![],
            remediations: vec![],
        }],
        advisories: vec![],
        notes: vec![],
    };
    let next = PrerequisiteReportNext::Questions {
        unresolved: crate::data::config::workflows::UnresolvedMode::Warn,
    };
    let dynamic = crate::command::prompts::prerequisite_report(&report, next);
    assert!(dynamic.body.contains("all workflow prerequisites"));
    assert!(dynamic
        .body
        .contains("prepares the listed missing or stale images"));
    assert!(dynamic
        .body
        .contains("failed refresh warns and keeps the installed image"));
    assert!(!dynamic.body.contains("teardown"));
    report.origin = PrerequisiteOrigin::Static;
    let concrete = crate::command::prompts::prerequisite_report(&report, next);
    assert!(concrete.body.contains("calculated for this workflow"));
}
