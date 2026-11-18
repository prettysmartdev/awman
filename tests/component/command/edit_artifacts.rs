//! Component journeys for `new|edit workflow|skill|agent`. The production command
//! launches a fake Docker CLI; the test edits the mounted host files while the
//! fake run is held, then releases it to exercise real save/validation paths.
use crate::support;
use awman::command::commands::edit::{
    EditArtifactFlags, EditCommand, EditCommandFrontend, EditCommonFlags, EditSubcommand,
};
use awman::command::commands::{
    agent_auth::*, agent_setup::*, agent_trust::AgentTrustFrontend, mount_scope::*, Command,
};
use awman::command::dispatch::Engines;
use awman::command::error::CommandError;
use awman::data::message::{UserMessage, UserMessageSink};
use awman::data::session::{AgentName, Session, SessionOpenOptions};
use awman::engine::agent::registry::{AgentRegistry, TrustStatus};
use awman::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use awman_test_support::fakes::FakeCli;
use awman_test_support::fixtures::{ConfigTree, RepoBuilder};
use awman_test_support::TestSandbox;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(15);

/// Messages written, plus the mount-scope answer to give (default: the Git
/// root) and how many times the scope was asked.
#[derive(Clone, Default)]
struct Messages(
    Arc<Mutex<Vec<String>>>,
    Option<MountScopeDecision>,
    Arc<Mutex<usize>>,
);
struct Front(Messages);
impl UserMessageSink for Front {
    fn write_message(&mut self, msg: UserMessage) {
        self.0 .0.lock().unwrap().push(msg.text);
    }
    fn replay_queued(&mut self) {}
}
impl MountScopeFrontend for Front {
    fn ask_mount_scope(&mut self, _: &Path, _: &Path) -> Result<MountScopeDecision, CommandError> {
        *self.0 .2.lock().unwrap() += 1;
        Ok(self.0 .1.unwrap_or(MountScopeDecision::MountGitRoot))
    }
}
impl AgentSetupFrontend for Front {
    fn ask_agent_setup(
        &mut self,
        _: &AgentName,
        _: &AgentName,
        _: bool,
        _: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        Ok(AgentSetupDecision::Abort)
    }
    fn record_fallback(&mut self, _: &AgentName, _: &AgentName) {}
}
impl AgentAuthFrontend for Front {
    fn ask_agent_auth_consent(
        &mut self,
        _: &AgentName,
        _: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        Ok(AgentAuthDecision::DeclineOnce)
    }
}
impl HasAgentFrontend for Front {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(Piped)
    }
}
impl AgentTrustFrontend for Front {}
impl AgentLaunchFrontend for Front {
    fn set_pty_active(&mut self, _: bool) {}
}
impl EditCommandFrontend for Front {}
impl awman::command::work_item::spec_service::NewSpecFrontend for Front {}
impl awman::command::commands::new::NewCommandFrontend for Front {}
struct Piped;
impl UserMessageSink for Piped {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}
#[async_trait::async_trait]
impl AgentFrontend for Piped {
    fn report_status(&mut self, _: AgentStatus) {}
    fn report_progress(&mut self, _: AgentProgress) {}
    fn take_io(&mut self) -> AgentIo {
        let (stdout, mut o) = tokio::sync::mpsc::unbounded_channel();
        let (stderr, mut e) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move { while o.recv().await.is_some() {} });
        tokio::spawn(async move { while e.recv().await.is_some() {} });
        AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
    fn grace_timeout(&self) -> Duration {
        Duration::from_secs(600)
    }
}

struct World {
    _sandbox: TestSandbox,
    root: PathBuf,
    session: Session,
    engines: Engines,
    docker: FakeCli,
}
fn world(id: &'static str, files: &[(&str, &str)]) -> World {
    world_with(id, files, json!({}), None)
}

/// `repo_config` is written as the repository config; `cwd` (relative to the
/// checkout) is the session's working directory.
fn world_with(
    id: &'static str,
    files: &[(&str, &str)],
    repo_config: serde_json::Value,
    cwd: Option<&str>,
) -> World {
    let mut sandbox = support::sandbox(id);
    let mut b = RepoBuilder::new(&sandbox, "edit-artifacts");
    for (p, c) in files {
        b = b.file(p, c);
    }
    let repo = b.init();
    let root = repo.path().to_path_buf();
    ConfigTree::new()
        .global(
            json!({"runtime":{"kind":"docker"},"agents":{"auth":{"refresh":{"enabled":false}}}}),
        )
        .repo(repo_config)
        .write(&sandbox, Some(&root));
    let docker = sandbox.install_fake(support::fake_cli_exe(), "docker");
    docker.on(&[]);
    awman_test_support::fakes::docker_cli_baseline(&docker);
    docker
        .on(&["image", "inspect"])
        .stdout("[{\"Config\":{\"Env\":[\"HOME=/home/agent\"]}}]\n");
    docker.on(&["run"]).hold_until_released();
    let tmp = sandbox.root().display().to_string();
    for k in ["TMPDIR", "TMP", "TEMP"] {
        sandbox.env_mut().set(k, &tmp);
    }
    let env = support::env_snapshot(&sandbox);
    let working_dir = match cwd {
        Some(rel) => {
            let dir = root.join(rel);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }
        None => root.clone(),
    };
    let session = Session::open_at_git_root(
        working_dir,
        root.clone(),
        SessionOpenOptions {
            env: Some(env.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut engines = Engines::for_env(env.clone()).unwrap();
    let runtime = Arc::new(awman::engine::container::ContainerRuntime::docker_in(env));
    engines.runtime = runtime.clone();
    engines.container_runtime = Some(runtime.clone());
    engines.agent_engine = Arc::new(awman::engine::agent::AgentEngine::new(
        engines.overlay_engine.clone(),
        runtime,
    ));
    World {
        _sandbox: sandbox,
        root,
        session,
        engines,
        docker,
    }
}
fn command(
    w: &World,
    kind: &str,
    reference: &str,
    amend: bool,
    prompt: Option<&str>,
    global: bool,
) -> EditCommand {
    command_with_overlays(w, kind, reference, amend, prompt, global, &[])
}
fn command_with_overlays(
    w: &World,
    kind: &str,
    reference: &str,
    amend: bool,
    prompt: Option<&str>,
    global: bool,
    overlays: &[&str],
) -> EditCommand {
    let common = EditCommonFlags {
        reference: reference.into(),
        amend,
        headless: true,
        prompt: prompt.map(str::to_owned),
        json: false,
        allow_docker: false,
        yolo: false,
        auto: false,
        agent: None,
        model: None,
        overlays: overlays.iter().map(|s| s.to_string()).collect(),
        agent_flags: None,
    };
    let artifact = EditArtifactFlags { common, global };
    let sub = match kind {
        "workflow" => EditSubcommand::Workflow(artifact),
        "skill" => EditSubcommand::Skill(artifact),
        "agent" => EditSubcommand::Agent(artifact),
        _ => unreachable!(),
    };
    EditCommand::new(sub, w.engines.clone(), w.session.clone())
}
async fn held(docker: &FakeCli, prior_holds: usize) {
    let d = docker.clone();
    tokio::task::spawn_blocking(move || {
        awman_test_support::wait_for("new fake docker run hold", LIMIT, || {
            d.calls().into_iter().filter(|c| c.held).nth(prior_holds)
        })
    })
    .await
    .unwrap();
}
async fn finish<T: Send + 'static>(task: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(LIMIT, task)
        .await
        .expect("command ended")
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn creation_authors_scoped_files_with_supplied_brief_and_yolo() {
    use awman::command::commands::new::{
        NewAgentFlags, NewCommand, NewInput, NewSkillFlags, NewSubcommand, NewWorkflowFlags,
    };
    for global in [false, true] {
        for kind in ["toml", "yaml", "skill", "agent"] {
            let w = world("command::edit_artifacts::creation_authors_scoped_files_with_supplied_brief_and_yolo", &[]);
            w.docker.on(&["run"]).read_stdin().hold_until_released();
            let input = NewInput {
                title: Some("authoring-test".into()),
                summary: Some("Keep the literal {path} and add useful instructions".into()),
            };
            let (sub, relative, content) = match kind {
                "toml" | "yaml" => (
                    NewSubcommand::Workflow(NewWorkflowFlags {
                        input,
                        non_interactive: true,
                        global,
                        format: kind.into(),
                        yolo: true,
                    }),
                    format!("workflows/authoring-test.{kind}"),
                    if kind == "toml" {
                        "title = \"Authored\"\n[[step]]\nname = \"check\"\nprompt = \"Check the project\"\n"
                    } else {
                        "title: Authored\nsteps:\n  - name: check\n    prompt: Check the project\n"
                    },
                ),
                "skill" => (
                    NewSubcommand::Skill(NewSkillFlags {
                        input,
                        non_interactive: true,
                        global,
                        yolo: true,
                        ..Default::default()
                    }),
                    "skills/authoring-test/SKILL.md".into(),
                    "---\nname: authoring-test\ndescription: Check the project when asked to review it.\n---\n\nReview the project.\n",
                ),
                "agent" => (
                    NewSubcommand::Agent(NewAgentFlags {
                        input,
                        name: "authoring-test".into(),
                        non_interactive: true,
                        global,
                        yolo: true,
                        ..Default::default()
                    }),
                    "agents/authoring-test.toml".into(),
                    awman::command::commands::new::agent::EXAMPLE_CRUSH,
                ),
                _ => unreachable!(),
            };
            let path = if global {
                w._sandbox.awman_home()
            } else if kind == "toml" || kind == "yaml" {
                w.root.join("aspec")
            } else if kind == "skill" {
                w.root.join(".claude")
            } else {
                w.root.join(".awman")
            }
            .join(relative);
            let command = NewCommand::new(sub, w.engines.clone(), w.session.clone());
            let task =
                tokio::spawn(command.run_with_frontend(Box::new(Front(Messages::default()))));
            held(&w.docker, 0).await;
            let run = w
                .docker
                .calls()
                .into_iter()
                .find(|c| c.argv.first().is_some_and(|a| a == "run"))
                .unwrap();
            let argv = run.argv.join(" ");
            let prompt = String::from_utf8(run.stdin).unwrap();
            assert!(prompt.contains("Keep the literal {path}"), "{prompt}");
            assert!(argv.contains("--dangerously-skip-permissions"), "{argv}");
            assert!(
                !argv.contains(&format!("{}:", w._sandbox.home().display())),
                "{argv}"
            );
            let editable = if global {
                assert!(argv.contains("/awman/context/repo"), "{argv}");
                walk(&w._sandbox.awman_home().join("context"))
                    .into_iter()
                    .find(|candidate| candidate.file_name() == path.file_name())
                    .expect("staged file")
            } else {
                path.clone()
            };
            std::fs::write(&editable, content)
                .unwrap_or_else(|e| panic!("{kind} global={global}, {}: {e}", editable.display()));
            w.docker.release();
            finish(task)
                .await
                .unwrap_or_else(|e| panic!("{kind} global={global}: {e}"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
            if kind == "agent" && !global {
                let name = AgentName::new("authoring-test").unwrap();
                let definition = AgentRegistry::load_untrusted(
                    w.session.env(),
                    &name,
                    Some(w.session.definition_root()),
                )
                .unwrap();
                assert_eq!(
                    AgentRegistry::trust_status(
                        w.session.env(),
                        definition.definition(),
                        Some(w.session.definition_root())
                    )
                    .unwrap(),
                    TrustStatus::Pending
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn creation_requires_a_brief_and_never_overwrites_existing_targets() {
    use awman::command::commands::new::{
        NewAgentFlags, NewCommand, NewInput, NewSkillFlags, NewSubcommand, NewWorkflowFlags,
    };
    for kind in ["workflow", "skill", "agent"] {
        let relative = match kind {
            "workflow" => "aspec/workflows/existing.toml",
            "skill" => ".claude/skills/existing/SKILL.md",
            "agent" => ".awman/agents/existing.toml",
            _ => unreachable!(),
        };
        let w = world(
            "command::edit_artifacts::creation_requires_a_brief_and_never_overwrites_existing_targets",
            &[(relative, "keep the original")],
        );
        for summary in [None, Some("Revise the instructions".to_string())] {
            let missing_brief = summary.is_none();
            let input = NewInput {
                title: Some("existing".into()),
                summary,
            };
            let sub = match kind {
                "workflow" => NewSubcommand::Workflow(NewWorkflowFlags {
                    input,
                    non_interactive: true,
                    format: "toml".into(),
                    ..Default::default()
                }),
                "skill" => NewSubcommand::Skill(NewSkillFlags {
                    input,
                    non_interactive: true,
                    ..Default::default()
                }),
                "agent" => NewSubcommand::Agent(NewAgentFlags {
                    input,
                    non_interactive: true,
                    name: "existing".into(),
                    ..Default::default()
                }),
                _ => unreachable!(),
            };
            let error = NewCommand::new(sub, w.engines.clone(), w.session.clone())
                .run_with_frontend(Box::new(Front(Messages::default())))
                .await
                .unwrap_err();
            if missing_brief {
                assert!(
                    matches!(error, CommandError::InteractiveInputUnavailable { .. }),
                    "{error}"
                );
                assert!(error.to_string().contains("--prompt"), "{error}");
            }
            assert_eq!(
                std::fs::read_to_string(w.root.join(relative)).unwrap(),
                "keep the original"
            );
            assert!(!w
                .docker
                .calls()
                .iter()
                .any(|c| c.argv.first().is_some_and(|a| a == "run")));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn local_workflow_toml_yaml_and_skill_edit_in_place_preserve_identity_and_neighbors() {
    let w = world(
        "command::edit_artifacts::local_workflows_and_skill",
        &[
            (
                "aspec/workflows/release.toml",
                "title = \"Release\"\n\n[[steps]]\nname = \"build\"\nprompt = \"old\"\n",
            ),
            (
                "aspec/workflows/check.yaml",
                "title: Check\nsteps:\n  - name: build\n    prompt: old\n",
            ),
            (
                ".claude/skills/guide/SKILL.md",
                "---\nname: guide\n---\nold\n",
            ),
            (".claude/skills/guide/helper.txt", "keep\n"),
            ("aspec/workflows/neighbor.toml", "name = \"neighbor\"\n"),
        ],
    );
    for (kind, name, path, replacement) in [
        (
            "workflow",
            "release",
            "aspec/workflows/release.toml",
            "title = \"Release\"\n\n[[steps]]\nname = \"build\"\nprompt = \"new\"\n",
        ),
        (
            "workflow",
            "check",
            "aspec/workflows/check.yaml",
            "title: Check\nsteps:\n  - name: build\n    prompt: new\n",
        ),
        (
            "skill",
            "guide",
            ".claude/skills/guide/SKILL.md",
            "---\nname: guide\n---\nnew\n",
        ),
    ] {
        let prior_holds = w.docker.calls().into_iter().filter(|c| c.held).count();
        let task = tokio::spawn(
            command(&w, kind, name, false, Some("change it"), false)
                .run_with_frontend(Box::new(Front(Messages::default()))),
        );
        held(&w.docker, prior_holds).await;
        std::fs::write(w.root.join(path), replacement).unwrap();
        w.docker.release();
        let outcome = finish(task).await.unwrap();
        assert_eq!(
            outcome.target.files[0].host_path,
            w.root.join(path).canonicalize().unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(w.root.join(path)).unwrap(),
            replacement
        );
        assert_eq!(
            outcome.target.persistence,
            awman::command::commands::edit::EditPersistence::InPlace
        );
    }
    assert_eq!(
        std::fs::read_to_string(w.root.join(".claude/skills/guide/helper.txt")).unwrap(),
        "keep\n"
    );
    assert_eq!(
        std::fs::read_to_string(w.root.join("aspec/workflows/neighbor.toml")).unwrap(),
        "name = \"neighbor\"\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn global_workflow_is_staged_and_published_without_exposing_home_or_siblings() {
    let w = world(
        "command::edit_artifacts::global_staging",
        &[("unrelated.txt", "keep")],
    );
    let global = w._sandbox.awman_home().join("workflows");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(
        global.join("release.toml"),
        "title = \"Release\"\n\n[[steps]]\nname = \"build\"\nprompt = \"old\"\n",
    )
    .unwrap();
    std::fs::write(global.join("sibling.toml"), "name = \"sibling\"\n").unwrap();
    let prior_holds = w.docker.calls().into_iter().filter(|c| c.held).count();
    let task = tokio::spawn(
        command(&w, "workflow", "release", true, None, true)
            .run_with_frontend(Box::new(Front(Messages::default()))),
    );
    held(&w.docker, prior_holds).await;
    let run = w
        .docker
        .calls()
        .into_iter()
        .find(|c| c.argv.first().is_some_and(|a| a == "run"))
        .unwrap();
    let argv = run.argv.join(" ");
    assert!(argv.contains(":/awman/context/repo"), "{argv}");
    assert!(
        !argv.contains(&format!("{}:", w._sandbox.home().display())),
        "whole host HOME mounted: {argv}"
    );
    let stage = walk(&w._sandbox.awman_home().join("context"))
        .into_iter()
        .find(|p| p.ends_with("release.toml"))
        .expect("staged target");
    std::fs::write(
        &stage,
        "title = \"Release\"\n\n[[steps]]\nname = \"build\"\nprompt = \"new\"\n",
    )
    .unwrap();
    w.docker.release();
    let outcome = finish(task).await.unwrap();
    assert!(matches!(
        outcome.target.persistence,
        awman::command::commands::edit::EditPersistence::Staged { .. }
    ));
    assert_eq!(
        std::fs::read_to_string(global.join("release.toml")).unwrap(),
        "title = \"Release\"\n\n[[steps]]\nname = \"build\"\nprompt = \"new\"\n"
    );
    assert_eq!(
        std::fs::read_to_string(global.join("sibling.toml")).unwrap(),
        "name = \"sibling\"\n"
    );
    assert_eq!(
        std::fs::read_to_string(w.root.join("unrelated.txt")).unwrap(),
        "keep"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn staged_invalid_output_keeps_original_and_recoverable_draft() {
    let w = world(
        "command::edit_artifacts::staged_recovery",
        &[("unrelated.txt", "untouched")],
    );
    let global = w._sandbox.awman_home().join("workflows");
    std::fs::create_dir_all(&global).unwrap();
    let baseline = "title = \"Recoverable\"\n\n[[step]]\nname = \"run\"\nprompt = \"baseline\"\n";
    std::fs::write(global.join("recover.toml"), baseline).unwrap();
    let prior_holds = w.docker.calls().into_iter().filter(|c| c.held).count();
    let invalid = tokio::spawn(
        command(&w, "workflow", "recover", true, None, true)
            .run_with_frontend(Box::new(Front(Messages::default()))),
    );
    held(&w.docker, prior_holds).await;
    let staged = walk(&w._sandbox.awman_home().join("context"))
        .into_iter()
        .find(|p| p.ends_with("recover.toml"))
        .expect("workflow draft");
    std::fs::write(&staged, "title = \"broken\"\n").unwrap();
    w.docker.release();
    let error = finish(invalid).await.unwrap_err().to_string();
    assert!(error.contains("invalid"), "{error}");
    assert!(
        staged.exists(),
        "invalid draft should be retained for recovery"
    );
    assert_eq!(
        std::fs::read_to_string(global.join("recover.toml")).unwrap(),
        baseline
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn staged_baseline_conflict_keeps_concurrent_original_and_draft() {
    let w = world(
        "command::edit_artifacts::staged_conflict",
        &[("unrelated.txt", "untouched")],
    );
    let global = w._sandbox.awman_home().join("workflows");
    std::fs::create_dir_all(&global).unwrap();
    let baseline = "title = \"Recoverable\"\n\n[[step]]\nname = \"run\"\nprompt = \"baseline\"\n";
    std::fs::write(global.join("recover.toml"), baseline).unwrap();
    let task = tokio::spawn(
        command(&w, "workflow", "recover", true, None, true)
            .run_with_frontend(Box::new(Front(Messages::default()))),
    );
    held(&w.docker, 0).await;
    let staged = walk(&w._sandbox.awman_home().join("context"))
        .into_iter()
        .find(|p| p.ends_with("recover.toml"))
        .expect("workflow draft");
    std::fs::write(&staged, baseline.replace("baseline", "agent edit")).unwrap();
    let concurrent = baseline.replace("baseline", "concurrent edit");
    std::fs::write(global.join("recover.toml"), &concurrent).unwrap();
    w.docker.release();
    let error = finish(task).await.unwrap_err().to_string();
    assert!(error.contains("changed"), "{error}");
    assert!(
        staged.exists(),
        "conflicted draft should be retained for recovery"
    );
    assert_eq!(
        std::fs::read_to_string(global.join("recover.toml")).unwrap(),
        concurrent
    );
    assert_eq!(
        std::fs::read_to_string(w.root.join("unrelated.txt")).unwrap(),
        "untouched"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_agent_definition_reports_reapproval_and_keeps_unrelated_files() {
    let definition = "schema = 1\ndisplay_name = \"Worker\"\n\n[run]\ncommand = [\"worker\"]\n\n[image]\ndockerfile = \"Worker.Dockerfile\"\n";
    let w = world(
        "command::edit_artifacts::agent_reapproval",
        &[
            (".awman/agents/worker.toml", definition),
            (".awman/agents/Worker.Dockerfile", "FROM scratch\n"),
            (".awman/agents/neighbor.toml", "leave me\n"),
        ],
    );
    let name = AgentName::new("worker").unwrap();
    let baseline_agent =
        AgentRegistry::load_untrusted(w.session.env(), &name, Some(w.session.definition_root()))
            .unwrap();
    AgentRegistry::record_approval(
        w.session.env(),
        baseline_agent.definition(),
        Some(w.session.definition_root()),
    )
    .unwrap();
    let messages = Messages::default();
    let prior_holds = w.docker.calls().into_iter().filter(|c| c.held).count();
    let task = tokio::spawn(
        command(
            &w,
            "agent",
            "worker",
            false,
            Some("change definition"),
            false,
        )
        .run_with_frontend(Box::new(Front(messages.clone()))),
    );
    held(&w.docker, prior_holds).await;
    let dockerfile = "FROM scratch\n# changed companion\n";
    std::fs::write(w.root.join(".awman/agents/Worker.Dockerfile"), dockerfile).unwrap();
    let changed = definition.replace("display_name = \"Worker\"", "display_name = \"Worker v2\"");
    std::fs::write(w.root.join(".awman/agents/worker.toml"), changed).unwrap();
    w.docker.release();
    let outcome = finish(task).await.unwrap();
    assert_eq!(outcome.changed.len(), 2);
    assert!(outcome
        .notices
        .iter()
        .any(|m| m.contains("review and approve it again with `awman ready --agent worker`")));
    assert_eq!(
        std::fs::read_to_string(w.root.join(".awman/agents/Worker.Dockerfile")).unwrap(),
        dockerfile
    );
    assert_eq!(
        std::fs::read_to_string(w.root.join(".awman/agents/neighbor.toml")).unwrap(),
        "leave me\n"
    );
    let edited =
        AgentRegistry::load_untrusted(w.session.env(), &name, Some(w.session.definition_root()))
            .unwrap();
    assert_eq!(
        AgentRegistry::trust_status(
            w.session.env(),
            edited.definition(),
            Some(w.session.definition_root())
        )
        .unwrap(),
        TrustStatus::Pending,
        "editing a definition must not approve its new content"
    );
}

#[test]
fn artifact_resolution_errors_and_headless_prompt_validation_do_not_launch() {
    let w = world(
        "command::edit_artifacts::prelaunch_errors",
        &[
            ("aspec/workflows/release.toml", "name = \"release\"\n"),
            ("aspec/workflows/release.yaml", "name: release\n"),
        ],
    );
    let rt = tokio::runtime::Runtime::new().unwrap();
    for (kind, name) in [("workflow", "missing"), ("workflow", "release")] {
        let result = rt.block_on(
            command(&w, kind, name, true, None, false)
                .run_with_frontend(Box::new(Front(Messages::default()))),
        );
        assert!(matches!(result, Err(CommandError::Edit(_))));
    }
    let global = w._sandbox.awman_home().join("workflows");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(
        global.join("only-global.toml"),
        "title = \"X\"\n[[step]]\nname = \"a\"\nprompt = \"b\"\n",
    )
    .unwrap();
    let collision = rt.block_on(
        command(&w, "workflow", "only-global", true, None, false)
            .run_with_frontend(Box::new(Front(Messages::default()))),
    );
    let collision = collision.unwrap_err().to_string();
    assert!(collision.contains("--global"), "{collision}");
    let builtin = rt.block_on(
        command(&w, "agent", "claude", true, None, false)
            .run_with_frontend(Box::new(Front(Messages::default()))),
    );
    assert!(builtin.unwrap_err().to_string().contains("read-only"));
    let escape = w._sandbox.home().join("outside.toml");
    std::fs::write(&escape, "title = \"outside\"\n").unwrap();
    #[cfg(unix)]
    {
        let link = w.root.join("aspec/workflows/escape.toml");
        std::os::unix::fs::symlink(&escape, &link).unwrap();
        let linked = rt.block_on(
            command(
                &w,
                "workflow",
                "aspec/workflows/escape.toml",
                true,
                None,
                false,
            )
            .run_with_frontend(Box::new(Front(Messages::default()))),
        );
        assert!(linked
            .unwrap_err()
            .to_string()
            .to_lowercase()
            .contains("symlink"));
    }
    let outside = rt.block_on(
        command(&w, "workflow", "../../outside.toml", true, None, false)
            .run_with_frontend(Box::new(Front(Messages::default()))),
    );
    assert!(outside.is_err());
    let cli = support::awman(&w._sandbox)
        .args(["edit", "workflow", "release", "--headless"])
        .current_dir(&w.root)
        .run();
    assert_ne!(cli.exit, awman_test_support::Exit::Code(0));
    assert!(
        cli.stderr_text().contains("needs --prompt"),
        "{}",
        cli.stderr_text()
    );
    assert!(
        w.docker.calls().is_empty(),
        "resolution failures launched docker: {:#?}",
        w.docker.calls()
    );
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn docker_runs(w: &World) -> usize {
    w.docker
        .calls()
        .into_iter()
        .filter(|c| c.argv.first().is_some_and(|a| a == "run"))
        .count()
}

fn global_workflow(w: &World) -> PathBuf {
    let global = w._sandbox.awman_home().join("workflows");
    std::fs::create_dir_all(&global).unwrap();
    let path = global.join("release.toml");
    std::fs::write(
        &path,
        "title = \"Release\"\n\n[[steps]]\nname = \"build\"\nprompt = \"old\"\n",
    )
    .unwrap();
    path
}

#[test]
fn nested_cwd_asks_for_mount_scope_and_never_widens_a_narrower_choice() {
    let w = world_with(
        "command::edit_artifacts::nested_cwd_mount_scope",
        &[(
            "aspec/workflows/release.toml",
            "title = \"Release\"\n\n[[steps]]\nname = \"build\"\nprompt = \"old\"\n",
        )],
        json!({}),
        Some("sub/dir"),
    );
    global_workflow(&w);
    let rt = tokio::runtime::Runtime::new().unwrap();
    for decision in [
        MountScopeDecision::Abort,
        MountScopeDecision::MountCurrentDirOnly,
    ] {
        // Both an in-place repository edit and a staged global edit mount the
        // repository, so both need consent.
        for global in [false, true] {
            let messages = Messages(Default::default(), Some(decision), Default::default());
            let result = rt.block_on(
                command(&w, "workflow", "release", false, Some("change it"), global)
                    .run_with_frontend(Box::new(Front(messages.clone()))),
            );
            let error = result.unwrap_err();
            match decision {
                MountScopeDecision::Abort => {
                    assert!(matches!(error, CommandError::Aborted), "{error}")
                }
                _ => assert!(
                    error.to_string().contains("requires the repository mount"),
                    "{error}"
                ),
            }
            assert_eq!(*messages.2.lock().unwrap(), 1, "mount scope asked once");
        }
    }
    assert_eq!(docker_runs(&w), 0, "nothing was launched");
    assert!(
        walk(&w._sandbox.awman_home().join("context")).is_empty(),
        "nothing was staged"
    );
}

#[test]
fn explicit_read_only_repo_context_is_never_hidden_by_an_earlier_read_write_overlay() {
    let configured = world_with(
        "command::edit_artifacts::read_only_context_config_rw",
        &[("unrelated.txt", "keep")],
        json!({"overlays": ["context(repo:rw)"]}),
        None,
    );
    let flags_only = world(
        "command::edit_artifacts::read_only_context_flag_orders",
        &[("unrelated.txt", "keep")],
    );
    let rt = tokio::runtime::Runtime::new().unwrap();
    for (w, overlays) in [
        (&configured, vec!["context(repo:ro)"]),
        (&flags_only, vec!["context(repo:rw)", "context(repo:ro)"]),
        (&flags_only, vec!["context(repo:ro)", "context(repo:rw)"]),
    ] {
        let original = global_workflow(w);
        let result = rt.block_on(
            command_with_overlays(w, "workflow", "release", true, None, true, &overlays)
                .run_with_frontend(Box::new(Front(Messages::default()))),
        );
        assert!(
            matches!(
                result,
                Err(CommandError::WorkItem(
                    awman::command::work_item::WorkItemError::ReadOnlyContext
                ))
            ),
            "{overlays:?}: {result:?}"
        );
        assert_eq!(docker_runs(w), 0, "{overlays:?}: nothing was launched");
        assert!(
            walk(&w._sandbox.awman_home().join("context")).is_empty(),
            "{overlays:?}: nothing was staged"
        );
        assert!(std::fs::read_to_string(original).unwrap().contains("old"));
    }
}
