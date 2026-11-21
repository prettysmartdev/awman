use awman::command::commands::mount_scope::{MountScope, MountScopeDecision, MountScopeFrontend};
use awman::command::error::CommandError;
use awman::command::session_create::{SessionCreatePolicy, SessionCreateRequest};
use awman::data::message::{UserMessage, UserMessageSink};
use awman::engine::git::GitEngine;
use awman_test_support::fixtures::RepoBuilder;
use std::path::{Path, PathBuf};

struct Choice {
    decision: MountScopeDecision,
    asked: Vec<(PathBuf, PathBuf)>,
}
impl UserMessageSink for Choice {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}
impl MountScopeFrontend for Choice {
    fn ask_mount_scope(
        &mut self,
        root: &Path,
        cwd: &Path,
    ) -> Result<MountScopeDecision, CommandError> {
        self.asked.push((root.into(), cwd.into()));
        Ok(self.decision)
    }
}

#[test]
fn widening_to_the_parent_git_root_requires_a_decision_and_dismissal_aborts() {
    let s = crate::support::sandbox("security::mount_scope::widening_to_the_parent_git_root_requires_a_decision_and_dismissal_aborts");
    let repo = RepoBuilder::new(&s, "app")
        .file("nested/file", "fixture")
        .init();
    let root = repo.path().canonicalize().unwrap();
    let cwd = root.join("nested");
    for decision in [
        MountScopeDecision::MountCurrentDirOnly,
        MountScopeDecision::MountGitRoot,
        MountScopeDecision::Abort,
    ] {
        let mut f = Choice {
            decision,
            asked: vec![],
        };
        let result = MountScope::resolve(&cwd, &root, &mut f);
        assert_eq!(f.asked, [(root.clone(), cwd.clone())]);
        match decision {
            MountScopeDecision::MountCurrentDirOnly => assert_eq!(result.unwrap(), cwd),
            MountScopeDecision::MountGitRoot => assert_eq!(result.unwrap(), root),
            MountScopeDecision::Abort => assert!(matches!(result, Err(CommandError::Aborted))),
        }
    }
    let mut f = Choice {
        decision: MountScopeDecision::Abort,
        asked: vec![],
    };
    assert_eq!(
        MountScope::resolve(&root.join("nested/.."), &root, &mut f).unwrap(),
        root
    );
    assert!(f.asked.is_empty());
    s.finish();
}

#[test]
fn admission_allows_only_exact_mounts_or_independent_direct_child_repositories() {
    let s = crate::support::sandbox("security::mount_scope::admission_allows_only_exact_mounts_or_independent_direct_child_repositories");
    let repo = RepoBuilder::new(&s, "repos/app")
        .file("nested/file", "fixture")
        .init();
    let root = repo.path().parent().unwrap();
    let git = GitEngine::with_env(crate::support::env_snapshot(&s));
    let policy = SessionCreatePolicy::new(vec![], s.tmp().join("clones"))
        .with_workdir_roots(vec![root.into()]);
    let request = |p: &Path| SessionCreateRequest {
        context_key: None,
        workdir: Some(p.to_string_lossy().into_owned()),
        ..Default::default()
    };
    assert!(request(repo.path()).validate(&policy, &git).is_ok());
    for path in [root.to_path_buf(), repo.path().join("nested"), s.home()] {
        let error = request(&path).validate(&policy, &git).unwrap_err();
        assert!(
            matches!(error, CommandError::SessionWorkdirNotAllowed { .. }),
            "{error:?}"
        );
        assert_eq!(error.http_status(), 403);
    }
    s.finish();
}

#[test]
fn remote_plans_reject_local_transport_and_option_injection_before_clone() {
    let s = crate::support::sandbox("security::mount_scope::remote_plans_reject_local_transport_and_option_injection_before_clone");
    let git = GitEngine::with_env(crate::support::env_snapshot(&s));
    let policy = SessionCreatePolicy::new(vec![], s.tmp().join("clones"));
    for url in ["file:///etc", "/etc", "../repo", "ext::codex", "", "  "] {
        let request = SessionCreateRequest {
            context_key: None,
            session_type: Some("remote".into()),
            repo_url: Some(url.into()),
            ..Default::default()
        };
        assert!(request.validate(&policy, &git).is_err(), "{url}");
    }
    for branch in [
        "--upload-pack=codex",
        "../escape",
        "a..b",
        "refs/heads/no",
        "x\nexec",
    ] {
        let request = SessionCreateRequest {
            context_key: None,
            session_type: Some("remote".into()),
            repo_url: Some("https://example.invalid/r.git".into()),
            branch: Some(branch.into()),
            ..Default::default()
        };
        assert!(
            matches!(
                request.validate(&policy, &git),
                Err(CommandError::SessionRefInvalid { .. })
            ),
            "{branch}"
        );
    }
    assert!(!policy.clone_base_dir.exists());
    s.finish();
}

#[cfg(unix)]
#[test]
fn symlink_escapes_and_linked_git_directories_cannot_expand_admission() {
    #[cfg(unix)]
    {
        let s = crate::support::sandbox(
        "security::mount_scope::symlink_escapes_and_linked_git_directories_cannot_expand_admission",
    );
        let repo = RepoBuilder::new(&s, "repos/app")
            .file("file", "fixture")
            .init();
        let git = GitEngine::with_env(crate::support::env_snapshot(&s));
        let policy = SessionCreatePolicy::new(vec![repo.path().into()], s.tmp().join("clones"));
        let link = repo.path().join("escape");
        std::os::unix::fs::symlink(s.home(), &link).unwrap();
        let request = SessionCreateRequest {
            context_key: None,
            workdir: Some(link.to_string_lossy().into_owned()),
            ..Default::default()
        };
        assert!(matches!(
            request.validate(&policy, &git),
            Err(CommandError::SessionWorkdirNotAllowed { .. })
        ));
        let worktree = repo.worktree("linked", "linked-branch");
        let policy = SessionCreatePolicy::new(vec![], s.tmp().join("clones"))
            .with_workdir_roots(vec![worktree.as_path().parent().unwrap().into()]);
        let request = SessionCreateRequest {
            context_key: None,
            workdir: Some(worktree.as_path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        assert!(request.validate(&policy, &git).is_err());
        s.finish();
    }
}

// ── Mount authority: admission → setup → launch, and after a restart ──────────
//
// Real API admission, `SessionSetup` and `exec prompt` over a recording runtime
// that captures the final container options (every host mount) and then fails
// the launch, so nothing runs. Setup stops at its ready stage because the
// engines carry no container runtime: the session is opened and its
// authority persisted first, which is the part under test, and no host agent
// or image work is reached.
mod authority {
    use awman::command::commands::api_server::admission::{
        AdmissionError, ApiAdmission, SessionSubmission,
    };
    use awman::command::commands::api_server::event_bus::EventBus;
    use awman::command::commands::exec_prompt::{ExecPromptCommand, ExecPromptCommandFlags};
    use awman::command::commands::Command;
    use awman::command::dispatch::Engines;
    use awman::command::error::CommandError;
    use awman::command::session_create::{SessionCreatePolicy, SessionCreateRequest};
    use awman::command::session_setup::{SessionSetup, SessionSetupPresenter};
    use awman::data::api_lifecycle_store::CommandLifecycleStore;
    use awman::data::fs::api_db::SqliteSessionStore;
    use awman::data::fs::{ApiPaths, AuthPathResolver};
    use awman::data::mount_authority::{MountAuthority, MountAuthoritySource};
    use awman::data::session::{AgentHandle, Session, SessionMountAuthority};
    use awman::data::session_manager::SessionManager;
    use awman::data::session_setup_event::{
        SessionSetupState, SessionSetupStatus, SetupEventPayload,
    };
    use awman::data::WorkflowStateStore;
    use awman::engine::agent::AgentEngine;
    use awman::engine::agent_runtime::{
        execution::AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities, ImageAcquisition,
        ResolvedAgentOptions,
    };
    use awman::engine::auth::AuthEngine;
    use awman::engine::error::EngineError;
    use awman::engine::git::GitEngine;
    use awman::engine::overlay::OverlayEngine;
    use awman::frontend::api::command_frontend::ApiDispatchFrontend;
    use awman_test_support::fixtures::{ConfigTree, RepoBuilder};
    use awman_test_support::TestSandbox;
    #[cfg(unix)]
    use std::os::unix::fs::symlink as symlink_dir;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_dir;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    fn remove_dir_link(path: &Path) {
        #[cfg(unix)]
        std::fs::remove_file(path).unwrap();
        #[cfg(windows)]
        std::fs::remove_dir(path).unwrap();
    }

    /// Every (host, guest) mount of every launch the runtime was asked for.
    #[derive(Default)]
    struct Recorder {
        caps: Option<Capabilities>,
        launches: Mutex<Vec<Vec<(PathBuf, PathBuf)>>>,
        /// Runs once inside the next `image_home_dir`: a host change made
        /// while the engine prepares a launch, after its first mount checks.
        during_preparation: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl AgentRuntimeEngine for Recorder {
        fn ready_agent(
            &self,
            _: &str,
            _: awman::engine::agent_runtime::ReadyAgentOptions,
            _: &mut dyn awman::data::message::UserMessageSink,
        ) -> Result<(), EngineError> {
            Ok(())
        }
        fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
            Ok(true)
        }
        fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
            if let Some(change) = self.during_preparation.lock().unwrap().take() {
                change();
            }
            Ok(Some("/home/awman".into()))
        }
        fn build_image(
            &self,
            _: &awman::engine::container::BuildRequest,
            _: &mut dyn FnMut(&str),
        ) -> Result<(), EngineError> {
            panic!("no image is built in this journey")
        }
        fn runtime_name(&self) -> &'static str {
            "test"
        }
        fn display_name(&self) -> &'static str {
            "recording runtime"
        }
        fn capabilities(&self) -> &Capabilities {
            self.caps.as_ref().unwrap()
        }
        fn is_available(&self) -> bool {
            true
        }
        fn build(
            &self,
            options: ResolvedAgentOptions,
        ) -> Result<Box<dyn AgentInstance>, EngineError> {
            let ResolvedAgentOptions::Container(options) = options else {
                panic!("the journey uses the container paradigm");
            };
            self.launches.lock().unwrap().push(
                options
                    .overlays
                    .iter()
                    .map(|o| (o.host_path.clone(), o.container_path.clone()))
                    .collect(),
            );
            Err(EngineError::Other("captured by test runtime".into()))
        }
        fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
            panic!("runtime must not be reached")
        }
        fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
            panic!("runtime must not be reached")
        }
        fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
            panic!("runtime must not be reached")
        }
        fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
            panic!("runtime must not be reached")
        }
        fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
            panic!("runtime must not be reached")
        }
        fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
            panic!("runtime must not be reached")
        }
    }

    struct World {
        engines: Engines,
        runtime: Arc<Recorder>,
        store: Arc<SqliteSessionStore>,
        sessions: Arc<SessionManager>,
        paths: ApiPaths,
        env: awman::data::config::env::EnvSnapshot,
    }

    impl World {
        fn new(s: &TestSandbox) -> Self {
            ConfigTree::new()
                .global(serde_json::json!({"agents": {"auth":{"refresh":{"enabled":false}}}}))
                .write(s, None);
            let env = crate::support::env_snapshot(s);
            let runtime = Arc::new(Recorder {
                caps: Some(Capabilities {
                    arbitrary_host_mounts: true,
                    kit_declarative: false,
                    has_image_store: true,
                    image_acquisition: ImageAcquisition::Build,
                    per_launch_vm_settings: false,
                    reports_resource_stats: false,
                }),
                launches: Default::default(),
                during_preparation: Default::default(),
            });
            let auth = AuthPathResolver::at_home(s.home());
            let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()));
            let placeholder =
                Session::open_at_directory_root(s.work(), Default::default()).unwrap();
            let engines = Engines {
                runtime: runtime.clone(),
                // No container runtime: setup stops at its ready stage, after
                // the session is opened and its authority persisted.
                container_runtime: None,
                sandbox_runtime: None,
                git_engine: Arc::new(GitEngine::with_env(env.clone())),
                overlay_engine: overlay.clone(),
                auth_engine: Arc::new(AuthEngine::with_paths(
                    auth,
                    ApiPaths::from_env(&env).unwrap(),
                )),
                agent_engine: Arc::new(AgentEngine::new(overlay, runtime.clone())),
                workflow_state_store: Arc::new(WorkflowStateStore::new(&placeholder)),
                credential_monitor: None,
                global_config: Arc::new(Default::default()),
                ephemeral_secrets: Arc::new(Default::default()),
                lease_workflow_review: None,
            };
            let paths = ApiPaths::from_root(s.awman_home().join("api"));
            let store = Arc::new(SqliteSessionStore::open_from_paths(&paths).unwrap());
            let sessions = Arc::new(SessionManager::in_memory(env.clone()));
            Self {
                engines,
                runtime,
                store,
                sessions,
                paths,
                env,
            }
        }

        fn admission(&self, workdirs: Vec<PathBuf>, roots: Vec<PathBuf>) -> ApiAdmission {
            ApiAdmission::new(
                self.store.clone(),
                CommandLifecycleStore::open(self.store.clone()).unwrap(),
                self.paths.clone(),
                self.engines.clone(),
                self.sessions.clone(),
                workdirs,
                roots,
            )
        }

        fn launches(&self) -> Vec<Vec<(PathBuf, PathBuf)>> {
            self.runtime.launches.lock().unwrap().clone()
        }
    }

    fn submission(workdir: &Path) -> SessionSubmission {
        SessionSubmission {
            request: SessionCreateRequest {
                workdir: Some(workdir.to_string_lossy().into_owned()),
                ..Default::default()
            },
            labels: serde_json::from_value(serde_json::json!({})).unwrap(),
        }
    }

    struct Presenter;
    #[async_trait::async_trait]
    impl SessionSetupPresenter for Presenter {
        fn log(&mut self, _: &str) {}
        fn emit(&mut self, _: SetupEventPayload) {}
        fn ready_frontend(&mut self) -> Box<dyn awman::engine::ready::frontend::ReadyFrontend> {
            Box::new(ApiDispatchFrontend::new(
                "ready",
                &[],
                EventBus::new(16).sender(),
            ))
        }
        fn git_log_sink(&mut self) -> Box<dyn awman::engine::git::GitFrontend + Send> {
            Box::new(awman::data::message::RecordingMessageSink::default())
        }
        async fn finished(&mut self) {}
    }

    /// Admit, then run the production setup to completion. Returns the
    /// session id and the terminal setup state.
    async fn admit_and_set_up(
        w: &World,
        admission: &ApiAdmission,
        workdir: &Path,
    ) -> (String, SessionSetupState) {
        let admitted = admission
            .admit_session(submission(workdir), None, false, None)
            .await
            .unwrap();
        set_up(w, admitted.session_id, admitted.plan).await
    }

    async fn set_up(
        w: &World,
        id: String,
        plan: awman::command::session_create::SessionCreatePlan,
    ) -> (String, SessionSetupState) {
        let state = Arc::new(std::sync::RwLock::new(SessionSetupState::new()));
        SessionSetup::new(
            id.clone(),
            plan,
            w.engines.clone(),
            w.sessions.clone(),
            state.clone(),
            w.store.clone(),
            w.paths.clone(),
        )
        .run(&mut Presenter)
        .await;
        let state = state.read().unwrap().clone();
        (id, state)
    }

    fn prompt_flags(overlay: Vec<String>) -> ExecPromptCommandFlags {
        ExecPromptCommandFlags {
            prompt: Some("review the repository".into()),
            work_item: None,
            non_interactive: true,
            plan: false,
            allow_docker: false,
            yolo: false,
            yolo_authorized: false,
            auto: false,
            auto_explicit: false,
            agent: Some("claude".into()),
            model: None,
            launch_mode: None,
            overlay,
            agent_flags: None,
            session_worktree: None,
            automation: None,
        }
    }

    /// Launch `exec prompt` in `session` through the API frontend.
    async fn launch(w: &World, session: Session, overlay: Vec<String>) -> Result<(), CommandError> {
        let bus = EventBus::new(1024);
        let frontend = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());
        ExecPromptCommand::new(prompt_flags(overlay), w.engines.clone(), session)
            .run_with_frontend(Box::new(frontend))
            .await
            .map(|_| ())
    }

    async fn managed(w: &World, id: &str) -> Session {
        w.sessions
            .get_by_key(id)
            .expect("setup registered the session")
            .read()
            .await
            .clone()
    }

    /// Every launch mounted `root` at `/workspace`, and no host mount is Git
    /// metadata outside `root`.
    fn assert_only_root(launch: &[(PathBuf, PathBuf)], root: &Path) {
        let workspace: Vec<_> = launch
            .iter()
            .filter(|(_, guest)| guest == Path::new("/workspace"))
            .collect();
        assert_eq!(workspace.len(), 1, "{launch:?}");
        assert_eq!(workspace[0].0.canonicalize().unwrap(), root, "{launch:?}");
        for (host, _) in launch {
            let host = host.canonicalize().unwrap_or_else(|_| host.clone());
            for ancestor in host.ancestors() {
                if ancestor.join("HEAD").is_file() && ancestor.join("objects").is_dir() {
                    assert!(
                        ancestor.starts_with(root),
                        "Git metadata outside the authority: {host:?}"
                    );
                }
            }
            assert!(
                !root.starts_with(&host) || host == root,
                "a parent of the authority is mounted: {host:?}"
            );
        }
    }

    /// A restart: a fresh manager restores the row through the production
    /// restore path against `policy`.
    fn restart(w: &World, id: &str, workdirs: Vec<PathBuf>, roots: Vec<PathBuf>) -> Session {
        let record = w.store.get_session(id).unwrap().unwrap();
        let policy =
            SessionCreatePolicy::new(workdirs, w.paths.session_dir(id)).with_workdir_roots(roots);
        awman::command::session_setup::restore_session(
            &w.store,
            &record,
            &policy,
            &w.env,
            w.engines.git_engine.as_ref(),
        )
        .unwrap()
    }

    fn refused_with<F: Fn(&EngineError) -> bool>(result: Result<(), CommandError>, want: F) {
        match result {
            Err(CommandError::Engine(e)) if want(&e) => {}
            other => panic!("expected a mount refusal, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn whole_repo_grant_mounts_only_its_root_at_launch_and_after_restart() {
        let s = crate::support::sandbox("security::mount_scope::authority::whole_repo_grant_mounts_only_its_root_at_launch_and_after_restart");
        let w = World::new(&s);
        let repo = RepoBuilder::new(&s, "repos/app")
            .file("README.md", "fixture\n")
            .init();
        let root = repo.path().canonicalize().unwrap();
        let admission = w.admission(vec![root.clone()], vec![]);

        let (id, state) = admit_and_set_up(&w, &admission, &root).await;
        // Setup stopped at ready (no container runtime), after the open.
        assert_eq!(
            state.error.as_ref().map(|e| e.stage.as_str()),
            Some("ready"),
            "{state:?}"
        );
        let session = managed(&w, &id).await;
        let authority = session.granted_mount_authority().unwrap().clone();
        assert_eq!(authority.root(), root);
        assert_eq!(authority.source(), MountAuthoritySource::ExactWorkdir);
        let saved = w
            .store
            .mount_authority(&id)
            .unwrap()
            .expect("authority persisted with the row");
        assert_eq!(MountAuthority::from_persisted(&saved).unwrap(), authority);

        let _ = launch(&w, session, vec![]).await;
        let launches = w.launches();
        assert_eq!(launches.len(), 1);
        assert_only_root(&launches[0], &root);

        // Restart: the same grant still holds and launches mount the same root.
        let restored = restart(&w, &id, vec![root.clone()], vec![]);
        assert_eq!(restored.granted_mount_authority(), Some(&authority));
        let _ = launch(&w, restored, vec![]).await;
        let launches = w.launches();
        assert_eq!(launches.len(), 2);
        assert_only_root(&launches[1], &root);

        // A Git directory outside the authority is refused as a mount, even
        // as an explicit overlay, before any runtime create.
        let foreign = RepoBuilder::new(&s, "repos/foreign").file("x", "x").init();
        let foreign_git = foreign.path().join(".git").canonicalize().unwrap();
        let result = launch(
            &w,
            managed(&w, &id).await,
            vec![format!("dir({}:/foreign:ro)", foreign_git.display())],
        )
        .await;
        refused_with(result, |e| {
            matches!(e, EngineError::MountOutsideAuthority { .. })
        });
        assert_eq!(w.launches().len(), 2, "refused before the runtime");
        s.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn direct_child_and_plain_directory_grants_keep_working_through_restart() {
        let s = crate::support::sandbox("security::mount_scope::authority::direct_child_and_plain_directory_grants_keep_working_through_restart");
        let w = World::new(&s);
        let repo = RepoBuilder::new(&s, "repos/child")
            .file("README.md", "fixture\n")
            .init();
        let child = repo.path().canonicalize().unwrap();
        let roots = vec![child.parent().unwrap().to_path_buf()];
        let plain = s.work().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let plain = plain.canonicalize().unwrap();
        let admission = w.admission(vec![plain.clone()], roots.clone());

        for (dir, source) in [
            (&child, MountAuthoritySource::WorkdirRootChild),
            (&plain, MountAuthoritySource::ExactWorkdir),
        ] {
            let before = w.launches().len();
            let (id, _) = admit_and_set_up(&w, &admission, dir).await;
            let session = managed(&w, &id).await;
            assert_eq!(session.granted_mount_authority().unwrap().source(), source);
            let _ = launch(&w, session, vec![]).await;
            let restored = restart(&w, &id, vec![plain.clone()], roots.clone());
            let _ = launch(&w, restored, vec![]).await;
            let launches = w.launches();
            assert_eq!(launches.len(), before + 2, "{dir:?}");
            for launch in &launches[before..] {
                assert_only_root(launch, dir);
            }
        }
        s.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exact_subdirectory_of_a_checkout_is_refused_before_setup_and_never_widens() {
        let s = crate::support::sandbox("security::mount_scope::authority::exact_subdirectory_of_a_checkout_is_refused_before_setup_and_never_widens");
        let w = World::new(&s);
        let repo = RepoBuilder::new(&s, "repos/project")
            .file("allowed/file", "fixture\n")
            .init();
        let root = repo.path().canonicalize().unwrap();
        let allowed = root.join("allowed");
        let admission = w.admission(vec![allowed.clone()], vec![]);
        match admission
            .admit_session(submission(&allowed), None, false, None)
            .await
        {
            Err(AdmissionError::SessionCreation(error)) => {
                assert_eq!(error.http_status(), 403);
                let text = error.to_string();
                assert!(text.contains("exact_workdir_not_git_root"), "{text}");
                assert!(
                    text.contains(&root.display().to_string()),
                    "names the enclosing root: {text}"
                );
            }
            Err(other) => panic!("expected a 403 refusal, got {other:?}"),
            Ok(_) => panic!("an exact subdirectory of a checkout was admitted"),
        }
        assert!(
            w.store.list_sessions_by_status(None).unwrap().is_empty(),
            "nothing persisted"
        );
        assert!(w.launches().is_empty());

        // A plain directory admitted while it had no enclosing repository,
        // whose parent becomes a checkout before setup: setup refuses to open
        // it rather than mounting the new parent.
        let outer = s.work().join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let inner = inner.canonicalize().unwrap();
        let admission = w.admission(vec![inner.clone()], vec![]);
        let admitted = admission
            .admit_session(submission(&inner), None, false, None)
            .await
            .unwrap();
        RepoBuilder::at(&s, outer.clone()).init();
        let (id, state) = set_up(&w, admitted.session_id, admitted.plan).await;
        assert_eq!(state.status, SessionSetupStatus::Failed);
        assert_eq!(
            state.error.as_ref().map(|e| e.stage.as_str()),
            Some("session_open"),
            "{state:?}"
        );
        assert!(w.sessions.get_by_key(&id).is_none());
        assert!(w.store.mount_authority(&id).unwrap().is_none());
        assert!(w.launches().is_empty());
        s.finish();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn path_replaced_after_admission_or_setup_refuses_the_launch() {
        let s = crate::support::sandbox("security::mount_scope::authority::path_replaced_after_admission_or_setup_refuses_the_launch");
        let w = World::new(&s);
        let repo = RepoBuilder::new(&s, "repos/app")
            .file("README.md", "fixture\n")
            .init();
        let root = repo.path().canonicalize().unwrap();
        let elsewhere = s.work().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let admission = w.admission(vec![root.clone()], vec![]);

        // Replaced between admission and setup: the open fails.
        let admitted = admission
            .admit_session(submission(&root), None, false, None)
            .await
            .unwrap();
        let moved = s.work().join("moved-app");
        std::fs::rename(&root, &moved).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &root).unwrap();
        let (_, state) = set_up(&w, admitted.session_id, admitted.plan).await;
        assert_eq!(
            state.error.as_ref().map(|e| e.stage.as_str()),
            Some("session_open"),
            "{state:?}"
        );
        std::fs::remove_file(&root).unwrap();
        std::fs::rename(&moved, &root).unwrap();

        // Replaced between setup and launch: the engine refuses.
        let (id, _) = admit_and_set_up(&w, &admission, &root).await;
        let session = managed(&w, &id).await;
        std::fs::rename(&root, &moved).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &root).unwrap();
        refused_with(launch(&w, session, vec![]).await, |e| {
            matches!(e, EngineError::MountOutsideAuthority { .. })
        });
        assert!(w.launches().is_empty());
        std::fs::remove_file(&root).unwrap();
        std::fs::rename(&moved, &root).unwrap();
        s.finish();
    }

    /// The workspace is swapped while the engine prepares the launch, after
    /// its first authority check: by a link to a foreign directory, and by a
    /// rename that leaves a different directory at the same canonical path.
    /// Both are refused before the runtime creates anything.
    #[tokio::test(flavor = "multi_thread")]
    async fn workspace_swapped_while_the_launch_is_prepared_never_reaches_the_runtime() {
        let s = crate::support::sandbox("security::mount_scope::authority::workspace_swapped_while_the_launch_is_prepared_never_reaches_the_runtime");
        let w = World::new(&s);
        let repo = RepoBuilder::new(&s, "repos/app")
            .file("README.md", "fixture\n")
            .init();
        let root = repo.path().canonicalize().unwrap();
        let foreign = s.work().join("foreign");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("secret.txt"), "foreign\n").unwrap();
        let moved = s.work().join("moved-app");
        let admission = w.admission(vec![root.clone()], vec![]);
        let (id, _) = admit_and_set_up(&w, &admission, &root).await;

        // A link to a foreign directory.
        {
            let (root, foreign, moved) = (root.clone(), foreign.clone(), moved.clone());
            *w.runtime.during_preparation.lock().unwrap() = Some(Box::new(move || {
                std::fs::rename(&root, &moved).unwrap();
                symlink_dir(&foreign, &root).unwrap();
            }));
        }
        refused_with(launch(&w, managed(&w, &id).await, vec![]).await, |e| {
            matches!(e, EngineError::MountOutsideAuthority { .. })
        });
        assert!(w.launches().is_empty(), "{:?}", w.launches());
        remove_dir_link(&root);
        std::fs::rename(&moved, &root).unwrap();

        // A different directory renamed into place: the path still resolves
        // to the authority root, but it is not the directory that was checked.
        let replacement = s.work().join("replacement");
        std::fs::create_dir_all(&replacement).unwrap();
        {
            let (root, replacement, moved) = (root.clone(), replacement.clone(), moved.clone());
            *w.runtime.during_preparation.lock().unwrap() = Some(Box::new(move || {
                std::fs::rename(&root, &moved).unwrap();
                std::fs::rename(&replacement, &root).unwrap();
            }));
        }
        refused_with(
            launch(&w, managed(&w, &id).await, vec![]).await,
            |e| matches!(e, EngineError::MountOutsideAuthority { reason, .. } if reason.contains("replaced")),
        );
        assert!(w.launches().is_empty(), "{:?}", w.launches());
        std::fs::rename(&root, &replacement).unwrap();
        std::fs::rename(&moved, &root).unwrap();

        // Untouched, the same session launches with only its root.
        let _ = launch(&w, managed(&w, &id).await, vec![]).await;
        let launches = w.launches();
        assert_eq!(launches.len(), 1);
        assert_only_root(&launches[0], &root);
        s.finish();
    }

    /// A Git directory mount (presented to the guest as `.git`, like the
    /// linked-worktree Git mount) is judged by its declared purpose, not by
    /// what its target contains: a nested `.git` inside the authority that becomes
    /// a link to an unrelated non-Git directory, or to a foreign repository,
    /// is refused before the runtime, whether it changes before the launch or
    /// while the launch is prepared.
    #[tokio::test(flavor = "multi_thread")]
    async fn declared_git_mount_redirected_to_a_foreign_target_is_refused() {
        let s = crate::support::sandbox("security::mount_scope::authority::declared_git_mount_redirected_to_a_foreign_target_is_refused");
        let w = World::new(&s);
        let repo = RepoBuilder::new(&s, "repos/app")
            .file("README.md", "fixture\n")
            .init();
        let root = repo.path().canonicalize().unwrap();
        let nested = root.join("vendor/lib");
        std::fs::create_dir_all(nested.join(".git/objects")).unwrap();
        std::fs::create_dir_all(nested.join(".git/refs")).unwrap();
        std::fs::write(nested.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let nested_git = nested.join(".git");
        let saved = s.work().join("saved-nested-git");
        let plain = s.work().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("data"), "not git\n").unwrap();
        let foreign = RepoBuilder::new(&s, "repos/foreign").file("x", "x").init();
        let foreign_git = foreign.path().join(".git").canonicalize().unwrap();
        let admission = w.admission(vec![root.clone()], vec![]);
        let (id, _) = admit_and_set_up(&w, &admission, &root).await;
        let overlay = || vec![format!("dir({}:/vendor-lib/.git:ro)", nested_git.display())];

        // The nested Git directory inside the authority is an allowed mount.
        let _ = launch(&w, managed(&w, &id).await, overlay()).await;
        assert_eq!(w.launches().len(), 1);

        for target in [&plain, &foreign_git] {
            // Redirected before the launch.
            std::fs::rename(&nested_git, &saved).unwrap();
            symlink_dir(target, &nested_git).unwrap();
            refused_with(launch(&w, managed(&w, &id).await, overlay()).await, |e| {
                matches!(e, EngineError::MountOutsideAuthority { .. })
            });
            remove_dir_link(&nested_git);
            std::fs::rename(&saved, &nested_git).unwrap();

            // Redirected while the launch is prepared.
            {
                let (nested_git, saved, target) =
                    (nested_git.clone(), saved.clone(), target.to_path_buf());
                *w.runtime.during_preparation.lock().unwrap() = Some(Box::new(move || {
                    std::fs::rename(&nested_git, &saved).unwrap();
                    symlink_dir(&target, &nested_git).unwrap();
                }));
            }
            refused_with(launch(&w, managed(&w, &id).await, overlay()).await, |e| {
                matches!(e, EngineError::MountOutsideAuthority { .. })
            });
            remove_dir_link(&nested_git);
            std::fs::rename(&saved, &nested_git).unwrap();
        }
        assert_eq!(
            w.launches().len(),
            1,
            "every redirect refused before the runtime"
        );
        s.finish();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restored_sessions_without_a_current_grant_require_revalidation() {
        let s = crate::support::sandbox("security::mount_scope::authority::restored_sessions_without_a_current_grant_require_revalidation");
        let w = World::new(&s);
        let repo = RepoBuilder::new(&s, "repos/app")
            .file("README.md", "fixture\n")
            .init();
        let root = repo.path().canonicalize().unwrap();
        let admission = w.admission(vec![root.clone()], vec![]);
        let (id, _) = admit_and_set_up(&w, &admission, &root).await;

        let revalidation = |session: &Session| {
            assert!(
                matches!(
                    session.mount_authority(),
                    SessionMountAuthority::RevalidationRequired { .. }
                ),
                "{:?}",
                session.mount_authority()
            );
        };
        // The allowlist no longer grants the workdir.
        let narrowed = restart(&w, &id, vec![], vec![]);
        revalidation(&narrowed);
        refused_with(launch(&w, narrowed, vec![]).await, |e| {
            matches!(e, EngineError::MountAuthorityRevalidationRequired { .. })
        });

        // A row written before authorities were recorded: never inferred,
        // even though the workdir is still allowlisted.
        w.store
            .with_conn(|conn| {
                conn.execute_batch(&format!(
                    "UPDATE sessions SET mount_authority = NULL WHERE id = '{id}'"
                ))?;
                Ok(())
            })
            .unwrap();
        let older = restart(&w, &id, vec![root.clone()], vec![]);
        revalidation(&older);
        let refusal = launch(&w, older, vec![]).await;
        assert!(
            format!("{refusal:?}").contains("RevalidationRequired"),
            "{refusal:?}"
        );
        refused_with(refusal, |e| {
            e.to_string()
                .contains("mount_authority_revalidation_required")
        });
        assert!(
            w.launches().is_empty(),
            "no runtime create for an unrevalidated session"
        );
        s.finish();
    }

    #[cfg(unix)]
    #[test]
    fn linked_worktrees_pointer_files_and_core_worktree_are_refused_as_exact_entries() {
        let s = crate::support::sandbox("security::mount_scope::authority::linked_worktrees_pointer_files_and_core_worktree_are_refused_as_exact_entries");
        let git = GitEngine::with_env(crate::support::env_snapshot(&s));
        let repo = RepoBuilder::new(&s, "repos/app")
            .file("file", "fixture\n")
            .init();
        let worktree = repo
            .worktree("linked", "linked-branch")
            .canonicalize()
            .unwrap();
        let redirected = RepoBuilder::new(&s, "repos/redirected")
            .file("f", "x")
            .init();
        let config = redirected.path().join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str("[core]\n\tworktree = /\n");
        std::fs::write(&config, text).unwrap();
        let symlinked = s.work().join("symlinked");
        std::fs::create_dir_all(&symlinked).unwrap();
        std::os::unix::fs::symlink(repo.path().join(".git"), symlinked.join(".git")).unwrap();

        for dir in [
            worktree,
            redirected.path().canonicalize().unwrap(),
            symlinked.canonicalize().unwrap(),
        ] {
            let policy = SessionCreatePolicy::new(vec![dir.clone()], s.tmp().join("clones"));
            let request = SessionCreateRequest {
                workdir: Some(dir.to_string_lossy().into_owned()),
                ..Default::default()
            };
            let error = request.validate(&policy, &git).unwrap_err();
            assert_eq!(error.http_status(), 403, "{dir:?}");
            assert!(
                error.to_string().contains("exact_workdir_not_git_root"),
                "{dir:?}: {error}"
            );
        }
        // The plain checkout itself is still a valid exact grant.
        let policy = SessionCreatePolicy::new(vec![repo.path().into()], s.tmp().join("clones"));
        let plan = SessionCreateRequest {
            workdir: Some(repo.path().to_string_lossy().into_owned()),
            ..Default::default()
        }
        .validate(&policy, &git)
        .unwrap();
        assert_eq!(
            plan.mount_authority.unwrap().root(),
            repo.path().canonicalize().unwrap()
        );
        s.finish();
    }
}
