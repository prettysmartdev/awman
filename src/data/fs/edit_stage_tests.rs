//! Staging and host-side publication of global/library artifacts: copy,
//! no-change, conflicts, invalid output, unsafe drafts and partial writes.

use super::*;
use crate::data::fs::edit_target::tests::{fx_in, write, Fx, AGENT_TOML, SKILL_MD, WORKFLOW_TOML};
use crate::data::fs::edit_target::{resolve_artifact_target, ArtifactRequest, ArtifactScope};

fn fx(name: &str) -> Fx {
    fx_in("data::fs::edit_stage::tests", name)
}

fn global_target(fx: &Fx, kind: ArtifactKind, reference: &str) -> ArtifactTarget {
    resolve_artifact_target(&ArtifactRequest {
        kind,
        reference,
        global: true,
        env: &fx.env,
        git_root: &fx.repo,
        definition_root: &fx.repo,
        cwd: &fx.repo,
    })
    .unwrap()
}

fn stage(fx: &Fx, target: &ArtifactTarget) -> EditStage {
    let context = fx.repo.parent().unwrap().join("repo-context");
    std::fs::create_dir_all(&context).unwrap();
    EditStage::create(target, stage_paths(&context, uuid::Uuid::new_v4())).unwrap()
}

#[test]
fn stage_paths_follow_the_repo_context_layout() {
    let id = uuid::Uuid::nil();
    let paths = stage_paths(Path::new("/ctx"), id);
    assert_eq!(
        paths.host_dir,
        Path::new("/ctx/scratch/edit").join(id.to_string())
    );
    assert_eq!(
        paths.container_dir,
        format!("/awman/context/repo/scratch/edit/{id}")
    );
}

#[test]
fn staged_global_workflow_edit_is_validated_and_published_back() {
    let fx = fx("staged_global_workflow_edit_is_validated_and_published_back");
    let dest = fx.home.join("workflows/release.toml");
    write(&dest, WORKFLOW_TOML);
    let target = global_target(&fx, ArtifactKind::Workflow, "release");
    let stage = stage(&fx, &target);
    let staged = stage.paths().host_dir.join("release.toml");
    assert_eq!(std::fs::read_to_string(&staged).unwrap(), WORKFLOW_TOML);
    assert!(stage
        .container_path("release.toml")
        .ends_with("/release.toml"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&stage.paths().host_dir)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    let edited = format!("{WORKFLOW_TOML}\n[[steps]]\nname = \"ship\"\nprompt = \"ship\"\ndepends_on = [\"build\"]\n");
    std::fs::write(&staged, &edited).unwrap();
    std::fs::write(
        stage.paths().host_dir.join("scratch.txt"),
        "agent leftovers",
    )
    .unwrap();
    let host_dir = stage.paths().host_dir.clone();
    let report = stage.publish().unwrap();
    assert_eq!(report.published, vec![dest.clone()]);
    assert_eq!(report.ignored, vec!["scratch.txt".to_string()]);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), edited);
    assert!(!host_dir.exists(), "a successful publish removes the draft");
}

#[test]
fn no_change_session_writes_nothing_and_discard_removes_the_stage() {
    let fx = fx("no_change_session_writes_nothing_and_discard_removes_the_stage");
    let dest = fx.home.join("workflows/release.toml");
    write(&dest, WORKFLOW_TOML);
    let before = std::fs::metadata(&dest).unwrap().modified().unwrap();
    let target = global_target(&fx, ArtifactKind::Workflow, "release");
    let report = stage(&fx, &target).publish().unwrap();
    assert!(report.published.is_empty());
    assert_eq!(
        std::fs::metadata(&dest).unwrap().modified().unwrap(),
        before
    );

    let s = stage(&fx, &target);
    let dir = s.paths().host_dir.clone();
    s.discard().unwrap();
    assert!(!dir.exists());
}

#[test]
fn concurrent_destination_change_is_a_conflict_and_keeps_both_versions() {
    let fx = fx("concurrent_destination_change_is_a_conflict_and_keeps_both_versions");
    let dest = fx.home.join("workflows/release.toml");
    write(&dest, WORKFLOW_TOML);
    let target = global_target(&fx, ArtifactKind::Workflow, "release");
    let stage = stage(&fx, &target);
    let staged = stage.paths().host_dir.join("release.toml");
    std::fs::write(&staged, format!("{WORKFLOW_TOML}# agent\n")).unwrap();
    let user = format!("{WORKFLOW_TOML}# user edit\n");
    std::fs::write(&dest, &user).unwrap();
    let failure = stage.publish().unwrap_err();
    assert!(matches!(&failure.kind, PublishFailureKind::Conflict(p) if p == &vec![dest.clone()]));
    assert_eq!(
        std::fs::read_to_string(&dest).unwrap(),
        user,
        "never silently overwritten"
    );
    assert!(staged.exists(), "the draft is kept for recovery");
    assert!(failure
        .to_string()
        .contains(&failure.stage_dir.display().to_string()));
}

#[test]
fn invalid_staged_bundle_is_rejected_without_writing() {
    let fx = fx("invalid_staged_bundle_is_rejected_without_writing");
    let dest = fx.home.join("workflows/release.toml");
    write(&dest, WORKFLOW_TOML);
    let target = global_target(&fx, ArtifactKind::Workflow, "release");
    let stage = stage(&fx, &target);
    let staged = stage.paths().host_dir.join("release.toml");
    std::fs::write(&staged, "not = = toml").unwrap();
    let failure = stage.publish().unwrap_err();
    let PublishFailureKind::Invalid(problems) = &failure.kind else {
        panic!("{failure}")
    };
    assert!(
        problems.iter().all(|p| !p.contains("awman-edit-validate")),
        "{problems:?}"
    );
    assert_eq!(failure.pending, vec![dest.clone()]);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), WORKFLOW_TOML);
    assert!(staged.exists());

    // Removing the primary file is invalid too.
    let stage2 = self::stage(&fx, &target);
    std::fs::remove_file(stage2.paths().host_dir.join("release.toml")).unwrap();
    assert!(matches!(
        stage2.publish().unwrap_err().kind,
        PublishFailureKind::Invalid(_)
    ));
}

#[cfg(unix)]
#[test]
fn symlinks_in_the_draft_make_it_unreadable() {
    let fx = fx("symlinks_in_the_draft_make_it_unreadable");
    write(&fx.home.join("skills/demo/SKILL.md"), SKILL_MD);
    let target = global_target(&fx, ArtifactKind::Skill, "demo");
    let stage = stage(&fx, &target);
    let secret = fx.repo.parent().unwrap().join("secret");
    write(&secret, "private");
    std::os::unix::fs::symlink(&secret, stage.paths().host_dir.join("leak.md")).unwrap();
    let failure = stage.publish().unwrap_err();
    assert!(
        matches!(failure.kind, PublishFailureKind::Unreadable(_)),
        "{failure}"
    );
    assert!(!fx.home.join("skills/demo/leak.md").exists());

    // A stage directory swapped for a symlink fails closed too.
    let stage = self::stage(&fx, &target);
    let dir = stage.paths().host_dir.clone();
    std::fs::remove_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(fx.home.join("skills/demo"), &dir).unwrap();
    assert!(matches!(
        stage.publish().unwrap_err().kind,
        PublishFailureKind::Unreadable(_)
    ));
    assert!(fx.home.join("skills/demo/SKILL.md").exists());
}

#[test]
fn skill_stage_publishes_new_files_and_keeps_deleted_ones() {
    let fx = fx("skill_stage_publishes_new_files_and_keeps_deleted_ones");
    let lib = fx.home.join("skills/.library/lib");
    write(
        &lib.join(".awman.json"),
        r#"{"source":"s","owner":"o","repo":"lib","subdir":"skills"}"#,
    );
    let dir = lib.join("skills/demo");
    write(&dir.join("SKILL.md"), SKILL_MD);
    write(&dir.join("old.md"), "old");
    let target = global_target(&fx, ArtifactKind::Skill, "lib/demo");
    assert_eq!(target.scope, ArtifactScope::Library { slug: "lib".into() });
    let stage = stage(&fx, &target);
    let host = stage.paths().host_dir.clone();
    std::fs::remove_file(host.join("old.md")).unwrap();
    write(&host.join("refs/new file.md"), "new");
    std::fs::write(host.join("SKILL.md"), format!("{SKILL_MD}more\n")).unwrap();
    let report = stage.publish().unwrap();
    assert_eq!(
        report.published,
        vec![dir.join("refs/new file.md"), dir.join("SKILL.md")]
    );
    assert!(report.ignored.iter().any(|i| i.starts_with("old.md")));
    assert!(dir.join("old.md").exists());
    assert!(
        !lib.join("refs").exists(),
        "nothing written outside the skill directory"
    );
}

#[test]
fn partial_publication_reports_written_and_pending_files_and_keeps_the_draft() {
    let fx = fx("partial_publication_reports_written_and_pending_files_and_keeps_the_draft");
    let agents = fx.home.join("agents");
    write(
        &agents.join("maki.toml"),
        &format!("{AGENT_TOML}[image]\ndockerfile = \"maki.Dockerfile\"\n"),
    );
    write(&agents.join("maki.Dockerfile"), "FROM scratch\n");
    let target = global_target(&fx, ArtifactKind::Agent, "maki");
    assert!(
        target.initial_problems.is_empty(),
        "{:?}",
        target.initial_problems
    );
    let stage = stage(&fx, &target);
    let host = stage.paths().host_dir.clone();
    std::fs::write(host.join("maki.Dockerfile"), "FROM scratch\nRUN true\n").unwrap();
    std::fs::write(
        host.join("maki.toml"),
        format!("{AGENT_TOML}[image]\ndockerfile = \"maki.Dockerfile\"\n# edited\n"),
    )
    .unwrap();
    FAIL_WRITE.with(|f| *f.borrow_mut() = Some("maki.toml".into()));
    let failure = stage.publish().unwrap_err();
    FAIL_WRITE.with(|f| *f.borrow_mut() = None);
    assert!(
        matches!(failure.kind, PublishFailureKind::Write { .. }),
        "{failure}"
    );
    // The companion went first; the definition did not follow.
    assert_eq!(failure.published, vec![agents.join("maki.Dockerfile")]);
    assert_eq!(failure.pending, vec![agents.join("maki.toml")]);
    assert!(failure.to_string().contains("partial publication"));
    assert!(host.join("maki.toml").exists());
    assert!(!std::fs::read_to_string(agents.join("maki.toml"))
        .unwrap()
        .contains("# edited"));
}

#[test]
fn staged_agent_may_add_a_newly_referenced_dockerfile_only() {
    let fx = fx("staged_agent_may_add_a_newly_referenced_dockerfile_only");
    let agents = fx.home.join("agents");
    write(&agents.join("demo.toml"), AGENT_TOML);
    let target = global_target(&fx, ArtifactKind::Agent, "demo");
    let stage = stage(&fx, &target);
    let host = stage.paths().host_dir.clone();
    std::fs::write(
        host.join("demo.toml"),
        format!("{AGENT_TOML}[image]\ndockerfile = \"demo.Dockerfile\"\n"),
    )
    .unwrap();
    std::fs::write(host.join("demo.Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(host.join("other.txt"), "ignored").unwrap();
    let report = stage.publish().unwrap();
    assert_eq!(
        report.published,
        vec![agents.join("demo.Dockerfile"), agents.join("demo.toml")]
    );
    assert_eq!(report.ignored, vec!["other.txt".to_string()]);
    assert!(!agents.join("other.txt").exists());
}

#[test]
fn stage_creation_fails_when_the_source_changed_since_resolution() {
    let fx = fx("stage_creation_fails_when_the_source_changed_since_resolution");
    let dest = fx.home.join("workflows/release.toml");
    write(&dest, WORKFLOW_TOML);
    let target = global_target(&fx, ArtifactKind::Workflow, "release");
    std::fs::write(&dest, format!("{WORKFLOW_TOML}# changed\n")).unwrap();
    let context = fx.repo.parent().unwrap().join("repo-context");
    std::fs::create_dir_all(&context).unwrap();
    let paths = stage_paths(&context, uuid::Uuid::new_v4());
    assert!(EditStage::create(&target, paths.clone()).is_err());
    assert!(!paths.host_dir.exists());
}

fn context_of(fx: &Fx) -> PathBuf {
    let context = fx.repo.parent().unwrap().join("repo-context");
    std::fs::create_dir_all(&context).unwrap();
    context
}

#[cfg(unix)]
#[test]
fn discard_after_an_ancestor_swap_never_deletes_outside_the_context() {
    let fx = fx("discard_after_an_ancestor_swap_never_deletes_outside_the_context");
    write(&fx.home.join("workflows/release.toml"), WORKFLOW_TOML);
    let target = global_target(&fx, ArtifactKind::Workflow, "release");
    let context = context_of(&fx);
    let id = uuid::Uuid::new_v4();
    let stage = EditStage::create(&target, stage_paths(&context, id)).unwrap();
    // Another writer of the shared context moves `scratch` aside and puts a
    // link to a host directory with the same layout in its place.
    let outside = fx.repo.parent().unwrap().join("outside");
    let victim = outside.join("edit").join(id.to_string());
    write(&victim.join("sentinel"), "keep me");
    std::fs::rename(context.join("scratch"), context.join("scratch-moved")).unwrap();
    std::os::unix::fs::symlink(&outside, context.join("scratch")).unwrap();

    assert!(stage.discard().is_err(), "a swapped ancestor is refused");
    assert!(victim.join("sentinel").exists(), "outside data survives");
    assert!(context
        .join("scratch-moved/edit")
        .join(id.to_string())
        .join("release.toml")
        .exists());
}

#[cfg(unix)]
#[test]
fn discard_refuses_a_stage_replaced_by_another_directory() {
    let fx = fx("discard_refuses_a_stage_replaced_by_another_directory");
    write(&fx.home.join("workflows/release.toml"), WORKFLOW_TOML);
    let target = global_target(&fx, ArtifactKind::Workflow, "release");
    let stage = stage(&fx, &target);
    let host = stage.paths().host_dir.clone();
    std::fs::rename(&host, host.with_extension("moved")).unwrap();
    write(&host.join("someone-else"), "keep me");
    assert!(stage.discard().is_err());
    assert!(host.join("someone-else").exists());
}

#[cfg(unix)]
#[test]
fn stage_creation_through_a_symlinked_ancestor_writes_nothing_outside() {
    let fx = fx("stage_creation_through_a_symlinked_ancestor_writes_nothing_outside");
    write(&fx.home.join("workflows/release.toml"), WORKFLOW_TOML);
    let target = global_target(&fx, ArtifactKind::Workflow, "release");
    let context = context_of(&fx);
    let outside = fx.repo.parent().unwrap().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, context.join("scratch")).unwrap();
    let paths = stage_paths(&context, uuid::Uuid::new_v4());
    assert!(EditStage::create(&target, paths).is_err());
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn unchanged_companion_changed_at_the_destination_is_a_conflict() {
    let fx = fx("unchanged_companion_changed_at_the_destination_is_a_conflict");
    let agents = fx.home.join("agents");
    let toml = format!("{AGENT_TOML}[image]\ndockerfile = \"demo.Dockerfile\"\n");
    write(&agents.join("demo.toml"), &toml);
    write(&agents.join("demo.Dockerfile"), "FROM scratch\n");
    let target = global_target(&fx, ArtifactKind::Agent, "demo");
    assert_eq!(target.files.len(), 2);
    let stage = stage(&fx, &target);
    let staged = stage.paths().host_dir.join("demo.toml");
    std::fs::write(&staged, format!("{toml}# edited\n")).unwrap();
    // Only the primary changed in the draft; the companion vanishes on disk.
    std::fs::remove_file(agents.join("demo.Dockerfile")).unwrap();
    let failure = stage.publish().unwrap_err();
    assert!(
        matches!(&failure.kind, PublishFailureKind::Conflict(p) if p.contains(&agents.join("demo.Dockerfile"))),
        "{failure}"
    );
    assert_eq!(
        std::fs::read_to_string(agents.join("demo.toml")).unwrap(),
        toml
    );
    assert!(staged.exists(), "the draft is kept");
}

#[test]
fn supporting_file_deleted_in_the_draft_but_changed_on_disk_is_a_conflict() {
    let fx = fx("supporting_file_deleted_in_the_draft_but_changed_on_disk_is_a_conflict");
    let dir = fx.home.join("skills/demo");
    write(&dir.join("SKILL.md"), SKILL_MD);
    write(&dir.join("notes.md"), "original");
    let target = global_target(&fx, ArtifactKind::Skill, "demo");
    let stage = stage(&fx, &target);
    let host = stage.paths().host_dir.clone();
    std::fs::write(host.join("SKILL.md"), format!("{SKILL_MD}more\n")).unwrap();
    std::fs::remove_file(host.join("notes.md")).unwrap();
    std::fs::write(dir.join("notes.md"), "changed by someone else").unwrap();
    let failure = stage.publish().unwrap_err();
    assert!(
        matches!(&failure.kind, PublishFailureKind::Conflict(p) if p == &vec![dir.join("notes.md")]),
        "{failure}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
        SKILL_MD
    );
}
