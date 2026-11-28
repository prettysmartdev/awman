//! Pure contracts for the task-owned image context and the legacy migration
//! plan (WI 0105). Private sandbox directories only.
use super::*;
use awman_test_support::{TestCase, TestSandbox};

fn sandbox(name: &'static str) -> TestSandbox {
    TestSandbox::for_test(TestCase::component(name))
}

fn paths(s: &TestSandbox) -> SquadPaths {
    SquadPaths::from_root(s.root().join("squad"))
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn agents(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| n.to_string()).collect()
}

#[test]
fn context_is_the_guarded_task_folder_and_unsafe_names_are_refused() {
    let s = sandbox("data::fs::squad_paths::image_context::image_context_tests::names");
    let paths = paths(&s);
    let context = paths.task_image_context("alpha").unwrap();
    assert_eq!(context.root(), paths.task_dir("alpha").unwrap());
    assert_eq!(
        context.state_file(),
        paths.tasks_dir().join("alpha").join(TASK_IMAGE_STATE_FILE)
    );
    for bad in ["", ".", "..", "../escape", "a/b", "/abs"] {
        assert!(paths.task_image_context(bad).is_err(), "{bad:?}");
    }
}

#[cfg(unix)]
#[test]
fn symlinked_workspace_awman_and_recipes_are_refused() {
    use std::os::unix::fs::symlink;
    let s = sandbox("data::fs::squad_paths::image_context::image_context_tests::links");
    let paths = paths(&s);
    let elsewhere = s.root().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();

    // workspace itself a link.
    let task = paths.tasks_dir().join("linked");
    std::fs::create_dir_all(&task).unwrap();
    symlink(&elsewhere, task.join("workspace")).unwrap();
    assert!(paths.task_image_context("linked").is_err());

    // .awman a link.
    let context = paths.task_image_context("dot").unwrap();
    symlink(&elsewhere, context.root().join(".awman")).unwrap();
    assert!(paths.task_image_context("dot").is_err());
    assert!(context.verify_recipes(&agents(&["claude"])).is_err());

    // Live and dangling Dockerfile.dev links are refused, never written through.
    for (name, target) in [
        ("live", elsewhere.join("real")),
        ("dangling", elsewhere.join("missing")),
    ] {
        write(&elsewhere.join("real"), "FROM real\n");
        let context = paths.task_image_context(name).unwrap();
        symlink(&target, context.root().join(PROJECT_RECIPE)).unwrap();
        assert!(context.ensure_project_recipe("FROM template\n").is_err());
        assert!(context.verify_recipes(&[]).is_err());
        assert!(!elsewhere.join("missing").exists());
        assert_eq!(
            std::fs::read_to_string(elsewhere.join("real")).unwrap(),
            "FROM real\n"
        );
    }
    s.finish();
}

#[test]
fn project_recipe_is_create_if_missing_and_keeps_edits() {
    let s = sandbox("data::fs::squad_paths::image_context::image_context_tests::scaffold");
    let context = paths(&s).task_image_context("t").unwrap();
    assert_eq!(
        context.ensure_project_recipe("FROM template\n").unwrap(),
        CreateOutcome::Created
    );
    write(&context.root().join(PROJECT_RECIPE), "FROM edited\n");
    assert_eq!(
        context.ensure_project_recipe("FROM template\n").unwrap(),
        CreateOutcome::Existing
    );
    assert_eq!(
        std::fs::read_to_string(context.root().join(PROJECT_RECIPE)).unwrap(),
        "FROM edited\n"
    );
    s.finish();
}

#[test]
fn migration_state_round_trips() {
    let s = sandbox("data::fs::squad_paths::image_context::image_context_tests::state");
    let context = paths(&s).task_image_context("t").unwrap();
    assert_eq!(context.load_state().unwrap(), None);
    context.record_state(Path::new("/custom")).unwrap();
    let state = context.load_state().unwrap().unwrap();
    assert!(state.legacy_checked);
    assert_eq!(state.from, Path::new("/custom"));
    s.finish();
}

#[test]
fn legacy_plan_nothing_recipes_only_and_explicit_copies() {
    let s = sandbox("data::fs::squad_paths::image_context::image_context_tests::plan_copy");
    let context = paths(&s).task_image_context("t").unwrap();
    let legacy = s.work().join("custom");
    std::fs::create_dir_all(&legacy).unwrap();
    assert_eq!(
        context
            .plan_legacy_migration(&legacy, &agents(&["claude"]))
            .unwrap(),
        LegacyRecipePlan::Nothing
    );
    // The context itself is never its own legacy source.
    assert_eq!(
        context.plan_legacy_migration(context.root(), &[]).unwrap(),
        LegacyRecipePlan::Nothing
    );

    write(
        &legacy.join(".awman/Dockerfile.claude"),
        "FROM awman-custom\n",
    );
    write(
        &legacy.join(PROJECT_RECIPE),
        "FROM scratch\nCOPY tools/setup.sh /setup.sh\nCOPY conf /etc/conf\nADD https://example.invalid/x /x\nCOPY --from=builder /a /a\n",
    );
    write(&legacy.join("tools/setup.sh"), "#!/bin/sh\n");
    write(&legacy.join("conf/a.toml"), "a");
    write(&legacy.join("conf/nested/b.toml"), "b");
    write(&legacy.join(".dockerignore"), "target\n");
    write(&legacy.join("unrelated.txt"), "not copied");
    let before = snapshot(&legacy);
    let plan = context
        .plan_legacy_migration(&legacy, &agents(&["claude"]))
        .unwrap();
    let LegacyRecipePlan::Copy { files, kept } = &plan else {
        panic!("{plan:?}")
    };
    let rels: Vec<_> = files.iter().map(|f| f.rel.as_str()).collect();
    assert_eq!(
        rels,
        [
            "Dockerfile.dev",
            ".dockerignore",
            "tools/setup.sh",
            "conf",
            "conf/a.toml",
            "conf/nested",
            "conf/nested/b.toml",
            ".awman/Dockerfile.claude"
        ]
    );
    assert!(kept.is_empty());
    // Planning is side-effect free.
    assert!(!context.root().join(PROJECT_RECIPE).exists());

    context.apply_legacy_migration(&legacy, &plan).unwrap();
    for rel in rels.into_iter().filter(|rel| legacy.join(rel).is_file()) {
        assert_eq!(
            std::fs::read(context.root().join(rel)).unwrap(),
            std::fs::read(legacy.join(rel)).unwrap(),
            "{rel}"
        );
    }
    assert!(!context.root().join("unrelated.txt").exists());
    assert_eq!(snapshot(&legacy), before, "the custom folder is only read");
    // A second apply is a no-op, and a re-plan copies nothing new.
    context.apply_legacy_migration(&legacy, &plan).unwrap();
    let again = context
        .plan_legacy_migration(&legacy, &agents(&["claude"]))
        .unwrap();
    let LegacyRecipePlan::Copy { files, kept } = again else {
        panic!()
    };
    assert!(files.is_empty());
    assert_eq!(kept, ["Dockerfile.dev", ".awman/Dockerfile.claude"]);
    s.finish();
}

#[test]
fn task_owned_recipe_wins_over_the_legacy_one() {
    let s = sandbox("data::fs::squad_paths::image_context::image_context_tests::plan_kept");
    let context = paths(&s).task_image_context("t").unwrap();
    let legacy = s.work().join("custom");
    write(&legacy.join(PROJECT_RECIPE), "FROM legacy\nCOPY . /src\n");
    write(&context.root().join(PROJECT_RECIPE), "FROM owned\n");
    // The task-owned file wins, so even an unmovable legacy recipe is fine.
    assert_eq!(
        context.plan_legacy_migration(&legacy, &[]).unwrap(),
        LegacyRecipePlan::Copy {
            files: vec![],
            kept: vec!["Dockerfile.dev".into()]
        }
    );
    s.finish();
}

#[test]
fn every_refusal_reason_changes_nothing() {
    let s = sandbox("data::fs::squad_paths::image_context::image_context_tests::refusals");
    let paths = paths(&s);
    let cases: Vec<(&str, &str, LegacyRefusal)> = vec![
        (
            "whole",
            "FROM x\nCOPY . /src\n",
            LegacyRefusal::WholeContext,
        ),
        (
            "glob",
            "FROM x\nCOPY *.sh /bin/\n",
            LegacyRefusal::Unresolvable,
        ),
        (
            "var",
            "FROM x\nCOPY $SRC /bin/\n",
            LegacyRefusal::Unresolvable,
        ),
        (
            "escape",
            "FROM x\nCOPY ../up /up\n",
            LegacyRefusal::EscapesDirectory,
        ),
        (
            "differs",
            "FROM x\nCOPY input.txt /i\n",
            LegacyRefusal::DestinationDiffers,
        ),
    ];
    for (name, recipe, why) in cases {
        let context = paths.task_image_context(name).unwrap();
        let legacy = s.work().join(name);
        write(&legacy.join(PROJECT_RECIPE), recipe);
        write(&legacy.join("input.txt"), "legacy");
        if why == LegacyRefusal::DestinationDiffers {
            write(&context.root().join("input.txt"), "task-owned");
        }
        let plan = context.plan_legacy_migration(&legacy, &[]).unwrap();
        assert!(
            matches!(&plan, LegacyRecipePlan::Refuse { why: got, recipe, .. } if *got == why && recipe == "Dockerfile.dev"),
            "{name}: {plan:?}"
        );
        let message = plan.refusal_message(name, &legacy, context.root()).unwrap();
        assert!(message.contains("Nothing was changed"), "{message}");
        assert!(message.contains(&context.root().display().to_string()));
        context.apply_legacy_migration(&legacy, &plan).unwrap();
        assert!(!context.root().join(PROJECT_RECIPE).exists(), "{name}");
    }

    // Over the file limit.
    let context = paths.task_image_context("large").unwrap();
    let legacy = s.work().join("large");
    write(&legacy.join(PROJECT_RECIPE), "FROM x\nCOPY many /many\n");
    for i in 0..=MAX_LEGACY_INPUT_FILES {
        write(&legacy.join(format!("many/{i}")), "");
    }
    assert!(matches!(
        context.plan_legacy_migration(&legacy, &[]).unwrap(),
        LegacyRecipePlan::Refuse {
            why: LegacyRefusal::TooLarge,
            ..
        }
    ));
    s.finish();
}

#[cfg(unix)]
#[test]
fn symlinked_legacy_recipes_and_inputs_are_refused() {
    use std::os::unix::fs::symlink;
    let s = sandbox("data::fs::squad_paths::image_context::image_context_tests::legacy_links");
    let paths = paths(&s);
    let outside = s.root().join("outside.txt");
    write(&outside, "secret");

    let context = paths.task_image_context("recipe").unwrap();
    let legacy = s.work().join("recipe");
    std::fs::create_dir_all(&legacy).unwrap();
    symlink(&outside, legacy.join(PROJECT_RECIPE)).unwrap();
    assert!(matches!(
        context.plan_legacy_migration(&legacy, &[]).unwrap(),
        LegacyRecipePlan::Refuse {
            why: LegacyRefusal::Symlink,
            ..
        }
    ));

    let context = paths.task_image_context("input").unwrap();
    let legacy = s.work().join("input");
    write(&legacy.join(PROJECT_RECIPE), "FROM x\nCOPY dir /d\n");
    std::fs::create_dir_all(legacy.join("dir")).unwrap();
    symlink(&outside, legacy.join("dir/link")).unwrap();
    assert!(matches!(
        context.plan_legacy_migration(&legacy, &[]).unwrap(),
        LegacyRecipePlan::Refuse {
            why: LegacyRefusal::Symlink,
            ..
        }
    ));
    s.finish();
}

fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
    }
    out.sort();
    out
}

fn copy_plan(context: &TaskImageContext, legacy: &Path) -> LegacyRecipePlan {
    let plan = context.plan_legacy_migration(legacy, &agents(&[])).unwrap();
    assert!(matches!(plan, LegacyRecipePlan::Copy { .. }), "{plan:?}");
    plan
}

#[test]
fn an_interrupted_migration_is_planned_again_and_completes_on_retry() {
    let s = sandbox("data::fs::squad_paths::image_context::image_context_tests::interrupted");
    let context = paths(&s).task_image_context("t").unwrap();
    let legacy = s.work().join("custom");
    write(
        &legacy.join(PROJECT_RECIPE),
        "FROM scratch\nCOPY tool /tool\n",
    );
    write(&legacy.join("tool"), "tool v1");
    let plan = copy_plan(&context, &legacy);
    // The input disappears between plan and apply: apply fails, and no
    // recipe was published ahead of its inputs.
    std::fs::rename(legacy.join("tool"), s.work().join("tool.away")).unwrap();
    assert!(context.apply_legacy_migration(&legacy, &plan).is_err());
    assert!(!context.root().join(PROJECT_RECIPE).exists());
    // Restored, the retry plans the recipe and its input again.
    std::fs::rename(s.work().join("tool.away"), legacy.join("tool")).unwrap();
    let retry = copy_plan(&context, &legacy);
    context.apply_legacy_migration(&legacy, &retry).unwrap();
    assert_eq!(
        std::fs::read(context.root().join("tool")).unwrap(),
        b"tool v1"
    );
    assert!(context.root().join(PROJECT_RECIPE).is_file());
    s.finish();
}

#[test]
fn apply_installs_only_planned_bytes_and_refuses_a_competing_destination() {
    let s = sandbox("data::fs::squad_paths::image_context::image_context_tests::bound_plan");
    let context = paths(&s).task_image_context("t").unwrap();
    let legacy = s.work().join("custom");
    write(&legacy.join(PROJECT_RECIPE), "FROM scratch\n");
    let plan = copy_plan(&context, &legacy);
    // The recipe gains an unplanned input after planning.
    write(
        &legacy.join(PROJECT_RECIPE),
        "FROM scratch\nCOPY secret /secret\n",
    );
    write(&legacy.join("secret"), "s");
    assert!(context.apply_legacy_migration(&legacy, &plan).is_err());
    assert!(!context.root().join(PROJECT_RECIPE).exists());

    // A different task-owned input appears after planning.
    let context = paths(&s).task_image_context("u").unwrap();
    let legacy = s.work().join("custom-u");
    write(
        &legacy.join(PROJECT_RECIPE),
        "FROM scratch\nCOPY tool /tool\n",
    );
    write(&legacy.join("tool"), "legacy tool");
    let plan = copy_plan(&context, &legacy);
    write(&context.root().join("tool"), "someone else's tool");
    let error = context
        .apply_legacy_migration(&legacy, &plan)
        .unwrap_err()
        .to_string();
    assert!(error.contains("differs"), "{error}");
    assert!(!context.root().join(PROJECT_RECIPE).exists());
    assert_eq!(
        std::fs::read(context.root().join("tool")).unwrap(),
        b"someone else's tool"
    );
    s.finish();
}

#[test]
fn copied_directories_keep_empty_directories_and_git_metadata() {
    let s = sandbox("data::fs::squad_paths::image_context::image_context_tests::directories");
    let context = paths(&s).task_image_context("t").unwrap();
    let legacy = s.work().join("custom");
    write(
        &legacy.join(PROJECT_RECIPE),
        "FROM scratch\nCOPY empty /empty\nCOPY bundle /bundle\n",
    );
    std::fs::create_dir_all(legacy.join("empty")).unwrap();
    write(&legacy.join("bundle/file"), "f");
    write(&legacy.join("bundle/.git/HEAD"), "ref: refs/heads/main\n");
    let plan = copy_plan(&context, &legacy);
    context.apply_legacy_migration(&legacy, &plan).unwrap();
    assert!(context.root().join("empty").is_dir());
    assert_eq!(
        std::fs::read(context.root().join("bundle/.git/HEAD")).unwrap(),
        b"ref: refs/heads/main\n"
    );
    assert_eq!(
        std::fs::read(context.root().join("bundle/file")).unwrap(),
        b"f"
    );
    s.finish();
}
