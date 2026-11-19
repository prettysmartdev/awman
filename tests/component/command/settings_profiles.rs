//! Lifecycle policy with a controlled runtime; no host provider or daemon.
use awman::{
    command::{commands::launch_policy::settings_profile::*, error::CommandError},
    data::{
        fs::{agent_profiles::*, private_files::create_private_dir, DataPaths},
        session::{AgentHandle, AgentName, Session, SessionOpenOptions},
        settings_profile::{NativeSessionId, ProfileState, SettingsProfileName},
    },
    engine::{
        agent::{first_party::first_party, AgentRunOptions},
        agent_runtime::{
            AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities, ResolvedAgentOptions,
        },
        error::EngineError,
    },
};
use awman_test_support::TestSandbox;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Default)]
struct Resources {
    handle: Option<AgentHandle>,
    label: Option<String>,
    running: bool,
    unavailable: bool,
    stops: usize,
    removes: usize,
}
pub(super) struct Runtime {
    caps: Capabilities,
    resources: Mutex<Resources>,
}
impl Runtime {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            caps: Capabilities {
                arbitrary_host_mounts: true,
                kit_declarative: false,
                has_image_store: true,
                image_acquisition: awman::engine::agent_runtime::ImageAcquisition::Build,
                per_launch_vm_settings: false,
                reports_resource_stats: false,
            },
            resources: Default::default(),
        })
    }
}
impl AgentRuntimeEngine for Runtime {
    fn runtime_name(&self) -> &'static str {
        "profile-test"
    }
    fn display_name(&self) -> &'static str {
        "profile test"
    }
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
    fn is_available(&self) -> bool {
        !self.resources.lock().unwrap().unavailable
    }
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        self.list_running_all()
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        let r = self.resources.lock().unwrap();
        Ok(if r.running {
            r.handle.clone().into_iter().collect()
        } else {
            vec![]
        })
    }
    fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
        let r = self.resources.lock().unwrap();
        Ok(if !r.running {
            r.handle.clone().into_iter().collect()
        } else {
            vec![]
        })
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        self.list_running_all()
    }
    fn resource_label(&self, _: &AgentHandle, _: &str) -> Result<Option<String>, EngineError> {
        Ok(self.resources.lock().unwrap().label.clone())
    }
    fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
        let mut r = self.resources.lock().unwrap();
        r.stops += 1;
        r.running = false;
        Ok(())
    }
    fn remove_agent(&self, _: &str) -> Result<(), EngineError> {
        let mut r = self.resources.lock().unwrap();
        r.removes += 1;
        r.handle = None;
        Ok(())
    }
    fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
        panic!("no stats")
    }
    fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("no attach")
    }
    fn build(&self, _: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("no provider starts in lifecycle tests")
    }
    fn ready_agent(
        &self,
        _: &str,
        _: awman::engine::agent_runtime::ReadyAgentOptions,
        _: &mut dyn awman::data::message::UserMessageSink,
    ) -> Result<(), EngineError> {
        panic!("no image setup")
    }
    fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        Ok(Some("/root".into()))
    }
    fn build_image(
        &self,
        _: &awman::engine::container::BuildRequest,
        _: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        panic!("no build")
    }
}
pub(super) fn session(s: &TestSandbox) -> Session {
    Session::open_at_git_root(
        s.work(),
        s.work(),
        SessionOpenOptions {
            env: Some(crate::support::env_snapshot(s)),
            ..Default::default()
        },
    )
    .unwrap()
}
fn request(name: &str, id: Option<&str>) -> ProfileRequest {
    ProfileRequest::parse(Some(name), id).unwrap()
}
fn run() -> AgentRunOptions {
    AgentRunOptions {
        non_interactive: true,
        initial_prompt: Some("follow up".into()),
        ..Default::default()
    }
}
fn layout(session: &Session, name: &str) -> ProfileLayout {
    ProfileLayout::new(
        &DataPaths::from_env(session.env()).unwrap(),
        &ProfileRepoBinding::from_session(session).unwrap(),
        &AgentName::new("claude").unwrap(),
        &SettingsProfileName::parse(name).unwrap(),
    )
}
async fn start(session: &Session, name: &str, runtime: Arc<Runtime>) -> ProfileSession {
    enter(
        session,
        &AgentName::new("claude").unwrap(),
        &request(name, None),
        &run(),
        &first_party("claude").unwrap(),
        runtime,
    )
    .await
    .unwrap()
    .unwrap()
}
fn transcript(p: &ProfileSession, id: &str) {
    let dir = p
        .run_state()
        .run_dir
        .join(".claude/projects/workspace-tokenizer");
    create_private_dir(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{id}.jsonl")),
        format!("{{\"sessionId\":\"{id}\",\"type\":\"assistant\"}}\n"),
    )
    .unwrap();
}
#[tokio::test]
async fn contention_retention_two_profiles_under_one_home_and_exact_resume() {
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    create_private_dir(&s.home().join(".claude")).unwrap();
    std::fs::write(
        s.home().join(".claude/settings.json"),
        r#"{"model":"first","permissions":{"deny":["Bash(rm *)"]},"env":{"TOKEN":"canary"}}"#,
    )
    .unwrap();
    let a = start(&session, "a", runtime.clone()).await;
    let b = start(&session, "b", runtime.clone()).await;
    assert_ne!(a.run_state().run_dir, b.run_state().run_dir);
    assert!(matches!(
        enter(
            &session,
            &AgentName::new("claude").unwrap(),
            &request("a", None),
            &run(),
            &first_party("claude").unwrap(),
            runtime.clone()
        )
        .await,
        Err(CommandError::SettingsProfileBusy(_))
    ));
    transcript(&a, "11111111-1111-4111-8111-111111111111");
    std::fs::write(
        a.run_state().run_dir.join(".claude/.credentials.json"),
        "credential-canary",
    )
    .unwrap();
    let m = a
        .leave(runtime.clone(), TurnStateAtExit::Complete)
        .await
        .unwrap();
    assert_eq!(m.state, ProfileState::Inactive, "cleanup: {:?}", m.cleanup);
    assert_eq!(m.native_sessions.len(), 1);
    b.leave(runtime.clone(), TurnStateAtExit::Unknown)
        .await
        .unwrap();
    let lease = layout(&session, "a")
        .try_lock(Duration::ZERO)
        .unwrap()
        .unwrap();
    let tree = lease.generation_tree(m.current_generation);
    assert!(!tree.join(".claude/.credentials.json").exists());
    let text = std::fs::read_to_string(tree.join(".claude/settings.json")).unwrap();
    assert!(!text.contains("canary"));
    assert!(text.contains("Bash(rm *)"));
    drop(lease);
    std::fs::write(
        s.home().join(".claude/settings.json"),
        r#"{"model":"changed"}"#,
    )
    .unwrap();
    let id = "11111111-1111-4111-8111-111111111111";
    let resumed = enter(
        &session,
        &AgentName::new("claude").unwrap(),
        &request("a", Some(id)),
        &run(),
        &first_party("claude").unwrap(),
        runtime.clone(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        std::fs::read_to_string(resumed.run_state().run_dir.join(".claude/settings.json"))
            .unwrap()
            .contains("first")
    );
    resumed
        .leave(runtime.clone(), TurnStateAtExit::Complete)
        .await
        .unwrap();
    assert!(matches!(
        enter(
            &session,
            &AgentName::new("claude").unwrap(),
            &request("b", Some(id)),
            &run(),
            &first_party("claude").unwrap(),
            runtime
        )
        .await,
        Err(CommandError::NativeSessionNotFound)
    ));
}
#[tokio::test]
async fn crash_free_lock_never_authorizes_a_second_writer_and_close_is_idempotent() {
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    let p = start(&session, "review", runtime.clone()).await;
    let id = p.execution_id().clone();
    let name = format!("awman-profile-{}", id.as_str());
    {
        let mut r = runtime.resources.lock().unwrap();
        r.handle = Some(AgentHandle {
            id: "resource".into(),
            name,
            image_tag: "test".into(),
            started_at: chrono::Utc::now(),
        });
        r.running = true;
        r.label = Some(id.as_str().into());
    }
    drop(p);
    assert!(matches!(
        enter(
            &session,
            &AgentName::new("claude").unwrap(),
            &request("review", None),
            &run(),
            &first_party("claude").unwrap(),
            runtime.clone()
        )
        .await,
        Err(CommandError::SettingsProfileBusy(_))
    ));
    assert_eq!(runtime.resources.lock().unwrap().stops, 0);
    let lease = layout(&session, "review")
        .try_lock(Duration::ZERO)
        .unwrap()
        .unwrap();
    let m = lease.read_manifest().unwrap().unwrap();
    assert_eq!(m.state, ProfileState::Closing);
    assert!(lease
        .remove_run_dir(&awman::data::ids::ExecutionId::new())
        .is_ok());
    let spec = first_party("claude").unwrap().profile_state.unwrap();
    let m = reconcile(
        &lease,
        m,
        ReconcileMode::StopOwned,
        runtime.clone(),
        &spec,
        TurnStateAtExit::Unknown,
    )
    .await
    .unwrap();
    assert_eq!(m.state, ProfileState::Inactive);
    assert_eq!(runtime.resources.lock().unwrap().stops, 1);
    let generation = m.current_generation;
    let again = reconcile(
        &lease,
        m,
        ReconcileMode::StopOwned,
        runtime.clone(),
        &spec,
        TurnStateAtExit::Unknown,
    )
    .await
    .unwrap();
    assert_eq!(again.current_generation, generation);
    assert_eq!(runtime.resources.lock().unwrap().stops, 1);
    lease.remove_profile().unwrap();
}
#[tokio::test]
async fn restart_after_cleanup_before_inactive_and_unavailable_runtime() {
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    let p = start(&session, "review", runtime.clone()).await;
    let id = p.execution_id().clone();
    drop(p);
    let l = layout(&session, "review");
    let lease = l.try_lock(Duration::ZERO).unwrap().unwrap();
    let mut m = lease.read_manifest().unwrap().unwrap();
    m.state = ProfileState::Closing;
    lease.write_manifest(&m).unwrap();
    lease.remove_run_dir(&id).unwrap();
    drop(lease);
    runtime.resources.lock().unwrap().unavailable = true;
    assert!(enter(
        &session,
        &AgentName::new("claude").unwrap(),
        &request("review", None),
        &run(),
        &first_party("claude").unwrap(),
        runtime.clone()
    )
    .await
    .is_err());
    let lease = l.try_lock(Duration::ZERO).unwrap().unwrap();
    assert_eq!(
        lease.read_manifest().unwrap().unwrap().state,
        ProfileState::Closing
    );
    assert!(lease.remove_profile().is_err());
    runtime.resources.lock().unwrap().unavailable = false;
    let p = start(&session, "review", runtime.clone()).await;
    p.leave(runtime, TurnStateAtExit::Unknown).await.unwrap();
}

#[tokio::test]
async fn crash_after_generation_commit_never_reexports_partial_run_tree() {
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    let p = start(&session, "review", runtime.clone()).await;
    let id = p.execution_id().clone();
    drop(p);
    let lease = layout(&session, "review")
        .try_lock(Duration::ZERO)
        .unwrap()
        .unwrap();
    let mut m = lease.read_manifest().unwrap().unwrap();
    let base_generation = m.owner.as_ref().unwrap().base_generation;
    let owner_name = m.owner.as_ref().unwrap().resource_name.clone();
    runtime.resources.lock().unwrap().handle = Some(AgentHandle {
        id: "resource".into(),
        name: owner_name,
        image_tag: "test".into(),
        started_at: chrono::Utc::now(),
    });
    runtime.resources.lock().unwrap().label = Some(id.as_str().into());
    let next = m.current_generation + 1;
    m.current_generation = next;
    m.state = ProfileState::Closing;
    let stage = lease.begin_staging().unwrap();
    awman::data::fs::private_files::write_private_json_atomic(
        &stage.join("tree/complete.jsonl"),
        &"complete history",
        || Ok(()),
    )
    .unwrap();
    awman::data::fs::private_files::write_private_json_atomic(
        &stage.join("snapshot.json"),
        &serde_json::json!({}),
        || Ok(()),
    )
    .unwrap();
    lease.publish(stage, &m).unwrap();
    // Model a crash partway through recursive cleanup after publication.
    let partial = lease.run_dir(&id).join(".claude/partial.jsonl");
    std::fs::write(&partial, "partial").unwrap();
    std::fs::remove_file(partial).unwrap();
    let spec = first_party("claude").unwrap().profile_state.unwrap();
    let recovered = reconcile(
        &lease,
        lease.read_manifest().unwrap().unwrap(),
        ReconcileMode::StopOwned,
        runtime,
        &spec,
        TurnStateAtExit::Unknown,
    )
    .await
    .unwrap();
    assert_eq!(recovered.current_generation, next);
    assert!(recovered.current_generation > base_generation);
    assert_eq!(
        recovered.state,
        ProfileState::Inactive,
        "cleanup: {:?}",
        recovered.cleanup
    );
    assert!(!lease.run_dir(&id).exists());
    assert!(lease.generation_tree(next).join("complete.jsonl").exists());
}

#[tokio::test]
async fn runtime_listing_miss_keeps_profile_closing() {
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    let p = start(&session, "review", runtime.clone()).await;
    let id = p.execution_id().clone();
    runtime.resources.lock().unwrap().handle = Some(AgentHandle {
        id: "backend-id".into(),
        name: "runtime-renamed-resource".into(),
        image_tag: "test".into(),
        started_at: chrono::Utc::now(),
    });
    runtime.resources.lock().unwrap().label = Some(id.as_str().into());
    runtime.resources.lock().unwrap().running = true;
    drop(p);
    let lease = layout(&session, "review")
        .try_lock(Duration::ZERO)
        .unwrap()
        .unwrap();
    let spec = first_party("claude").unwrap().profile_state.unwrap();
    let m = reconcile(
        &lease,
        lease.read_manifest().unwrap().unwrap(),
        ReconcileMode::VerifyOnly,
        runtime.clone(),
        &spec,
        TurnStateAtExit::Unknown,
    )
    .await
    .unwrap();
    assert_eq!(m.state, ProfileState::Closing);
    assert_eq!(m.cleanup.unwrap().reason, CleanupReason::LivenessUnknown);
    assert_eq!(runtime.resources.lock().unwrap().stops, 0);
}

#[tokio::test]
async fn missing_definition_close_keeps_previous_generation() {
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    let p = start(&session, "review", runtime.clone()).await;
    let id = p.execution_id().clone();
    let owner_name = format!("awman-profile-{}", id.as_str());
    runtime.resources.lock().unwrap().handle = Some(AgentHandle {
        id: "resource".into(),
        name: owner_name,
        image_tag: "test".into(),
        started_at: chrono::Utc::now(),
    });
    runtime.resources.lock().unwrap().label = Some(id.as_str().into());
    drop(p);
    let lease = layout(&session, "review")
        .try_lock(Duration::ZERO)
        .unwrap()
        .unwrap();
    let m = reconcile_retaining_previous(
        &lease,
        lease.read_manifest().unwrap().unwrap(),
        ReconcileMode::StopOwned,
        runtime,
    )
    .await
    .unwrap();
    assert_eq!(m.state, ProfileState::Inactive);
    assert_eq!(m.current_generation, 1);
    assert!(matches!(
        m.last_publication.unwrap().status,
        PublicationStatus::RetainedPrevious
    ));
    assert!(!lease.run_dir(&id).exists());
}

#[cfg(unix)]
#[tokio::test]
async fn unsafe_unrelated_profile_directory_does_not_block_admission() {
    let s = TestSandbox::new();
    let session = session(&s);
    let paths = DataPaths::from_env(session.env()).unwrap();
    let root = paths.agent_profiles_dir(session.git_root());
    create_private_dir(&root).unwrap();
    let outside = s.tmp().join("foreign-profile-state");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(outside, root.join("codex")).unwrap();
    let runtime = Runtime::new();
    let p = enter(
        &session,
        &AgentName::new("claude").unwrap(),
        &request("review", None),
        &run(),
        &first_party("claude").unwrap(),
        runtime.clone(),
    )
    .await
    .unwrap()
    .unwrap();
    p.leave(runtime, TurnStateAtExit::Unknown).await.unwrap();
}

#[test]
fn native_ids_are_taken_only_from_provider_transcript_paths() {
    let s = TestSandbox::new();
    let root = s.tmp().join("history-tree");
    let claude = first_party("claude").unwrap().profile_state.unwrap();
    let project = root.join(".claude/projects/project");
    std::fs::create_dir_all(&project).unwrap();
    let id = "11111111-1111-4111-8111-111111111111";
    std::fs::write(
        root.join(".claude/history.jsonl"),
        format!("{{\"sessionId\":\"{id}\"}}\n"),
    )
    .unwrap();
    std::fs::write(
        project.join(format!("{id}.jsonl")),
        "{\"type\":\"user\",\"message\":{}}\n",
    )
    .unwrap();
    assert_eq!(claude.list_sessions(&root).unwrap()[0].as_str(), id);

    let cursor = first_party("cursor").unwrap().profile_state.unwrap();
    let workspace = root.join(".cursor/chats/22222222-2222-4222-8222-222222222222");
    let chat_id = "33333333-3333-4333-8333-333333333333";
    std::fs::create_dir_all(workspace.join(chat_id)).unwrap();
    std::fs::write(workspace.join(chat_id).join("store.db"), b"opaque database").unwrap();
    assert_eq!(cursor.list_sessions(&root).unwrap()[0].as_str(), chat_id);

    let opencode = first_party("opencode").unwrap().profile_state.unwrap();
    let storage = root.join(".local/share/opencode/storage");
    std::fs::create_dir_all(storage.join("message")).unwrap();
    std::fs::create_dir_all(storage.join("session/project")).unwrap();
    std::fs::write(
        storage.join("message/not-a-session.json"),
        "{\"id\":\"wrong\"}\n",
    )
    .unwrap();
    std::fs::write(
        storage.join("session/project/ses_exact.json"),
        "{\"id\":\"message-id\",\"sessionID\":\"ses_exact\"}\n",
    )
    .unwrap();
    assert_eq!(
        opencode.list_sessions(&root).unwrap()[0].as_str(),
        "ses_exact"
    );
}
#[tokio::test]
async fn rejected_snapshot_stays_closing_until_explicit_close_retains_previous() {
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    let p = start(&session, "review", runtime.clone()).await;
    std::fs::write(
        p.run_state().run_dir.join(".claude/settings.json"),
        r#"{"permissions":{"deny":"unknown shape"}}"#,
    )
    .unwrap();
    let m = p
        .leave(runtime.clone(), TurnStateAtExit::Unknown)
        .await
        .unwrap();
    assert_eq!(m.state, ProfileState::Closing);
    assert_eq!(m.current_generation, 1);
    let lease = layout(&session, "review")
        .try_lock(Duration::ZERO)
        .unwrap()
        .unwrap();
    let spec = first_party("claude").unwrap().profile_state.unwrap();
    let m = reconcile(
        &lease,
        m,
        ReconcileMode::StopOwned,
        runtime,
        &spec,
        TurnStateAtExit::Unknown,
    )
    .await
    .unwrap();
    assert_eq!(m.state, ProfileState::Inactive);
    assert!(matches!(
        m.last_publication.unwrap().status,
        PublicationStatus::RetainedPrevious
    ));
    lease.remove_profile().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_close_retains_previous_and_removes_guest_planted_special_file() {
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    let p = start(&session, "review", runtime.clone()).await;
    let id = p.execution_id().clone();
    let owner_name = format!("awman-profile-{}", id.as_str());
    runtime.resources.lock().unwrap().handle = Some(AgentHandle {
        id: "resource".into(),
        name: owner_name,
        image_tag: "test".into(),
        started_at: chrono::Utc::now(),
    });
    runtime.resources.lock().unwrap().label = Some(id.as_str().into());
    let credential = p.run_state().run_dir.join(".claude/.credentials.json");
    std::fs::write(&credential, "private token").unwrap();
    let special = p
        .run_state()
        .run_dir
        .join(".claude/projects/project/history-entry");
    std::fs::create_dir_all(special.parent().unwrap()).unwrap();
    #[cfg(unix)]
    if unsafe { libc::geteuid() } != 0 {
        use std::os::unix::fs::PermissionsExt;
        let unreadable = special.parent().unwrap().join("unreadable.jsonl");
        std::fs::write(&unreadable, "{}\n").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o0)).unwrap();
    }
    use std::os::unix::ffi::OsStrExt;
    let special_c = std::ffi::CString::new(special.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(special_c.as_ptr(), 0o600) }, 0);
    let m = p
        .leave(runtime.clone(), TurnStateAtExit::Unknown)
        .await
        .unwrap();
    assert_eq!(m.state, ProfileState::Closing);
    let lease = layout(&session, "review")
        .try_lock(Duration::ZERO)
        .unwrap()
        .unwrap();
    let spec = first_party("claude").unwrap().profile_state.unwrap();
    let m = reconcile(
        &lease,
        m,
        ReconcileMode::StopOwned,
        runtime,
        &spec,
        TurnStateAtExit::Unknown,
    )
    .await
    .unwrap();
    assert_eq!(m.state, ProfileState::Inactive);
    assert_eq!(m.current_generation, 1);
    assert!(matches!(
        m.last_publication.unwrap().status,
        PublicationStatus::RetainedPrevious
    ));
    assert!(!lease.run_dir(&id).exists());
}
#[tokio::test]
async fn reused_resource_identity_is_left_alone_and_close_delete_launch_contend() {
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    let p = start(&session, "review", runtime.clone()).await;
    let id = p.execution_id().clone();
    drop(p);
    {
        let mut r = runtime.resources.lock().unwrap();
        r.handle = Some(AgentHandle {
            id: "other".into(),
            name: format!("awman-profile-{}", id.as_str()),
            image_tag: "test".into(),
            started_at: chrono::Utc::now(),
        });
        r.running = true;
        r.label = Some("another-execution".into());
    }
    let l = layout(&session, "review");
    let lease = l.try_lock(Duration::ZERO).unwrap().unwrap();
    let m = lease.read_manifest().unwrap().unwrap();
    let spec = first_party("claude").unwrap().profile_state.unwrap();
    let concurrent = l.clone();
    assert!(
        std::thread::spawn(move || concurrent.try_lock(Duration::ZERO).unwrap().is_none())
            .join()
            .unwrap()
    );
    let m = reconcile(
        &lease,
        m,
        ReconcileMode::StopOwned,
        runtime.clone(),
        &spec,
        TurnStateAtExit::Unknown,
    )
    .await
    .unwrap();
    assert_eq!(m.state, ProfileState::Inactive);
    assert_eq!(runtime.resources.lock().unwrap().stops, 0);
    assert_eq!(runtime.resources.lock().unwrap().removes, 0);
    lease.remove_profile().unwrap();
}
#[test]
fn usage_requires_profile_and_unknown_native_flag_is_not_an_alias() {
    assert!(ProfileRequest::parse(None, Some("id")).is_err());
    assert!(awman::command::dispatch::catalogue::CommandCatalogue::get()
        .parse_raw_args(&["chat"], &["--native-session".into(), "id".into()])
        .is_err());
    assert!(NativeSessionId::parse("exact-id").is_ok());
}

fn engines(
    s: &TestSandbox,
    session: &Session,
    runtime: Arc<Runtime>,
) -> awman::command::dispatch::Engines {
    use awman::{
        data::{
            fs::{ApiPaths, AuthPathResolver},
            WorkflowStateStore,
        },
        engine::{agent::AgentEngine, auth::AuthEngine, git::GitEngine, overlay::OverlayEngine},
    };
    let auth = AuthPathResolver::at_home(s.home());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()));
    let env = session.env().clone();
    awman::command::dispatch::Engines {
        runtime: runtime.clone(),
        container_runtime: None,
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::with_env(env.clone())),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(
            auth,
            ApiPaths::from_env(&env).unwrap(),
        )),
        agent_engine: Arc::new(AgentEngine::new(overlay, runtime)),
        workflow_state_store: Arc::new(WorkflowStateStore::new(session)),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
        ephemeral_secrets: Arc::new(Default::default()),
        lease_workflow_review: None,
    }
}
async fn profile_command(
    session: &Session,
    engines: &awman::command::dispatch::Engines,
    action: &str,
    args: &[&str],
) -> Result<serde_json::Value, CommandError> {
    use awman::{
        command::{commands::api_server::event_bus::EventBus, dispatch::Dispatch},
        frontend::api::command_frontend::ApiDispatchFrontend,
    };
    let path = ["agents", "profile", action];
    let args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let bus = EventBus::new(128);
    let frontend = ApiDispatchFrontend::new(&path.join(" "), &args, bus.sender());
    let outcome = Dispatch::new(
        frontend,
        Arc::new(tokio::sync::RwLock::new(session.clone())),
        engines.clone(),
    )
    .with_profile_owner(true)
    .run_command(&path)
    .await?;
    Ok(serde_json::to_value(outcome).unwrap())
}
#[tokio::test]
async fn profile_commands_are_image_independent_and_close_delete_launch_serialize() {
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    let engines = engines(&s, &session, runtime.clone());
    let p = start(&session, "review", runtime.clone()).await;
    let agent = AgentName::new("claude").unwrap();
    let req = request("review", None);
    let options = run();
    let def = first_party("claude").unwrap();
    let (close, delete, launch) = tokio::join!(
        profile_command(
            &session,
            &engines,
            "close",
            &["review", "--agent", "claude"]
        ),
        profile_command(
            &session,
            &engines,
            "delete",
            &["review", "--agent", "claude", "--yes"]
        ),
        enter(&session, &agent, &req, &options, &def, runtime.clone())
    );
    assert!(matches!(close, Err(CommandError::SettingsProfileBusy(_))));
    assert!(matches!(delete, Err(CommandError::SettingsProfileBusy(_))));
    assert!(matches!(launch, Err(CommandError::SettingsProfileBusy(_))));
    p.leave(runtime.clone(), TurnStateAtExit::Unknown)
        .await
        .unwrap();
    runtime.resources.lock().unwrap().unavailable = true;
    let list = profile_command(&session, &engines, "list", &[])
        .await
        .unwrap()
        .to_string();
    assert!(list.contains("review"));
    assert!(!list.contains("root_sha256"));
    assert!(!list.contains(s.home().to_string_lossy().as_ref()));
    let close = profile_command(
        &session,
        &engines,
        "close",
        &["review", "--agent", "claude"],
    )
    .await
    .unwrap()
    .to_string();
    assert!(close.contains("unchanged"));
    assert!(close.contains("inactive"));
    assert!(profile_command(
        &session,
        &engines,
        "delete",
        &["review", "--agent", "claude"]
    )
    .await
    .is_err());
    let deleted = profile_command(
        &session,
        &engines,
        "delete",
        &["review", "--agent", "claude", "--yes"],
    )
    .await
    .unwrap()
    .to_string();
    assert!(deleted.contains("deleted"));
    assert!(!layout(&session, "review").root().exists());
}
#[tokio::test]
async fn profile_api_admission_requires_authenticated_local_owner() {
    use awman::{
        command::commands::api_server::admission::CommandSubmission,
        data::{
            daemon_state::{CommandListener, CommandRequestContext},
            fs::api_labels::Labels,
        },
    };
    let s = TestSandbox::new();
    let fixture = super::support::AdmissionFixture::new(&s);
    let command = || CommandSubmission {
        secrets: None,
        allow_remote_input: false,
        interaction: None,
        subcommand: "agents profile list".into(),
        args: vec![],
        labels: Labels::empty(),
        work_item: None,
    };
    let refused = fixture
        .admission
        .admit_session(fixture.local(&s), Some(command()), false, None)
        .await;
    assert!(refused.is_err());
    let context = CommandRequestContext {
        profile_owner: true,
        listener: CommandListener::Local,
        ..Default::default()
    };
    let admitted = fixture
        .admission
        .clone()
        .with_request_context(context.clone())
        .admit_session(fixture.local(&s), Some(command()), false, None)
        .await
        .unwrap();
    let record = fixture
        .store
        .list_commands_for_session(&admitted.session_id, 10)
        .unwrap();
    assert_eq!(record.len(), 1);
    assert!(awman::data::fs::agent_profiles::command_owner(
        &fixture.paths,
        &admitted.session_id,
        &record[0].id
    )
    .unwrap());
    fixture
        .store
        .update_setup_status(
            &admitted.session_id,
            awman::data::session_setup_event::SessionSetupStatus::Ready,
        )
        .unwrap();
    let native = CommandSubmission {
        subcommand: "exec prompt".into(),
        args: vec![
            "--settings-profile".into(),
            "review".into(),
            "--agent-session".into(),
            "provider-exact-id".into(),
            "follow-up".into(),
        ],
        ..command()
    };
    let projected = fixture
        .admission
        .clone()
        .with_request_context(context.clone())
        .admit_command(&admitted.session_id, native, None)
        .await
        .unwrap();
    assert_eq!(
        projected.flags_applied["agent_session"],
        "provider-exact-id"
    );
    assert_eq!(projected.flags_applied["settings_profile"], "review");
    assert_ne!(
        projected.flags_applied["agent_session"],
        admitted.session_id
    );
    let network = CommandRequestContext {
        listener: CommandListener::Network,
        ..context
    };
    assert!(fixture
        .admission
        .clone()
        .with_request_context(network)
        .admit_session(fixture.local(&s), Some(command()), false, None)
        .await
        .is_err());
    let smuggled = CommandSubmission {
        args: vec!["--settings-profile".into(), "review".into(), "task".into()],
        subcommand: "exec prompt".into(),
        ..command()
    };
    assert!(fixture
        .admission
        .admit_session(fixture.local(&s), Some(smuggled), false, None)
        .await
        .is_err());
}
#[tokio::test]
async fn identity_mismatch_never_publishes_a_replacement_conversation() {
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    let p = start(&session, "review", runtime.clone()).await;
    let id = "11111111-1111-4111-8111-111111111111";
    transcript(&p, id);
    p.leave(runtime.clone(), TurnStateAtExit::Complete)
        .await
        .unwrap();
    let mut resumed = enter(
        &session,
        &AgentName::new("claude").unwrap(),
        &request("review", Some(id)),
        &run(),
        &first_party("claude").unwrap(),
        runtime.clone(),
    )
    .await
    .unwrap()
    .unwrap();
    let wrong = "22222222-2222-4222-8222-222222222222";
    transcript(&resumed, wrong);
    assert!(resumed
        .observe_native_session(NativeSessionId::parse(wrong).unwrap())
        .is_err());
    let m = resumed
        .leave(runtime.clone(), TurnStateAtExit::Unknown)
        .await
        .unwrap();
    assert_eq!(m.state, ProfileState::Closing);
    assert_eq!(m.native_sessions.len(), 1);
    let l = layout(&session, "review");
    let lease = l.try_lock(Duration::ZERO).unwrap().unwrap();
    let spec = first_party("claude").unwrap().profile_state.unwrap();
    let m = reconcile(
        &lease,
        m,
        ReconcileMode::StopOwned,
        runtime,
        &spec,
        TurnStateAtExit::Unknown,
    )
    .await
    .unwrap();
    assert_eq!(m.native_sessions.len(), 1);
    assert_eq!(m.native_sessions[0].id.as_str(), id);
}
#[test]
fn sanitizer_allowlists_preserve_restrictions_and_drop_credential_endpoints() {
    use awman::engine::agent::profile_state::{sanitize, SeedSanitizer::*};
    for kind in [
        ClaudeConfig,
        ClaudeSettings,
        CursorCliConfig,
        CopilotConfig,
        ClineSettings,
        AntigravitySettings,
        OpenCodeJson,
    ] {
        let bytes=sanitize(&kind,br#"{"model":"selected","token":"credential-canary","env":{"KEY":"credential-canary"},"permissions":{"deny":["shell:rm"],"allow":["*"]}}"#).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("credential-canary"));
        assert!(!text.contains("allow"));
        assert!(text.contains("shell:rm"));
    }
    let json=sanitize(&OpenCodeJson,br#"{"provider":{"safe":{"options":{"baseURL":"https://service.test/v1"}},"unsafe":{"options":{"baseURL":"https://user:secret@service.test/v1?token=secret"}}},"permission":{"bash":"deny","read":"allow"}}"#).unwrap();
    let text = String::from_utf8(json).unwrap();
    assert!(text.contains("https://service.test/v1"));
    assert!(!text.contains("secret"));
    assert!(text.contains("deny"));
    assert!(sanitize(&ClaudeSettings, br#"{"permissions":{"deny":"malformed"}}"#).is_err());
    assert!(sanitize(&OpenCodeJson, br#"{"unknown":{"deny":["must-survive"]}}"#).is_err());
}

#[tokio::test]
async fn credential_injection_is_execution_private_and_never_exported() {
    use awman::data::fs::AuthPathResolver;
    use awman::engine::{
        agent::AgentEngine,
        auth::{
            credential::{CredentialFile, CredentialFingerprint},
            AgentCredentials, CredentialDelivery, RefreshableCredentialDelivery,
        },
        container::options::ResolvedContainerOptions,
        overlay::OverlayEngine,
    };
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    let a = start(&session, "a", runtime.clone()).await;
    let b = start(&session, "b", runtime.clone()).await;
    let overlay=OverlayEngine::with_auth_resolver(AuthPathResolver::at_home(s.home())).with_credential_provider(Arc::new(|_|Some(CredentialFile{relative_path:".credentials.json".into(),contents:br#"{"claudeAiOauth":{"accessToken":"injected-credential-canary","expiresAt":1900000000000}}"#.to_vec(),mode:0o600})));
    let engine = AgentEngine::new(Arc::new(overlay), runtime.clone());
    let agent = AgentName::new("claude").unwrap();
    let credentials = AgentCredentials {
        env_vars: vec![],
        delivery: CredentialDelivery::File(RefreshableCredentialDelivery {
            agent: agent.clone(),
            spec_agent: "claude",
            credential_env_key: "CLAUDE_CODE_OAUTH_TOKEN",
            staged_path: Default::default(),
            staged_root: Default::default(),
            initial_fingerprint: CredentialFingerprint::zeroed(),
        }),
    };
    let mut destinations = Vec::new();
    for p in [&a, &b] {
        let options = AgentRunOptions {
            profile_state: Some(p.run_state()),
            ..Default::default()
        };
        let resolved = ResolvedContainerOptions::resolve(
            engine
                .build_options_with_credentials(&session, &agent, &options, &credentials)
                .unwrap(),
        )
        .unwrap();
        let delivery = &resolved.refreshable_credentials[0];
        assert_eq!(delivery.staged_root, p.run_state().run_dir.join(".claude"));
        assert!(std::fs::read_to_string(&delivery.staged_path)
            .unwrap()
            .contains("injected-credential-canary"));
        assert!(!resolved
            .overlays
            .iter()
            .any(|o| o.host_path == s.home().join(".claude")));
        destinations.push(delivery.staged_path.clone());
    }
    assert_ne!(destinations[0], destinations[1]);
    for (name, p) in [("a", a), ("b", b)] {
        let m = p
            .leave(runtime.clone(), TurnStateAtExit::Unknown)
            .await
            .unwrap();
        let lease = layout(&session, name)
            .try_lock(Duration::ZERO)
            .unwrap()
            .unwrap();
        assert!(!lease
            .generation_tree(m.current_generation)
            .join(".claude/.credentials.json")
            .exists());
    }
}

#[tokio::test]
async fn crash_during_import_never_publishes_a_partial_policy_copy() {
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    create_private_dir(&s.home().join(".claude")).unwrap();
    std::fs::write(
        s.home().join(".claude/settings.json"),
        r#"{"permissions":{"deny":["must-survive"]}}"#,
    )
    .unwrap();
    let p = start(&session, "review", runtime.clone()).await;
    let run = p.run_state().run_dir;
    drop(p);
    let lease = layout(&session, "review")
        .try_lock(Duration::ZERO)
        .unwrap()
        .unwrap();
    let mut m = lease.read_manifest().unwrap().unwrap();
    m.owner.as_mut().unwrap().run_initialized = false;
    lease.write_manifest(&m).unwrap();
    std::fs::remove_file(run.join(".claude/settings.json")).unwrap();
    drop(lease);
    let p = start(&session, "review", runtime.clone()).await;
    assert!(
        std::fs::read_to_string(p.run_state().run_dir.join(".claude/settings.json"))
            .unwrap()
            .contains("must-survive")
    );
    p.leave(runtime, TurnStateAtExit::Unknown).await.unwrap();
}

#[tokio::test]
async fn credential_replacement_retains_the_profile_lease_and_imports_only_safe_exact_history() {
    let s = TestSandbox::new();
    let session = session(&s);
    let runtime = Runtime::new();
    let mut profile = start(&session, "rotation", runtime.clone()).await;
    profile
        .bind_redaction(awman::engine::auth::redaction::RedactionSet::new())
        .unwrap();
    let old = profile.execution_id().clone();
    let native = NativeSessionId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    transcript(&profile, native.as_str());
    std::fs::write(
        profile
            .run_state()
            .run_dir
            .join(".claude/.credentials.json"),
        "test-secret-excluded",
    )
    .unwrap();
    assert!(profile
        .quiesce_for_replacement(
            runtime.clone(),
            native.clone(),
            TurnStateAtExit::Complete,
            false
        )
        .await
        .is_err());
    let proof = profile
        .quiesce_for_replacement(
            runtime.clone(),
            native.clone(),
            TurnStateAtExit::Complete,
            true,
        )
        .await
        .unwrap();
    assert_eq!(proof.native_session_id(), &native);
    assert!(layout(&session, "rotation")
        .try_lock(Duration::ZERO)
        .unwrap()
        .is_none());
    let new = awman::data::ids::ExecutionId::new();
    let state = profile.replace_execution(proof, new.clone()).unwrap();
    assert_ne!(old, new);
    assert_eq!(state.execution_id, new);
    assert!(!state.run_dir.join(".claude/.credentials.json").exists());
    assert!(first_party("claude")
        .unwrap()
        .profile_state
        .unwrap()
        .list_sessions(&state.run_dir)
        .unwrap()
        .contains(&native));
    profile
        .leave(runtime.clone(), TurnStateAtExit::Complete)
        .await
        .unwrap();
    let mut missing = start(&session, "missing-rotation", runtime.clone()).await;
    missing
        .bind_redaction(awman::engine::auth::redaction::RedactionSet::new())
        .unwrap();
    transcript(&missing, native.as_str());
    let wrong = NativeSessionId::parse("22222222-2222-4222-8222-222222222222").unwrap();
    assert!(matches!(
        missing
            .quiesce_for_replacement(runtime.clone(), wrong, TurnStateAtExit::Complete, true)
            .await,
        Err(CommandError::NativeSessionNotFound)
    ));
    let closed = missing
        .leave(runtime, TurnStateAtExit::Complete)
        .await
        .unwrap();
    assert_eq!(closed.state, ProfileState::Inactive);
}

#[tokio::test]
async fn intermediate_and_final_history_publications_redact_retained_credentials() {
    use awman::engine::auth::{credential::SecretString, redaction::RedactionSet};
    let s = crate::support::sandbox("command::settings_profiles::intermediate_and_final_history_publications_redact_retained_credentials");
    let session = session(&s);
    let runtime = Runtime::new();
    let mut profile = start(&session, "redacted-rotation", runtime.clone()).await;
    let native = NativeSessionId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    transcript(&profile, native.as_str());
    let redaction = RedactionSet::new();
    profile.bind_redaction(redaction.clone()).unwrap();
    redaction.add(SecretString::new("test-secret-old-history"));
    redaction.add(SecretString::new("test-secret-new-history"));
    let relative = format!(".claude/projects/workspace-tokenizer/{native}.jsonl");
    std::fs::write(profile.run_state().run_dir.join(&relative), format!("{{\"sessionId\":\"{native}\",\"type\":\"assistant\",\"message\":\"test-secret-old-history test-secret-new-history\"}}\n")).unwrap();
    let proof = profile
        .quiesce_for_replacement(
            runtime.clone(),
            native.clone(),
            TurnStateAtExit::Complete,
            true,
        )
        .await
        .unwrap();
    let state = profile
        .replace_execution(proof, awman::data::ids::ExecutionId::new())
        .unwrap();
    let history = std::fs::read_to_string(state.run_dir.join(&relative)).unwrap();
    assert!(!history.contains("test-secret"));
    assert!(history.contains("*** ***"));
    std::fs::write(state.run_dir.join(&relative), format!("{{\"sessionId\":\"{native}\",\"type\":\"assistant\",\"message\":\"test-secret-new-history\"}}\n")).unwrap();
    let manifest = profile
        .leave(runtime, TurnStateAtExit::Complete)
        .await
        .unwrap();
    let lease = layout(&session, "redacted-rotation")
        .try_lock(Duration::ZERO)
        .unwrap()
        .unwrap();
    let history = std::fs::read_to_string(
        lease
            .generation_tree(manifest.current_generation)
            .join(&relative),
    )
    .unwrap();
    assert!(!history.contains("test-secret"));
    s.finish();
}

#[tokio::test]
async fn retired_or_lost_profile_redaction_withholds_new_history() {
    use awman::engine::auth::{credential::SecretString, redaction::RedactionSet};
    let s = crate::support::sandbox(
        "command::settings_profiles::retired_or_lost_profile_redaction_withholds_new_history",
    );
    let session = session(&s);
    let runtime = Runtime::new();
    let mut profile = start(&session, "retired-history", runtime.clone()).await;
    transcript(&profile, "11111111-1111-4111-8111-111111111111");
    let redaction = RedactionSet::new();
    redaction.add(SecretString::new("test-secret-withheld"));
    profile.bind_redaction(redaction.clone()).unwrap();
    redaction.clear();
    let before = layout(&session, "retired-history");
    // The held OS lease is released only when leave consumes the session.
    let manifest = profile
        .leave(runtime.clone(), TurnStateAtExit::Complete)
        .await
        .unwrap();
    assert_eq!(
        manifest.history_unavailable.as_deref(),
        Some("output_incomplete")
    );
    assert!(matches!(
        manifest.last_publication.unwrap().status,
        awman::data::fs::agent_profiles::PublicationStatus::RetainedPrevious
    ));
    assert!(before.try_lock(Duration::ZERO).unwrap().is_some());
    let mut profile = start(&session, "lost-redaction", runtime.clone()).await;
    transcript(&profile, "11111111-1111-4111-8111-111111111111");
    profile.bind_redaction(RedactionSet::new()).unwrap();
    drop(profile);
    let lease = layout(&session, "lost-redaction")
        .try_lock(Duration::ZERO)
        .unwrap()
        .unwrap();
    let mut manifest = lease.read_manifest().unwrap().unwrap();
    manifest.state = ProfileState::Closing;
    lease.write_manifest(&manifest).unwrap();
    let spec = first_party("claude").unwrap().profile_state.unwrap();
    let manifest = awman::command::commands::launch_policy::settings_profile::reconcile(
        &lease,
        manifest,
        awman::command::commands::launch_policy::settings_profile::ReconcileMode::VerifyOnly,
        runtime,
        &spec,
        TurnStateAtExit::Unknown,
    )
    .await
    .unwrap();
    assert_eq!(
        manifest.history_unavailable.as_deref(),
        Some("output_incomplete")
    );
    s.finish();
}

#[tokio::test]
async fn intermediate_publication_without_retained_redaction_is_withheld() {
    let s = crate::support::sandbox("command::settings_profiles::intermediate_publication_without_retained_redaction_is_withheld");
    let session = session(&s);
    let runtime = Runtime::new();
    let mut profile = start(&session, "unbound-redaction", runtime.clone()).await;
    let native = NativeSessionId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    transcript(&profile, native.as_str());
    assert!(profile
        .quiesce_for_replacement(runtime.clone(), native, TurnStateAtExit::Complete, true)
        .await
        .is_err());
    let manifest = profile
        .leave(runtime, TurnStateAtExit::Complete)
        .await
        .unwrap();
    assert_eq!(
        manifest.history_unavailable.as_deref(),
        Some("output_incomplete")
    );
    assert!(matches!(
        manifest.last_publication.unwrap().status,
        awman::data::fs::agent_profiles::PublicationStatus::RetainedPrevious
    ));
    s.finish();
}
