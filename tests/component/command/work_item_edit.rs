//! Creating and editing work items (WI 0147): `new spec` and `specs amend`
//! against local files and a GitHub source, through the production commands,
//! resolver, provider and runtime. The agent is a fake `docker run` that holds
//! until the test releases it; GitHub is a fake `gh`. Nothing reaches a real
//! tracker, container or host agent.
use crate::support;
use awman::command::commands::new::{
    NewCommand, NewCommandFrontend, NewOutcome, NewSpecFlags, NewSubcommand,
};
use awman::command::commands::prompt_templates::{render_amend_prompt, render_interview_prompt};
use awman::command::commands::specs::{
    SpecsAmendFlags, SpecsCommand, SpecsCommandFrontend, SpecsSubcommand,
};
use awman::command::commands::{
    agent_auth::*, agent_setup::*, agent_trust::AgentTrustFrontend, mount_scope::*, Command,
};
use awman::command::dispatch::Engines;
use awman::command::error::CommandError;
use awman::command::work_item::WorkItemError;
use awman::data::message::{MessageLevel, UserMessage, UserMessageSink};
use awman::data::session::{AgentName, Session, SessionOpenOptions};
use awman::engine::agent_runtime::frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus};
use awman_test_support::fakes::FakeCli;
use awman_test_support::fixtures::{ConfigTree, RepoBuilder};
use awman_test_support::TestSandbox;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ISSUE_URL: &str = "https://github.com/Acme/Widgets/issues/73";
const TEMPLATE: &str = "# Work Item: {{kind}}\n\nTitle: x\nNumber: [{{number}}]\n\n- summary\n";
const LIMIT: Duration = Duration::from_secs(15);
/// A title that makes the test frontend dismiss the title question.
const ABORT: &str = "<dismissed>";

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Message(MessageLevel, String),
    Pty(bool),
}

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<Event>>>);
impl Log {
    fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }
    fn messages(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Message(_, text) => Some(text),
                Event::Pty(_) => None,
            })
            .collect()
    }
    fn all_text(&self) -> String {
        self.messages().join("\n")
    }
    /// The draft's host path from the pre-launch message.
    fn draft_path(&self) -> Option<PathBuf> {
        self.messages()
            .iter()
            .find_map(|m| m.strip_prefix("Work item draft: ").map(PathBuf::from))
    }
}

/// A command frontend that records messages and terminal ownership, answers
/// the Q&A with fixed values, and gives the agent piped (non-PTY) I/O.
struct Front {
    log: Log,
    title: String,
}
impl UserMessageSink for Front {
    fn write_message(&mut self, msg: UserMessage) {
        self.log
            .0
            .lock()
            .unwrap()
            .push(Event::Message(msg.level, msg.text));
    }
    fn replay_queued(&mut self) {}
}
impl MountScopeFrontend for Front {
    fn ask_mount_scope(&mut self, _: &Path, _: &Path) -> Result<MountScopeDecision, CommandError> {
        Ok(MountScopeDecision::MountGitRoot)
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
    fn set_pty_active(&mut self, active: bool) {
        self.log.0.lock().unwrap().push(Event::Pty(active));
    }
}
impl SpecsCommandFrontend for Front {
    fn ask_spec_title(&mut self) -> Result<String, CommandError> {
        // A dismissed dialog (Ctrl+C) surfaces as `Aborted`.
        if self.title == ABORT {
            return Err(CommandError::Aborted);
        }
        Ok(self.title.clone())
    }
    fn ask_spec_summary(&mut self) -> Result<String, CommandError> {
        Ok("the summary".into())
    }
}
impl NewCommandFrontend for Front {
    fn ask_workflow_step_name(&mut self) -> Result<String, CommandError> {
        Ok("one".into())
    }
    fn ask_workflow_step_prompt(&mut self) -> Result<String, CommandError> {
        Ok("prompt".into())
    }
}

/// Headless agent I/O: output is discarded, stdin stays open.
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
        let (stdout, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stderr, mut err_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move { while out_rx.recv().await.is_some() {} });
        tokio::spawn(async move { while err_rx.recv().await.is_some() {} });
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
    sandbox: TestSandbox,
    repo: PathBuf,
    session: Session,
    engines: Engines,
    docker: FakeCli,
    gh: FakeCli,
}

impl World {
    fn front(&self, title: &str) -> (Box<Front>, Log) {
        let log = Log::default();
        (
            Box::new(Front {
                log: log.clone(),
                title: title.into(),
            }),
            log,
        )
    }

    fn new_spec(&self, interview: bool, work_item: Option<&str>) -> NewCommand {
        NewCommand::new(
            NewSubcommand::Spec(NewSpecFlags {
                interview,
                non_interactive: true,
                work_item: work_item.map(str::to_string),
            }),
            self.engines.clone(),
            self.session.clone(),
        )
    }

    fn amend(&self, work_item: &str) -> SpecsCommand {
        SpecsCommand::new(
            SpecsSubcommand::Amend(SpecsAmendFlags {
                work_item: work_item.into(),
                non_interactive: true,
                allow_docker: false,
            }),
            self.engines.clone(),
            self.session.clone(),
        )
    }

    fn work_item_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.repo.join("aspec/work-items"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn gh_calls(&self, sub: &str) -> Vec<awman_test_support::fakes::Call> {
        self.gh
            .calls()
            .into_iter()
            .filter(|c| c.argv.get(1).map(String::as_str) == Some(sub))
            .collect()
    }

    fn docker_runs(&self) -> Vec<awman_test_support::fakes::Call> {
        self.docker
            .calls()
            .into_iter()
            .filter(|c| c.argv.first().map(String::as_str) == Some("run"))
            .collect()
    }

    /// Drafts left in the repo context directory.
    fn drafts(&self) -> Vec<PathBuf> {
        walk(&self.sandbox.awman_home().join("context"))
            .into_iter()
            .filter(|p| {
                p.parent()
                    .is_some_and(|d| d.ends_with("scratch/work-items"))
            })
            .collect()
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// A checkout with a work item template, `origin` on GitHub, the given repo
/// config, a fake `gh` and a fake Docker whose `run` holds until released.
fn world(id: &'static str, repo_config: serde_json::Value, files: &[(&str, &str)]) -> World {
    let mut sandbox = support::sandbox(id);
    let mut builder = RepoBuilder::new(&sandbox, "repo")
        .file("aspec/work-items/0000-template.md", TEMPLATE)
        .file(
            "aspec/work-items/0003-existing.md",
            "# Existing\n\nlocal 3\n",
        );
    for (path, content) in files {
        builder = builder.file(path, content);
    }
    let repo = builder.init();
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/Acme/Widgets.git",
    ]);
    let root = repo.path().to_path_buf();
    ConfigTree::new()
        .global(
            json!({"runtime":{"kind":"docker"},"agents":{"auth":{"refresh":{"enabled":false}}}}),
        )
        .repo(repo_config)
        .write(&sandbox, Some(&root));
    let docker = sandbox.install_fake(support::fake_cli_exe(), "docker");
    // Anything the runtime asks besides `run` succeeds quietly.
    docker.on(&[]);
    awman_test_support::fakes::docker_cli_baseline(&docker);
    docker
        .on(&["image", "inspect"])
        .stdout("[{\"Config\":{\"Env\":[\"HOME=/home/agent\"]}}]\n");
    docker.on(&["run"]).hold_until_released();
    let gh = sandbox.install_fake(support::fake_cli_exe(), "gh");
    gh.on(&["issue", "create"])
        .read_stdin()
        .stdout(&format!("{ISSUE_URL}\n"));
    gh.on(&["issue", "edit"]).read_stdin();
    gh.on(&["issue", "view"]).stdout(&issue_json("raw body\n"));
    // Host CLI isolation admits a fake only inside the snapshot's temp root;
    // the sandbox's fake binaries sit beside its scratch directory.
    let temp_root = sandbox.root().display().to_string();
    sandbox.env_mut().set("TMPDIR", &temp_root);
    let env = support::env_snapshot(&sandbox);
    let session = Session::open_at_git_root(
        root.clone(),
        root.clone(),
        SessionOpenOptions {
            env: Some(env.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    // The bundle resolves docker from the process PATH; launch through the
    // sandbox's fake instead, with the production agent engine.
    let mut engines = Engines::for_env(env.clone()).unwrap();
    let docker_runtime = Arc::new(awman::engine::container::ContainerRuntime::docker_in(env));
    engines.runtime = docker_runtime.clone();
    engines.container_runtime = Some(docker_runtime.clone());
    engines.agent_engine = Arc::new(awman::engine::agent::AgentEngine::new(
        engines.overlay_engine.clone(),
        docker_runtime,
    ));
    World {
        sandbox,
        repo: root,
        session,
        engines,
        docker,
        gh,
    }
}

fn issue_json(body: &str) -> String {
    json!({"number": 73, "title": "Widget title", "body": body, "url": ISSUE_URL}).to_string()
}

fn github() -> serde_json::Value {
    json!({"workItems": {"source": "github"}})
}

/// Wait (off the async runtime) until the fake agent is running.
async fn agent_running<T: std::fmt::Debug>(
    docker: &FakeCli,
    task: &mut tokio::task::JoinHandle<T>,
) {
    let calls = docker.clone();
    let docker = docker.clone();
    let held = tokio::task::spawn_blocking(move || docker.wait_until_held(LIMIT));
    tokio::select! {
        held = held => { held.unwrap(); }
        done = task => panic!("the command finished before the agent ran: {done:?}; docker calls: {:#?}", calls.calls()),
    }
}

async fn finish<T: Send + 'static>(task: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(LIMIT, task)
        .await
        .expect("command finished")
        .unwrap()
}

/// The pre-launch draft message precedes terminal hand-off to the agent.
fn assert_path_before_launch(log: &Log) {
    let events = log.events();
    let path = events
        .iter()
        .position(|e| matches!(e, Event::Message(_, t) if t.starts_with("Work item draft: ")))
        .expect("draft path message");
    let pty = events
        .iter()
        .position(|e| *e == Event::Pty(true))
        .expect("agent launched");
    assert!(path < pty, "{events:#?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn github_new_spec_creates_an_issue_without_a_file_or_number() {
    let w = world(
        "command::work_item_edit::github_new_spec_creates_an_issue_without_a_file_or_number",
        github(),
        &[],
    );
    let before = w.work_item_files();
    let (front, log) = w.front("-dash title");
    let outcome = w
        .new_spec(false, None)
        .run_with_frontend(front)
        .await
        .unwrap();
    let NewOutcome::Spec(outcome) = outcome else {
        panic!("expected spec")
    };
    assert_eq!(outcome.path, None);
    assert_eq!(outcome.url.as_deref(), Some(ISSUE_URL));
    assert_eq!(outcome.number, Some(73));
    assert_eq!(w.work_item_files(), before, "no local file or number");
    let creates = w.gh_calls("create");
    assert_eq!(creates.len(), 1);
    assert!(
        creates[0].argv.contains(&"--title=-dash title".to_string()),
        "{:?}",
        creates[0].argv
    );
    let body = String::from_utf8(creates[0].stdin.clone()).unwrap();
    assert!(body.contains("Number: []"), "{body}");
    assert!(body.contains("- the summary"), "{body}");
    assert!(log.all_text().contains(ISSUE_URL), "{}", log.all_text());
    assert!(w.docker_runs().is_empty(), "no agent without an interview");
    // The destination is named before anything is published.
    assert!(
        log.all_text()
            .contains("creating a github work item in github:acme/widgets"),
        "{}",
        log.all_text()
    );
    assert!(w.drafts().is_empty(), "the recovery draft is removed");
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn interview_creates_the_issue_from_the_draft_even_after_a_failed_agent() {
    let w = world(
        "command::work_item_edit::interview_creates_the_issue_from_the_draft_even_after_a_failed_agent",
        github(),
        &[],
    );
    w.docker.on(&["run"]).hold_until_released().exit(3);
    let before = w.work_item_files();
    let (front, log) = w.front("Widget title");
    let command = w.new_spec(true, None);
    let mut task = tokio::spawn(async move { command.run_with_frontend(front).await });
    agent_running(&w.docker, &mut task).await;
    assert_path_before_launch(&log);
    let draft = log.draft_path().expect("draft path");
    assert!(
        draft.starts_with(w.sandbox.awman_home().join("context/repo")),
        "{draft:?}"
    );
    let seeded = std::fs::read_to_string(&draft).unwrap();
    assert!(seeded.contains("Number: []"), "{seeded}");
    // The agent sees the draft through a read-write context(repo) mount.
    let run = &w.docker_runs()[0];
    let repo_dir = draft.parent().unwrap().parent().unwrap().parent().unwrap();
    let mount = format!("{}:/awman/context/repo", repo_dir.display());
    assert!(
        run.argv
            .iter()
            .any(|a| *a == mount || *a == format!("{mount}:rw")),
        "{:?}",
        run.argv
    );
    assert!(
        w.gh_calls("create").is_empty(),
        "nothing is created before an edit"
    );
    std::fs::write(&draft, "# Widget\n\nwritten by the agent\n").unwrap();
    w.docker.release();
    let result = finish(task).await;
    assert!(result.is_err(), "a failed agent still fails the command");
    let creates = w.gh_calls("create");
    assert_eq!(creates.len(), 1, "exactly one create");
    assert!(creates[0]
        .argv
        .contains(&"--title=Widget title".to_string()));
    assert_eq!(creates[0].stdin, b"# Widget\n\nwritten by the agent\n");
    assert!(log.all_text().contains(ISSUE_URL), "{}", log.all_text());
    assert!(
        !draft.exists(),
        "the draft is removed after the final upload"
    );
    assert_eq!(w.work_item_files(), before);
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn amend_uploads_the_raw_body_once_and_removes_the_draft() {
    let w = world(
        "command::work_item_edit::amend_uploads_the_raw_body_once_and_removes_the_draft",
        github(),
        &[],
    );
    let (front, log) = w.front("unused");
    let command = w.amend("73");
    let mut task = tokio::spawn(async move { command.run_with_frontend(front).await });
    agent_running(&w.docker, &mut task).await;
    assert_path_before_launch(&log);
    assert!(
        log.all_text().contains(ISSUE_URL),
        "URL printed before launch"
    );
    let draft = log.draft_path().unwrap();
    // Seeded from the raw body: no `# <title>` heading is added.
    assert_eq!(std::fs::read_to_string(&draft).unwrap(), "raw body\n");
    std::fs::write(
        &draft,
        "raw body\n\n## Agent implementation notes\n\ndone\n",
    )
    .unwrap();
    w.docker.release();
    finish(task).await.unwrap();
    let edits = w.gh_calls("edit");
    assert_eq!(edits.len(), 1, "{edits:?}");
    let body = String::from_utf8(edits[0].stdin.clone()).unwrap();
    assert_eq!(body, "raw body\n\n## Agent implementation notes\n\ndone\n");
    assert!(!body.starts_with("# "), "no title heading is uploaded");
    assert!(!draft.exists());
    assert!(w.drafts().is_empty());
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn amend_with_an_unchanged_draft_makes_no_upload() {
    let w = world(
        "command::work_item_edit::amend_with_an_unchanged_draft_makes_no_upload",
        github(),
        &[],
    );
    let (front, log) = w.front("unused");
    let command = w.amend("github:73");
    let mut task = tokio::spawn(async move { command.run_with_frontend(front).await });
    agent_running(&w.docker, &mut task).await;
    let draft = log.draft_path().unwrap();
    w.docker.release();
    finish(task).await.unwrap();
    assert!(w.gh_calls("edit").is_empty());
    assert!(!draft.exists());
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn amend_never_overwrites_a_concurrent_remote_change() {
    let w = world(
        "command::work_item_edit::amend_never_overwrites_a_concurrent_remote_change",
        github(),
        &[],
    );
    let (front, log) = w.front("unused");
    let command = w.amend("73");
    let mut task = tokio::spawn(async move { command.run_with_frontend(front).await });
    agent_running(&w.docker, &mut task).await;
    let draft = log.draft_path().unwrap();
    // Someone edits the issue on the tracker while the agent runs.
    w.gh.on(&["issue", "view"])
        .stdout(&issue_json("edited on the tracker\n"));
    std::fs::write(&draft, "agent edit\n").unwrap();
    w.docker.release();
    let error = finish(task).await.unwrap_err();
    assert!(matches!(error, CommandError::DraftSync(_)), "{error}");
    assert!(w.gh_calls("edit").is_empty(), "the remote change is kept");
    assert!(draft.exists(), "the draft is kept for recovery");
    assert!(
        log.all_text().contains(&draft.display().to_string()),
        "{}",
        log.all_text()
    );
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_repo_context_stops_before_launch() {
    let mut config = github();
    config["overlays"] = json!(["context(repo:ro)"]);
    let w = world(
        "command::work_item_edit::read_only_repo_context_stops_before_launch",
        config,
        &[],
    );
    let (front, log) = w.front("unused");
    let error = w.amend("73").run_with_frontend(front).await.unwrap_err();
    assert!(
        matches!(
            error,
            CommandError::WorkItem(WorkItemError::ReadOnlyContext)
        ),
        "{error}"
    );
    assert!(w.docker_runs().is_empty());
    assert!(!log.events().contains(&Event::Pty(true)));
    assert!(w.drafts().is_empty());
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn ambiguous_local_amend_lists_every_match_before_launch() {
    let w = world(
        "command::work_item_edit::ambiguous_local_amend_lists_every_match_before_launch",
        json!({}),
        &[
            ("aspec/work-items/0042-first.md", "# First\n"),
            ("aspec/work-items/0042-second.md", "# Second\n"),
        ],
    );
    let (front, log) = w.front("unused");
    let error = w.amend("0042").run_with_frontend(front).await.unwrap_err();
    assert!(
        matches!(
            &error,
            CommandError::WorkItem(WorkItemError::AmbiguousWorkItem { number: 42, paths }) if paths.len() == 2
        ),
        "{error}"
    );
    let text = log.all_text();
    assert!(
        text.contains("0042-first.md") && text.contains("0042-second.md"),
        "{text}"
    );
    assert!(w.docker_runs().is_empty());
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn seeding_from_the_configured_source_suggests_amend() {
    let w = world(
        "command::work_item_edit::seeding_from_the_configured_source_suggests_amend",
        github(),
        &[],
    );
    let (front, _log) = w.front("unused");
    let error = w
        .new_spec(false, Some("#73"))
        .run_with_frontend(front)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            CommandError::WorkItem(WorkItemError::AlreadyExists { .. })
        ),
        "{error}"
    );
    assert!(error.to_string().contains("specs amend"), "{error}");
    assert!(w.gh_calls("create").is_empty());
    assert!(w.docker_runs().is_empty());
    w.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn local_new_spec_keeps_numbered_files() {
    let w = world(
        "command::work_item_edit::local_new_spec_keeps_numbered_files",
        json!({}),
        &[],
    );
    let (front, _log) = w.front("Local thing");
    let outcome = w
        .new_spec(false, None)
        .run_with_frontend(front)
        .await
        .unwrap();
    let NewOutcome::Spec(outcome) = outcome else {
        panic!("expected spec")
    };
    let path = PathBuf::from(outcome.path.unwrap());
    assert!(
        path.ends_with("aspec/work-items/0004-local-thing.md"),
        "{path:?}"
    );
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("Number: [0004]"));
    assert_eq!((outcome.url, outcome.number), (None, Some(4)));
    assert!(w.gh.calls().is_empty());
    w.sandbox.finish();
}

#[test]
fn prompts_name_the_file_and_never_expand_work_item_text() {
    let interview = render_interview_prompt(
        "/awman/context/repo/scratch/work-items/x.md",
        "Bug",
        "{summary}",
        "body with {path}",
    );
    assert!(interview.contains("/awman/context/repo/scratch/work-items/x.md"));
    assert!(interview.contains("\"{summary}\""), "{interview}");
    assert!(interview.ends_with("body with {path}"), "{interview}");
    assert!(interview.contains("Re-read the file"));
    let amend = render_amend_prompt("/workspace/aspec/work-items/0003-x.md", "Title");
    assert!(amend.contains("/workspace/aspec/work-items/0003-x.md"));
    assert!(amend.contains("\"Title\""));
    assert!(amend.contains("Re-read the file"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dismissed_question_never_publishes_a_work_item() {
    let w = world(
        "command::work_item_edit::a_dismissed_question_never_publishes_a_work_item",
        github(),
        &[],
    );
    let before = w.work_item_files();
    for interview in [false, true] {
        let (front, _log) = w.front(ABORT);
        let result = w.new_spec(interview, None).run_with_frontend(front).await;
        assert!(
            matches!(result, Err(CommandError::Aborted)),
            "{:?}",
            result.err()
        );
    }
    assert!(w.gh_calls("create").is_empty(), "nothing was published");
    assert!(w.docker_runs().is_empty(), "no agent was started");
    assert!(w.drafts().is_empty());
    assert_eq!(w.work_item_files(), before);
    w.sandbox.finish();
}
