//! Resolution, ambiguity, scope boundaries and symlink handling for
//! `edit workflow|skill|agent` targets. Everything lives in a private
//! sandbox; the developer's HOME is never consulted.

use super::*;
use awman_test_support::{TestCase, TestSandbox};

pub(crate) struct Fx {
    pub(crate) _sandbox: TestSandbox,
    pub(crate) repo: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) env: EnvSnapshot,
}

pub(crate) fn fx(name: &str) -> Fx {
    fx_in("data::fs::edit_target::tests", name)
}

pub(crate) fn fx_in(module: &str, name: &str) -> Fx {
    let sandbox = TestSandbox::for_test(TestCase::component(Box::leak(
        format!("{module}::{name}").into_boxed_str(),
    )));
    let repo = sandbox.work().join("my repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let home = sandbox.awman_home();
    std::fs::create_dir_all(&home).unwrap();
    let env = EnvSnapshot::with_overrides([
        ("AWMAN_HOME", home.display().to_string()),
        ("HOME", sandbox.home().display().to_string()),
    ]);
    let repo = std::fs::canonicalize(repo).unwrap();
    let home = std::fs::canonicalize(home).unwrap();
    Fx {
        _sandbox: sandbox,
        repo,
        home,
        env,
    }
}

pub(crate) fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

pub(crate) const WORKFLOW_TOML: &str = r#"
[[steps]]
name = "build"
prompt = "build it"
"#;

pub(crate) const SKILL_MD: &str = "---\nname: demo\ndescription: a demo\n---\n# Demo\n";

pub(crate) const AGENT_TOML: &str = r#"schema = 1
display_name = "Demo"
[system_prompt]
delivery = "none"
[run]
command = ["demo"]
"#;

fn resolve(
    fx: &Fx,
    kind: ArtifactKind,
    reference: &str,
    global: bool,
) -> Result<ArtifactTarget, EditTargetError> {
    resolve_in(fx, kind, reference, global, &fx.repo)
}

fn resolve_in(
    fx: &Fx,
    kind: ArtifactKind,
    reference: &str,
    global: bool,
    cwd: &Path,
) -> Result<ArtifactTarget, EditTargetError> {
    resolve_artifact_target(&ArtifactRequest {
        kind,
        reference,
        global,
        env: &fx.env,
        git_root: &fx.repo,
        definition_root: &fx.repo,
        cwd,
    })
}

// ── workflows ──

#[test]
fn repo_workflow_name_resolves_in_place_and_never_falls_back_to_global() {
    let fx = fx("repo_workflow_name_resolves_in_place_and_never_falls_back_to_global");
    write(&fx.repo.join("aspec/workflows/release.toml"), WORKFLOW_TOML);
    let target = resolve(&fx, ArtifactKind::Workflow, "release", false).unwrap();
    assert_eq!(target.name, "release");
    assert_eq!(target.scope, ArtifactScope::Repo);
    assert_eq!(target.root, fx.repo.join("aspec/workflows"));
    assert_eq!(target.format, Some(WorkflowFormat::Toml));
    assert_eq!(target.workspace_rel.as_deref(), Some("aspec/workflows"));
    assert_eq!(
        target.workspace_container_path(target.primary()).as_deref(),
        Some("/workspace/aspec/workflows/release.toml")
    );
    assert!(
        target.initial_problems.is_empty(),
        "{:?}",
        target.initial_problems
    );

    // A global-only name is a miss in repository scope, with a hint.
    write(&fx.home.join("workflows/nightly.yaml"), "steps: []\n");
    let err = resolve(&fx, ArtifactKind::Workflow, "nightly", false).unwrap_err();
    assert!(
        matches!(&err, EditTargetError::NotFound { hint: Some(h), .. } if h.contains("--global")),
        "{err}"
    );
    let global = resolve(&fx, ArtifactKind::Workflow, "nightly", true).unwrap();
    assert_eq!(global.scope, ArtifactScope::Global);
    assert!(!global.edits_in_place());
    assert_eq!(global.format, Some(WorkflowFormat::Yaml));
}

#[test]
fn workflow_with_two_extensions_is_ambiguous_and_lists_both_paths() {
    let fx = fx("workflow_with_two_extensions_is_ambiguous_and_lists_both_paths");
    write(
        &fx.repo.join(".awman/workflows/release.toml"),
        WORKFLOW_TOML,
    );
    write(
        &fx.repo.join(".awman/workflows/release.yaml"),
        "steps: []\n",
    );
    let err = resolve(&fx, ArtifactKind::Workflow, "release", false).unwrap_err();
    let EditTargetError::Ambiguous { candidates, .. } = &err else {
        panic!("{err}")
    };
    assert_eq!(candidates.len(), 2);
    assert!(err.to_string().contains("release.yaml"));
    // Picking by path resolves it.
    let target = resolve(
        &fx,
        ArtifactKind::Workflow,
        ".awman/workflows/release.yaml",
        false,
    )
    .unwrap();
    assert_eq!(target.format, Some(WorkflowFormat::Yaml));
}

#[test]
fn dynamic_stash_files_are_not_workflow_names_and_missing_names_create_nothing() {
    let fx = fx("dynamic_stash_files_are_not_workflow_names_and_missing_names_create_nothing");
    write(
        &fx.repo.join(".awman/workflows/dynamic-0001.toml"),
        WORKFLOW_TOML,
    );
    let err = resolve(&fx, ArtifactKind::Workflow, "dynamic-0001", false).unwrap_err();
    assert!(matches!(err, EditTargetError::NotFound { .. }));
    let err = resolve(&fx, ArtifactKind::Workflow, "absent", true).unwrap_err();
    assert!(matches!(err, EditTargetError::NotFound { .. }));
    assert!(
        !fx.home.join("workflows").exists(),
        "resolution must not create directories"
    );
}

#[test]
fn repo_path_with_global_is_a_scope_conflict_and_global_path_implies_global() {
    let fx = fx("repo_path_with_global_is_a_scope_conflict_and_global_path_implies_global");
    write(&fx.repo.join("flows/a b.toml"), WORKFLOW_TOML);
    let err = resolve(&fx, ArtifactKind::Workflow, "flows/a b.toml", true).unwrap_err();
    assert!(
        matches!(err, EditTargetError::ScopeConflict { .. }),
        "{err}"
    );
    let ok = resolve(&fx, ArtifactKind::Workflow, "flows/a b.toml", false).unwrap();
    assert_eq!(ok.name, "a b");

    let global = fx.home.join("workflows/g.toml");
    write(&global, WORKFLOW_TOML);
    let target = resolve(&fx, ArtifactKind::Workflow, global.to_str().unwrap(), false).unwrap();
    assert_eq!(target.scope, ArtifactScope::Global);
}

#[test]
fn paths_outside_every_root_and_traversal_escapes_are_rejected() {
    let fx = fx("paths_outside_every_root_and_traversal_escapes_are_rejected");
    let outside = fx.repo.parent().unwrap().join("outside.toml");
    write(&outside, WORKFLOW_TOML);
    let err = resolve(&fx, ArtifactKind::Workflow, "../outside.toml", false).unwrap_err();
    assert!(matches!(err, EditTargetError::OutsideScope { .. }), "{err}");
    let err = resolve(
        &fx,
        ArtifactKind::Workflow,
        "../my repo/../outside.toml",
        false,
    )
    .unwrap_err();
    assert!(matches!(err, EditTargetError::OutsideScope { .. }), "{err}");
    let err = resolve(&fx, ArtifactKind::Workflow, "..", false).unwrap_err();
    assert!(
        matches!(
            err,
            EditTargetError::InvalidReference { .. } | EditTargetError::OutsideScope { .. }
        ),
        "{err}"
    );
}

#[cfg(unix)]
#[test]
fn symlinked_files_and_directories_below_a_root_are_unsafe() {
    let fx = fx("symlinked_files_and_directories_below_a_root_are_unsafe");
    let outside = fx.repo.parent().unwrap().join("elsewhere");
    write(&outside.join("x.toml"), WORKFLOW_TOML);
    std::os::unix::fs::symlink(outside.join("x.toml"), fx.repo.join("link.toml")).unwrap();
    std::os::unix::fs::symlink(&outside, fx.repo.join("linkdir")).unwrap();
    let err = resolve(&fx, ArtifactKind::Workflow, "link.toml", false).unwrap_err();
    assert!(matches!(err, EditTargetError::Unsafe { .. }), "{err}");
    let err = resolve(&fx, ArtifactKind::Workflow, "linkdir/x.toml", false).unwrap_err();
    assert!(matches!(err, EditTargetError::Unsafe { .. }), "{err}");
    // A symlink that stays inside the repo is refused just the same.
    write(&fx.repo.join("real/y.toml"), WORKFLOW_TOML);
    std::os::unix::fs::symlink(fx.repo.join("real"), fx.repo.join("alias")).unwrap();
    let err = resolve(&fx, ArtifactKind::Workflow, "alias/y.toml", false).unwrap_err();
    assert!(matches!(err, EditTargetError::Unsafe { .. }), "{err}");
    // Name lookup goes through the same checks.
    std::fs::create_dir_all(fx.repo.join("aspec/workflows")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("x.toml"),
        fx.repo.join("aspec/workflows/sneaky.toml"),
    )
    .unwrap();
    let err = resolve(&fx, ArtifactKind::Workflow, "sneaky", false).unwrap_err();
    assert!(matches!(err, EditTargetError::Unsafe { .. }), "{err}");
}

#[test]
fn invalid_workflow_opens_for_repair_with_its_problems() {
    let fx = fx("invalid_workflow_opens_for_repair_with_its_problems");
    write(
        &fx.repo.join("aspec/workflows/broken.toml"),
        "this is = = not toml",
    );
    let target = resolve(&fx, ArtifactKind::Workflow, "broken", false).unwrap();
    assert!(!target.initial_problems.is_empty());
}

// ── skills ──

#[test]
fn skill_resolves_by_name_directory_or_skill_md_and_bundles_its_files() {
    let fx = fx("skill_resolves_by_name_directory_or_skill_md_and_bundles_its_files");
    let dir = fx.repo.join(".claude/skills/demo");
    write(&dir.join("SKILL.md"), SKILL_MD);
    write(&dir.join("refs/notes.md"), "notes");
    write(&fx.repo.join(".claude/skills/other/SKILL.md"), SKILL_MD);
    let by_name = resolve(&fx, ArtifactKind::Skill, "demo", false).unwrap();
    assert!(by_name.directory_bundle);
    assert_eq!(by_name.root, dir);
    let rels: Vec<_> = by_name
        .files
        .iter()
        .map(|f| (f.rel.as_str(), f.role))
        .collect();
    assert_eq!(
        rels,
        [
            ("SKILL.md", ArtifactFileRole::Primary),
            ("refs/notes.md", ArtifactFileRole::Supporting)
        ]
    );
    // Explicit in-repo paths outside .claude/skills work too.
    write(
        &fx.repo.join(".agents/skills/release prep/SKILL.md"),
        SKILL_MD,
    );
    for reference in [
        ".agents/skills/release prep",
        ".agents/skills/release prep/SKILL.md",
    ] {
        let t = resolve(&fx, ArtifactKind::Skill, reference, false).unwrap();
        assert_eq!(t.name, "release prep");
        assert_eq!(
            t.workspace_rel.as_deref(),
            Some(".agents/skills/release prep")
        );
    }
    // A directory without SKILL.md, or the repo itself, is not a skill.
    std::fs::create_dir_all(fx.repo.join("docs")).unwrap();
    assert!(matches!(
        resolve(&fx, ArtifactKind::Skill, "docs", false).unwrap_err(),
        EditTargetError::NotFound { .. }
    ));
    assert!(matches!(
        resolve(&fx, ArtifactKind::Skill, "./docs", false).unwrap_err(),
        EditTargetError::InvalidReference { .. }
    ));
}

#[cfg(unix)]
#[test]
fn skill_bundle_skips_symlinks_with_a_notice_and_enforces_limits() {
    let fx = fx("skill_bundle_skips_symlinks_with_a_notice_and_enforces_limits");
    let dir = fx.repo.join(".claude/skills/demo");
    write(&dir.join("SKILL.md"), SKILL_MD);
    let secret = fx.repo.parent().unwrap().join("secret");
    write(&secret, "do not read");
    std::os::unix::fs::symlink(&secret, dir.join("leak.md")).unwrap();
    let target = resolve(&fx, ArtifactKind::Skill, "demo", false).unwrap();
    assert_eq!(target.files.len(), 1);
    assert!(
        target.notices.iter().any(|n| n.contains("leak.md")),
        "{:?}",
        target.notices
    );

    let big = fx.repo.join(".claude/skills/big");
    write(&big.join("SKILL.md"), SKILL_MD);
    for i in 0..=MAX_BUNDLE_FILES {
        write(&big.join(format!("f{i}.txt")), "x");
    }
    assert!(matches!(
        resolve(&fx, ArtifactKind::Skill, "big", false).unwrap_err(),
        EditTargetError::TooLarge { .. }
    ));
}

#[test]
fn global_and_library_skills_resolve_with_scope_and_refresh_notice() {
    let fx = fx("global_and_library_skills_resolve_with_scope_and_refresh_notice");
    write(&fx.home.join("skills/mine/SKILL.md"), SKILL_MD);
    let lib = fx.home.join("skills/.library/superpowers");
    write(
        &lib.join(".awman.json"),
        r#"{"source":"github.com/o/superpowers","owner":"o","repo":"superpowers","subdir":"skills"}"#,
    );
    write(&lib.join("skills/brainstorm/SKILL.md"), SKILL_MD);
    write(&lib.join("README.md"), "not in the bundle");

    let mine = resolve(&fx, ArtifactKind::Skill, "mine", true).unwrap();
    assert_eq!(mine.scope, ArtifactScope::Global);
    assert!(!mine.edits_in_place());
    // Repository scope never falls back to the global skill.
    let err = resolve(&fx, ArtifactKind::Skill, "mine", false).unwrap_err();
    assert!(
        matches!(&err, EditTargetError::NotFound { hint: Some(_), .. }),
        "{err}"
    );

    let skill = resolve(&fx, ArtifactKind::Skill, "superpowers/brainstorm", true).unwrap();
    assert_eq!(
        skill.scope,
        ArtifactScope::Library {
            slug: "superpowers".into()
        }
    );
    assert_eq!(skill.name, "superpowers/brainstorm");
    assert_eq!(
        skill.files.len(),
        1,
        "siblings and metadata stay out of the bundle"
    );
    assert!(skill.notices.iter().any(|n| n.contains("--pull")));
    // The same skill by explicit path, without --global.
    let by_path = resolve(
        &fx,
        ArtifactKind::Skill,
        lib.join("skills/brainstorm/SKILL.md").to_str().unwrap(),
        false,
    )
    .unwrap();
    assert_eq!(by_path.scope, skill.scope);

    // A bare library name is not one skill.
    let err = resolve(&fx, ArtifactKind::Skill, "superpowers", true).unwrap_err();
    assert!(
        matches!(err, EditTargetError::InvalidReference { .. }),
        "{err}"
    );
    // Library content outside <subdir>/<skill> is out of scope.
    write(&lib.join("other/x/SKILL.md"), SKILL_MD);
    let err = resolve(
        &fx,
        ArtifactKind::Skill,
        lib.join("other/x").to_str().unwrap(),
        true,
    )
    .unwrap_err();
    assert!(matches!(err, EditTargetError::OutsideScope { .. }), "{err}");
}

// ── agents ──

#[test]
fn agent_name_resolves_with_explicit_dockerfile_companion_only() {
    let fx = fx("agent_name_resolves_with_explicit_dockerfile_companion_only");
    let dir = fx.repo.join(".awman/agents");
    write(
        &dir.join("maki.toml"),
        &format!("{AGENT_TOML}[image]\ndockerfile = \"maki.Dockerfile\"\n"),
    );
    write(&dir.join("maki.Dockerfile"), "FROM scratch\n");
    write(&dir.join("unrelated.Dockerfile"), "FROM scratch\n");
    let target = resolve(&fx, ArtifactKind::Agent, "maki", false).unwrap();
    let rels: Vec<_> = target
        .files
        .iter()
        .map(|f| (f.rel.as_str(), f.role))
        .collect();
    assert_eq!(
        rels,
        [
            ("maki.toml", ArtifactFileRole::Primary),
            ("maki.Dockerfile", ArtifactFileRole::Companion)
        ]
    );
    assert_eq!(target.workspace_rel.as_deref(), Some(".awman/agents"));
    let by_path = resolve(&fx, ArtifactKind::Agent, ".awman/agents/maki.toml", false).unwrap();
    assert_eq!(by_path.files.len(), 2);
}

#[test]
fn agent_companion_escaping_its_directory_is_rejected_and_missing_one_is_a_problem() {
    let fx = fx("agent_companion_escaping_its_directory_is_rejected_and_missing_one_is_a_problem");
    let dir = fx.repo.join(".awman/agents");
    write(&fx.repo.join(".awman/escape.Dockerfile"), "FROM scratch\n");
    write(
        &dir.join("bad.toml"),
        &format!("{AGENT_TOML}[image]\ndockerfile = \"../escape.Dockerfile\"\n"),
    );
    let err = resolve(&fx, ArtifactKind::Agent, "bad", false).unwrap_err();
    assert!(matches!(err, EditTargetError::Unsafe { .. }), "{err}");

    write(
        &dir.join("gone.toml"),
        &format!("{AGENT_TOML}[image]\ndockerfile = \"gone.Dockerfile\"\n"),
    );
    let target = resolve(&fx, ArtifactKind::Agent, "gone", false).unwrap();
    assert_eq!(target.files.len(), 1);
    assert!(target
        .initial_problems
        .iter()
        .any(|p| p.contains("image.dockerfile")));
}

#[test]
fn first_party_agents_are_read_only_and_agent_paths_must_be_definitions() {
    let fx = fx("first_party_agents_are_read_only_and_agent_paths_must_be_definitions");
    let err = resolve(&fx, ArtifactKind::Agent, "claude", false).unwrap_err();
    assert!(
        matches!(err, EditTargetError::BuiltinReadOnly { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("awman new agent"));
    write(&fx.repo.join("agents/x.toml"), AGENT_TOML);
    let err = resolve(&fx, ArtifactKind::Agent, "agents/x.toml", false).unwrap_err();
    assert!(matches!(err, EditTargetError::OutsideScope { .. }), "{err}");
    let err = resolve(&fx, ArtifactKind::Agent, "Bad Name", false).unwrap_err();
    assert!(
        matches!(err, EditTargetError::InvalidReference { .. }),
        "{err}"
    );
}

#[test]
fn global_agent_is_staged_and_repo_miss_hints_at_global() {
    let fx = fx("global_agent_is_staged_and_repo_miss_hints_at_global");
    write(&fx.home.join("agents/helper.toml"), AGENT_TOML);
    let err = resolve(&fx, ArtifactKind::Agent, "helper", false).unwrap_err();
    assert!(
        matches!(&err, EditTargetError::NotFound { hint: Some(_), .. }),
        "{err}"
    );
    let target = resolve(&fx, ArtifactKind::Agent, "helper", true).unwrap();
    assert_eq!(target.scope, ArtifactScope::Global);
    assert!(target.workspace_rel.is_none());
}

#[cfg(unix)]
#[test]
fn symlinked_repo_agents_directory_is_refused() {
    let fx = fx("symlinked_repo_agents_directory_is_refused");
    let real = fx.repo.join("real-agents");
    write(&real.join("x.toml"), AGENT_TOML);
    std::fs::create_dir_all(fx.repo.join(".awman")).unwrap();
    std::os::unix::fs::symlink(&real, fx.repo.join(".awman/agents")).unwrap();
    let err = resolve(&fx, ArtifactKind::Agent, "x", false).unwrap_err();
    assert!(matches!(err, EditTargetError::Unsafe { .. }), "{err}");
}

// ── in-place check ──

#[test]
fn in_place_check_reports_changes_new_skill_files_and_validation_problems() {
    let fx = fx("in_place_check_reports_changes_new_skill_files_and_validation_problems");
    let dir = fx.repo.join(".claude/skills/demo");
    write(&dir.join("SKILL.md"), SKILL_MD);
    let target = resolve(&fx, ArtifactKind::Skill, "demo", false).unwrap();
    let report = check_in_place(&target).unwrap();
    assert!(report.changed.is_empty() && report.problems.is_empty());

    write(&dir.join("SKILL.md"), "---\nname: [unclosed\n");
    write(&dir.join("extra.md"), "more");
    let report = check_in_place(&target).unwrap();
    assert_eq!(
        report.changed,
        vec![dir.join("SKILL.md"), dir.join("extra.md")]
    );
    assert!(!report.problems.is_empty());
}

#[test]
fn skill_frontmatter_rules() {
    assert!(skill_md_problems("# plain body\n").is_empty());
    assert!(skill_md_problems(SKILL_MD).is_empty());
    assert!(!skill_md_problems("  \n").is_empty());
    assert!(!skill_md_problems("---\nname: x\n").is_empty());
    assert!(!skill_md_problems("---\n- a list\n---\n").is_empty());
}

#[cfg(unix)]
#[test]
fn in_place_validation_never_parses_a_swapped_symlink_or_ancestor() {
    let fx = fx("in_place_validation_never_parses_a_swapped_symlink_or_ancestor");
    let secret_text = "PRIVATE-SENTINEL = = not toml\n";
    let secret = fx._sandbox.work().join("outside/demo.toml");
    write(&secret, secret_text);
    let dir = fx.repo.join(".awman/agents");
    write(&dir.join("demo.toml"), AGENT_TOML);
    let target = resolve(&fx, ArtifactKind::Agent, "demo", false).unwrap();

    // The definition itself is replaced by a link to a host file.
    std::fs::remove_file(dir.join("demo.toml")).unwrap();
    std::os::unix::fs::symlink(&secret, dir.join("demo.toml")).unwrap();
    let report = check_in_place(&target).unwrap();
    assert!(!report.problems.is_empty(), "the unsafe read is reported");
    assert!(
        report
            .problems
            .iter()
            .all(|p| !p.contains("PRIVATE-SENTINEL")),
        "outside content leaked: {:?}",
        report.problems
    );

    // An ancestor of the definition is replaced by a link.
    std::fs::rename(&dir, fx.repo.join(".awman/agents-moved")).unwrap();
    std::os::unix::fs::symlink(secret.parent().unwrap(), &dir).unwrap();
    let report = check_in_place(&target).unwrap();
    assert!(!report.problems.is_empty(), "the unsafe read is reported");
    assert!(
        report
            .problems
            .iter()
            .all(|p| !p.contains("PRIVATE-SENTINEL")),
        "outside content leaked: {:?}",
        report.problems
    );
}
