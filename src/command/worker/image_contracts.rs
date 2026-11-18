//! Component contracts for production lease provisioning and private-state eviction.
use super::*;
use crate::data::{
    config::env::EnvSnapshot,
    fs::api_db::SqliteSessionStore,
    image_inputs::{InputsHash, RepoKey, INPUTS_LABEL, REPO_LABEL},
    session::{Session, SessionOpenOptions},
    worker::{
        lease_image_store::LeaseImageStore,
        lease_images::{ImageBudget, LeaseImageKind, LeaseImageRecord},
    },
};
use crate::engine::{
    agent_runtime::*,
    container::{runtime::ImageMetadata, BuildRequest, ContainerRuntime},
    error::EngineError,
};
use awman_test_support::{TestCase, TestSandbox};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct Runtime {
    images: Mutex<BTreeMap<String, ImageMetadata>>,
    built: Mutex<Vec<BuildRequest>>,
    removed: Mutex<Vec<String>>,
    fail_remove: Mutex<BTreeSet<String>>,
    hold_first: AtomicBool,
    hold_remove: AtomicBool,
    remove_entered: tokio::sync::Notify,
    release_remove: (Mutex<bool>, std::sync::Condvar),
    build_entered: tokio::sync::Notify,
    release_build: (Mutex<bool>, std::sync::Condvar),
}
impl AgentRuntimeEngine for Runtime {
    fn runtime_name(&self) -> &'static str {
        "docker"
    }
    fn display_name(&self) -> &'static str {
        "lease contract runtime"
    }
    fn capabilities(&self) -> &Capabilities {
        static CAPS: Capabilities = Capabilities {
            arbitrary_host_mounts: true,
            kit_declarative: false,
            has_image_store: true,
            image_acquisition: ImageAcquisition::Build,
            per_launch_vm_settings: false,
            reports_resource_stats: false,
        };
        &CAPS
    }
    fn is_available(&self) -> bool {
        true
    }
    fn build(&self, _: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("no agents run")
    }
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }
    fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
        panic!("no agents run")
    }
    fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
        panic!("no agents run")
    }
    fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("no agents run")
    }
    fn ready_agent(
        &self,
        _: &str,
        _: ReadyAgentOptions,
        _: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<(), EngineError> {
        panic!("no agents run")
    }
    fn image_exists(&self, tag: &str) -> Result<bool, EngineError> {
        Ok(self.images.lock().unwrap().contains_key(tag))
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        Ok(None)
    }
    fn inspect_image(&self, tag: &str) -> Result<Option<ImageMetadata>, EngineError> {
        Ok(self.images.lock().unwrap().get(tag).cloned())
    }
    fn build_image(
        &self,
        request: &BuildRequest,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        let recipe = std::fs::read_to_string(&request.dockerfile).unwrap();
        for line in recipe.lines() {
            if let Some(base) = line.strip_prefix("FROM awman-") {
                let base = format!("awman-{}", base.split_whitespace().next().unwrap());
                assert!(
                    self.images.lock().unwrap().contains_key(&base),
                    "agent build must use its prepared project base: {base}"
                );
            }
        }
        on_line("contract builder output");
        self.built.lock().unwrap().push(request.clone());
        if self.hold_first.swap(false, Ordering::SeqCst) {
            self.build_entered.notify_one();
            let released = self.release_build.0.lock().unwrap();
            let (_released, deadline) = self
                .release_build
                .1
                .wait_timeout_while(released, Duration::from_secs(5), |released| !*released)
                .unwrap();
            assert!(
                !deadline.timed_out(),
                "the second lease must reserve before the first build finishes"
            );
        }
        request.labels.get(INPUTS_LABEL).expect("inputs label");
        self.images.lock().unwrap().insert(
            request.reference.clone(),
            ImageMetadata {
                id: format!("sha256:build-{}", self.built.lock().unwrap().len()),
                tags: vec![request.reference.clone()],
                labels: request.labels.clone(),
                size_bytes: 100,
            },
        );
        Ok(())
    }
    fn remove_image(&self, tag: &str) -> Result<(), EngineError> {
        self.removed.lock().unwrap().push(tag.into());
        if self.hold_remove.swap(false, Ordering::SeqCst) {
            self.remove_entered.notify_one();
            let (_guard, timeout) = self
                .release_remove
                .1
                .wait_timeout_while(
                    self.release_remove.0.lock().unwrap(),
                    Duration::from_secs(5),
                    |released| !*released,
                )
                .unwrap();
            assert!(!timeout.timed_out(), "removal must be released");
        }
        if self.fail_remove.lock().unwrap().contains(tag) {
            return Err(EngineError::Other(
                "image in use by external container".into(),
            ));
        }
        self.images.lock().unwrap().remove(tag);
        Ok(())
    }
}

fn env(sandbox: &TestSandbox) -> EnvSnapshot {
    let mut values: std::collections::HashMap<String, String> = sandbox
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    values.insert(
        "DOCKER_HOST".into(),
        format!(
            "unix://{}",
            sandbox.work().join("fake-engine.sock").display()
        ),
    );
    EnvSnapshot::from_instance_env(values.into_iter().collect(), sandbox.work(), vec![])
}
fn session(sandbox: &TestSandbox, name: &str, project: Option<&str>) -> Session {
    let root = sandbox.work().join(name);
    std::fs::create_dir_all(root.join(".awman")).unwrap();
    if let Some(project) = project {
        std::fs::write(root.join("Dockerfile.dev"), project).unwrap();
    }
    std::fs::write(
        root.join(".awman/Dockerfile.claude"),
        "FROM awman-original:latest\nRUN echo agent\n",
    )
    .unwrap();
    Session::open_at_git_root(
        root.clone(),
        root,
        SessionOpenOptions {
            env: Some(env(sandbox)),
            ..Default::default()
        },
    )
    .unwrap()
}
fn db(sandbox: &TestSandbox) -> Arc<SqliteSessionStore> {
    let paths = crate::data::fs::DataPaths::from_env(&env(sandbox)).unwrap();
    Arc::new(SqliteSessionStore::open_at(&paths.db_path()).unwrap())
}
fn lease(db: &Arc<SqliteSessionStore>, id: &str) -> LeaseRecord {
    let mut item = awman_wsp::conformance::work_item(id);
    item.exec.agent = Some("claude".into());
    let grant = awman_wsp::Lease {
        id: id.into(),
        epoch: 1,
        expires_at: Utc::now() + chrono::Duration::minutes(10),
        item,
        terminal: false,
        remote_input: false,
    };
    let record = LeaseRecord::claimed("test", &grant, LeaseHost::Daemon, Utc::now());
    LeaseStore::open(db.clone())
        .unwrap()
        .insert(&record)
        .unwrap();
    record
}
fn engines(session: &Session, runtime: Arc<Runtime>) -> crate::command::dispatch::Engines {
    let mut engines = crate::command::dispatch::Engines::from_detected(
        DetectedRuntime::Container(Arc::new(ContainerRuntime::docker_in(session.env().clone()))),
        session,
    )
    .unwrap();
    engines.runtime = runtime;
    engines
}
async fn provision(
    sandbox: &TestSandbox,
    session: &mut Session,
    runtime: Arc<Runtime>,
    db: Arc<SqliteSessionStore>,
    record: &LeaseRecord,
) {
    let private = sandbox.work().join(format!("private-{}", record.id));
    std::fs::create_dir_all(&private).unwrap();
    super::provision::agents_for_lease(
        &engines(session, runtime),
        session,
        &record.item,
        &private,
        db,
        record,
    )
    .await
    .unwrap();
}
fn ended(db: &Arc<SqliteSessionStore>, mut record: LeaseRecord) {
    record.state = LeaseState::Done;
    LeaseStore::open(db.clone())
        .unwrap()
        .update(&record)
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_items_reuse_images_and_project_changes_invalidate_agents() {
    let sandbox = TestSandbox::for_test(TestCase::component("command::worker::image_contracts::two_items_reuse_images_and_project_changes_invalidate_agents"));
    let runtime = Arc::new(Runtime::default());
    let db = db(&sandbox);
    let first = lease(&db, "first");
    let second = lease(&db, "second");
    let mut a = session(&sandbox, "clone-one", Some("FROM debian:stable\n"));
    let mut b = session(&sandbox, "clone-two", Some("FROM debian:stable\n"));
    provision(&sandbox, &mut a, runtime.clone(), db.clone(), &first).await;
    provision(&sandbox, &mut b, runtime.clone(), db.clone(), &second).await;
    assert_eq!(a.lease_images(), b.lease_images());
    assert_eq!(
        runtime.built.lock().unwrap().len(),
        2,
        "one project and one agent build across both items"
    );
    let old = b.lease_images().unwrap().clone();
    std::fs::write(
        b.git_root().join("Dockerfile.dev"),
        "FROM debian:stable\nRUN echo changed\n",
    )
    .unwrap();
    provision(&sandbox, &mut b, runtime.clone(), db.clone(), &second).await;
    assert_ne!(old.project, b.project_image_tag());
    assert_ne!(old.agents["claude"], b.agent_image_tag("claude"));
    assert_eq!(runtime.built.lock().unwrap().len(), 4);
    assert!(runtime
        .built
        .lock()
        .unwrap()
        .iter()
        .all(|request| request.labels[REPO_LABEL]
            == RepoKey::for_repo(Some(&first.item.repo.url), a.git_root()).as_str()));
    let store = LeaseImageStore::open(db.clone()).unwrap();
    let key = images::store_key(runtime.as_ref(), a.env())
        .unwrap()
        .describe();
    assert_eq!(store.records(&key, None).unwrap().len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_leases_share_builds_and_default_base_is_shared_across_repos() {
    let sandbox = TestSandbox::for_test(TestCase::component("command::worker::image_contracts::concurrent_leases_share_builds_and_default_base_is_shared_across_repos"));
    let runtime = Arc::new(Runtime::default());
    let db = db(&sandbox);
    let first = lease(&db, "first");
    let mut second = lease(&db, "second");
    let mut a = session(&sandbox, "clone-one", Some("FROM scratch\n"));
    let mut b = session(&sandbox, "clone-two", Some("FROM scratch\n"));
    runtime.hold_first.store(true, Ordering::SeqCst);
    let release = async {
        runtime.build_entered.notified().await;
        awman_test_support::wait_for_async(
            "both leases reserved the project",
            Duration::from_secs(3),
            || async {
                let count: u32 = db
                    .with_conn(|conn| {
                        Ok(conn.query_row(
                            "SELECT COUNT(*) FROM worker_lease_image_uses",
                            [],
                            |row| row.get(0),
                        )?)
                    })
                    .unwrap();
                (count == 2).then_some(())
            },
        )
        .await;
        *runtime.release_build.0.lock().unwrap() = true;
        runtime.release_build.1.notify_all();
    };
    tokio::join!(
        provision(&sandbox, &mut a, runtime.clone(), db.clone(), &first),
        provision(&sandbox, &mut b, runtime.clone(), db.clone(), &second),
        release
    );
    assert_eq!(runtime.built.lock().unwrap().len(), 2);
    assert_eq!(a.lease_images(), b.lease_images());
    let mut c = session(&sandbox, "default-one", None);
    let mut d = session(&sandbox, "default-two", None);
    second.item.repo.url = "https://example.test/another/repo.git".into();
    provision(&sandbox, &mut c, runtime.clone(), db.clone(), &first).await;
    provision(&sandbox, &mut d, runtime.clone(), db, &second).await;
    assert_eq!(c.project_image_tag(), d.project_image_tag());
    assert_eq!(
        runtime.built.lock().unwrap().len(),
        5,
        "one shared default base, two repository agent images"
    );
}

fn install(
    runtime: &Runtime,
    store: &LeaseImageStore,
    key: &str,
    lease: &LeaseRecord,
    image: (&str, u64, i64),
    built: bool,
) {
    let (tag, size, minute) = image;
    let record = LeaseImageRecord {
        tag: tag.into(),
        image_id: format!("id-{tag}"),
        repo_key: RepoKey::from_label("example.test/repo"),
        inputs_hash: InputsHash::from_label("aa"),
        kind: LeaseImageKind::Project,
        bytes: size,
        last_used: Utc::now() + chrono::Duration::minutes(minute),
    };
    runtime.images.lock().unwrap().insert(
        tag.into(),
        ImageMetadata {
            id: record.image_id.clone(),
            tags: vec![tag.into()],
            labels: BTreeMap::from([(INPUTS_LABEL.into(), "aa".into())]),
            size_bytes: size,
        },
    );
    store.reserve(key, tag, &lease.id, &lease.server).unwrap();
    store.record(key, &record, built).unwrap();
}
#[tokio::test]
async fn cleanup_respects_both_budgets_active_leases_ownership_and_removal_failure() {
    let sandbox = TestSandbox::for_test(TestCase::component("command::worker::image_contracts::cleanup_respects_both_budgets_active_leases_ownership_and_removal_failure"));
    let runtime = Runtime::default();
    let db = db(&sandbox);
    let old = lease(&db, "old");
    let active = lease(&db, "active");
    let store = LeaseImageStore::open(db.clone()).unwrap();
    let env = env(&sandbox);
    let key = images::store_key(&runtime, &env).unwrap().describe();
    install(&runtime, &store, &key, &old, ("old", 100, -5), true);
    install(&runtime, &store, &key, &old, ("middle", 100, -4), true);
    install(&runtime, &store, &key, &active, ("active", 100, -3), true);
    install(&runtime, &store, &key, &old, ("not-owned", 100, -9), false);
    ended(&db, old);
    runtime.fail_remove.lock().unwrap().insert("old".into());
    let budgets = BTreeMap::from([(
        "test".into(),
        ImageBudget {
            max_images: Some(1),
            max_bytes: Some(100),
        },
    )]);
    images::cleanup(&runtime, &env, db.clone(), &budgets);
    assert_eq!(*runtime.removed.lock().unwrap(), ["old", "middle"]);
    assert!(runtime.images.lock().unwrap().contains_key("active"));
    assert!(runtime.images.lock().unwrap().contains_key("not-owned"));
    assert!(runtime.images.lock().unwrap().contains_key("old"));
    assert_eq!(
        store.records(&key, None).unwrap().len(),
        2,
        "failed removal is retained for retry"
    );
    assert_eq!(
        LeaseStore::open(db.clone())
            .unwrap()
            .get(&active.id)
            .unwrap()
            .unwrap()
            .state,
        LeaseState::Claimed
    );
    runtime.fail_remove.lock().unwrap().clear();
    images::cleanup(&runtime, &env, db.clone(), &budgets);
    assert_eq!(store.records(&key, None).unwrap().len(), 1);
    // With a larger count budget, only the bytes axis forces eviction.
    let ended_record = lease(&db, "bytes");
    install(
        &runtime,
        &store,
        &key,
        &ended_record,
        ("bytes", 200, -6),
        true,
    );
    ended(&db, ended_record);
    images::cleanup(
        &runtime,
        &env,
        db.clone(),
        &BTreeMap::from([(
            "test".into(),
            ImageBudget {
                max_images: Some(20),
                max_bytes: Some(150),
            },
        )]),
    );
    assert!(!runtime.images.lock().unwrap().contains_key("bytes"));
    // The active set alone exceeds this budget: keep it, warn only once.
    let tiny = BTreeMap::from([(
        "test".into(),
        ImageBudget {
            max_images: Some(0),
            max_bytes: Some(1),
        },
    )]);
    images::cleanup(&runtime, &env, db.clone(), &tiny);
    assert!(runtime.images.lock().unwrap().contains_key("active"));
    assert!(
        !store.budget_warning(&key, "test", true).unwrap(),
        "cleanup already recorded its one warning"
    );
    ended(&db, active);
    images::cleanup(&runtime, &env, db, &tiny);
    assert!(store.records(&key, None).unwrap().is_empty());
    assert!(runtime.images.lock().unwrap().contains_key("not-owned"));
}

#[tokio::test]
async fn cleanup_never_removes_a_retagged_image_and_capability_follows_build_support() {
    let sandbox = TestSandbox::for_test(TestCase::component("command::worker::image_contracts::cleanup_never_removes_a_retagged_image_and_capability_follows_build_support"));
    let runtime = Runtime::default();
    let db = db(&sandbox);
    let old = lease(&db, "retag");
    let store = LeaseImageStore::open(db.clone()).unwrap();
    let env = env(&sandbox);
    let key = images::store_key(&runtime, &env).unwrap().describe();
    install(&runtime, &store, &key, &old, ("retag", 100, -5), true);
    ended(&db, old);
    runtime.images.lock().unwrap().get_mut("retag").unwrap().id = "someone-elses-image".into();
    let mut foreign = store.records(&key, None).unwrap().remove(0);
    foreign.image_id = "someone-elses-image".into();
    store.record(&key, &foreign, false).unwrap();
    assert_eq!(
        store.records(&key, None).unwrap()[0].image_id,
        "id-retag",
        "a cache hit must not adopt a replacement with matching labels"
    );
    images::cleanup(
        &runtime,
        &env,
        db,
        &BTreeMap::from([(
            "test".into(),
            ImageBudget {
                max_images: Some(0),
                max_bytes: None,
            },
        )]),
    );
    assert!(runtime.removed.lock().unwrap().is_empty());
    assert!(store.records(&key, None).unwrap().is_empty());
    let settings =
        crate::data::config::daemon::DaemonSettings::from_merged(&Default::default()).worker;
    let support = crate::engine::worker::lease_workspace::LeaseRuntimeSupport::from_capabilities(
        runtime.capabilities(),
    );
    let caps = super::capabilities::for_runtime(support, &settings, &runtime);
    assert!(caps.contains(&"lease_image_reuse"));
    assert!(
        crate::command::daemon_api::capabilities::advertised(false, &caps)
            .iter()
            .any(|c| c == "lease_image_reuse")
    );
    let importing = crate::engine::image_prep::contracts::Store::default();
    assert!(
        !super::capabilities::for_runtime(support, &settings, &importing)
            .contains(&"lease_image_reuse")
    );
}

#[derive(Default)]
struct CleanSink(Arc<Mutex<Vec<String>>>);
impl crate::data::message::UserMessageSink for CleanSink {
    fn write_message(&mut self, message: crate::data::message::UserMessage) {
        self.0.lock().unwrap().push(message.text);
    }
    fn replay_queued(&mut self) {}
}
impl crate::command::commands::clean::CleanCommandFrontend for CleanSink {
    fn confirm_deletion(
        &mut self,
        _: &crate::command::commands::clean::CleanSummary,
    ) -> Result<bool, crate::command::error::CommandError> {
        panic!("dry run and --yes do not ask")
    }
}

#[tokio::test]
async fn clean_lists_and_removes_only_unused_lease_images() {
    use crate::command::commands::{
        clean::{CleanCommand, CleanFlags},
        Command,
    };
    let sandbox = TestSandbox::for_test(TestCase::component(
        "command::worker::image_contracts::clean_lists_and_removes_only_unused_lease_images",
    ));
    let runtime = Arc::new(Runtime::default());
    let db = db(&sandbox);
    let old = lease(&db, "old");
    let active = lease(&db, "active");
    let store = LeaseImageStore::open(db.clone()).unwrap();
    let session = session(&sandbox, "clean-repo", None);
    let key = images::store_key(runtime.as_ref(), session.env())
        .unwrap()
        .describe();
    install(&runtime, &store, &key, &old, ("cached", 100, -5), true);
    install(&runtime, &store, &key, &active, ("running", 100, -4), true);
    ended(&db, old);
    let mut engines = engines(&session, runtime.clone());
    engines.container_runtime = None; // all image operations use the runtime fake
    let sink = CleanSink::default();
    let messages = sink.0.clone();
    let outcome = CleanCommand::new(
        CleanFlags {
            yes: false,
            dry_run: true,
        },
        engines.clone(),
        session.clone(),
    )
    .run_with_frontend(Box::new(sink))
    .await
    .unwrap();
    assert!(outcome.dry_run);
    assert!(messages
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.contains("Lease images (1)")));
    assert!(messages
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.contains("cached")));
    assert!(runtime.removed.lock().unwrap().is_empty());
    let outcome = CleanCommand::new(
        CleanFlags {
            yes: true,
            dry_run: false,
        },
        engines,
        session,
    )
    .run_with_frontend(Box::new(CleanSink::default()))
    .await
    .unwrap();
    assert_eq!(outcome.deleted, 1);
    assert_eq!(*runtime.removed.lock().unwrap(), ["cached"]);
    assert!(runtime.images.lock().unwrap().contains_key("running"));
}

#[tokio::test]
async fn a_matching_external_image_is_reused_without_becoming_worker_owned() {
    let sandbox = TestSandbox::for_test(TestCase::component("command::worker::image_contracts::a_matching_external_image_is_reused_without_becoming_worker_owned"));
    let runtime = Arc::new(Runtime::default());
    let db = db(&sandbox);
    let record = lease(&db, "external");
    let mut session = session(&sandbox, "clone", Some("FROM scratch\n"));
    let repo = RepoKey::for_repo(Some(&record.item.repo.url), session.git_root());
    let inputs = crate::data::image_inputs::project_inputs(
        Some(b"FROM scratch\n"),
        &crate::data::image_inputs::MemoryBuildContext::new(),
    );
    let tag = crate::data::image_tags::lease_project_image_tag(&repo, &inputs.hash);
    runtime.images.lock().unwrap().insert(
        tag.clone(),
        ImageMetadata {
            id: "external-id".into(),
            tags: vec![tag.clone()],
            labels: BTreeMap::from([
                (INPUTS_LABEL.into(), inputs.hash.as_str().into()),
                (REPO_LABEL.into(), repo.as_str().into()),
            ]),
            size_bytes: 100,
        },
    );
    provision(&sandbox, &mut session, runtime.clone(), db.clone(), &record).await;
    assert_eq!(
        runtime.built.lock().unwrap().len(),
        1,
        "only the agent needed building"
    );
    let store = LeaseImageStore::open(db.clone()).unwrap();
    let key = images::store_key(runtime.as_ref(), session.env())
        .unwrap()
        .describe();
    assert_eq!(store.records(&key, None).unwrap().len(), 1);
    ended(&db, record);
    images::cleanup(
        runtime.as_ref(),
        session.env(),
        db,
        &BTreeMap::from([(
            "test".into(),
            ImageBudget {
                max_images: Some(0),
                max_bytes: None,
            },
        )]),
    );
    assert!(runtime.images.lock().unwrap().contains_key(&tag));
    assert_eq!(runtime.removed.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_build_intents_recover_ownership_and_unresolved_inputs_are_not_shared() {
    let s = TestSandbox::for_test(TestCase::component("command::worker::image_contracts::interrupted_build_intents_recover_ownership_and_unresolved_inputs_are_not_shared"));
    let runtime = Arc::new(Runtime::default());
    let db = db(&s);
    let first = lease(&db, "intent-first");
    let second = lease(&db, "intent-second");
    let mut a = session(&s, "intent-a", Some("FROM scratch\nCOPY *.json /app/\n"));
    let mut b = session(&s, "intent-b", Some("FROM scratch\nCOPY *.json /app/\n"));
    provision(&s, &mut a, runtime.clone(), db.clone(), &first).await;
    provision(&s, &mut b, runtime.clone(), db.clone(), &second).await;
    assert_ne!(a.project_image_tag(), b.project_image_tag());
    assert_eq!(runtime.built.lock().unwrap().len(), 4);
    ended(&db, first);
    ended(&db, second);
    // Simulate losing the final record after the CLI published an image.
    db.with_conn(|conn| {
        conn.execute("DELETE FROM worker_lease_images", [])?;
        Ok(())
    })
    .unwrap();
    let store = LeaseImageStore::open(db.clone()).unwrap();
    let key = images::store_key(runtime.as_ref(), a.env())
        .unwrap()
        .describe();
    assert!(store.records(&key, None).unwrap().is_empty());
    images::cleanup(
        runtime.as_ref(),
        a.env(),
        db.clone(),
        &BTreeMap::from([(
            "test".into(),
            ImageBudget {
                max_images: Some(1),
                max_bytes: None,
            },
        )]),
    );
    assert_eq!(
        runtime.removed.lock().unwrap().len(),
        3,
        "intent images are recovered and subject to budget"
    );
    assert_eq!(store.records(&key, None).unwrap().len(), 1);
    let uses: i64 = db
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM worker_lease_image_uses WHERE lease NOT LIKE 'server:%'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(uses, 0, "completed reservations are compacted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_origins_and_context_copying_agents_have_safe_lease_keys() {
    let s = TestSandbox::for_test(TestCase::component("command::worker::image_contracts::local_origins_and_context_copying_agents_have_safe_lease_keys"));
    let runtime = Arc::new(Runtime::default());
    let db = db(&s);
    let mut first = lease(&db, "local-first");
    let mut second = lease(&db, "local-second");
    first.item.repo.url = format!("file://{}", s.work().display());
    second.item.repo.url = first.item.repo.url.clone();
    let mut a = session(&s, "local-a", Some("FROM scratch\n"));
    let mut b = session(&s, "local-b", Some("FROM scratch\n"));
    let recipe = "ARG BASE=awman-original\nFROM golang AS tools\nFROM ${BASE}\nCOPY --from=tools /tool /tool\n";
    for checkout in [&a, &b] {
        std::fs::write(checkout.git_root().join(".awman/Dockerfile.claude"), recipe).unwrap();
    }
    provision(&s, &mut a, runtime.clone(), db.clone(), &first).await;
    provision(&s, &mut b, runtime.clone(), db.clone(), &second).await;
    assert_eq!(a.lease_images(), b.lease_images());
    assert_eq!(runtime.built.lock().unwrap().len(), 2);
    for checkout in [&a, &b] {
        std::fs::write(
            checkout.git_root().join(".awman/Dockerfile.claude"),
            "FROM awman-original\nCOPY tool /tool\n",
        )
        .unwrap();
    }
    provision(&s, &mut a, runtime.clone(), db.clone(), &first).await;
    provision(&s, &mut b, runtime.clone(), db.clone(), &second).await;
    assert_eq!(a.project_image_tag(), b.project_image_tag());
    assert_ne!(a.agent_image_tag("claude"), b.agent_image_tag("claude"));
    assert_eq!(runtime.built.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn bound_lease_preflight_reuses_prepared_agents_and_refuses_unbound_agents() {
    let s = TestSandbox::for_test(TestCase::component("command::worker::image_contracts::bound_lease_preflight_reuses_prepared_agents_and_refuses_unbound_agents"));
    let runtime = Arc::new(Runtime::default());
    let db = db(&s);
    let lease = lease(&db, "bound-only");
    let mut session = session(&s, "bound-checkout", Some("FROM scratch\n"));
    provision(&s, &mut session, runtime.clone(), db, &lease).await;
    let engines = engines(&session, runtime.clone());
    let paths = crate::data::RepoDockerfilePaths::new(session.git_root());
    let mut sink = CleanSink(Default::default());
    crate::command::commands::workflow_preflight::ensure_agent_image(
        &engines,
        session.git_root(),
        &paths,
        "claude",
        &session,
        &mut sink,
    )
    .await
    .unwrap();
    let error = crate::command::commands::workflow_preflight::ensure_agent_image(
        &engines,
        session.git_root(),
        &paths,
        "codex",
        &session,
        &mut sink,
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("was not provisioned for this lease"));
    assert_eq!(runtime.built.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn slow_cleanup_does_not_hold_the_reservation_lock_or_block_the_executor() {
    let s = TestSandbox::for_test(TestCase::component("command::worker::image_contracts::slow_cleanup_does_not_hold_the_reservation_lock_or_block_the_executor"));
    let runtime = Arc::new(Runtime::default());
    let db = db(&s);
    let ended_lease = lease(&db, "slow-cleanup");
    let mut session = session(&s, "slow-checkout", Some("FROM scratch\n"));
    provision(&s, &mut session, runtime.clone(), db.clone(), &ended_lease).await;
    ended(&db, ended_lease);
    runtime.hold_remove.store(true, Ordering::SeqCst);
    let cleanup = images::schedule_cleanup(
        runtime.clone(),
        session.env().clone(),
        db.clone(),
        BTreeMap::from([(
            "test".into(),
            ImageBudget {
                max_images: Some(1),
                max_bytes: None,
            },
        )]),
    );
    runtime.remove_entered.notified().await;
    // A new preparer may reserve the same tag after removal's last check.
    // Its active reservation and build intent must survive forgetting the old image.
    let next = lease(&db, "reserved-during-removal");
    let store = LeaseImageStore::open(db.clone()).unwrap();
    let key = images::store_key(runtime.as_ref(), session.env())
        .unwrap()
        .describe();
    let tag = runtime.removed.lock().unwrap()[0].clone();
    let intent = store
        .records(&key, None)
        .unwrap()
        .into_iter()
        .find(|record| record.tag == tag)
        .unwrap();
    store.reserve(&key, &tag, &next.id, &next.server).unwrap();
    store.intent(&key, &intent).unwrap();
    let data_home = crate::data::fs::DataPaths::from_env(session.env())
        .unwrap()
        .root()
        .to_path_buf();
    let locked = tokio::task::spawn_blocking(move || LeaseImageStore::lock(&data_home).map(drop));
    let responsive = tokio::time::timeout(Duration::from_millis(500), locked).await;
    *runtime.release_remove.0.lock().unwrap() = true;
    runtime.release_remove.1.notify_all();
    cleanup.await.unwrap();
    assert!(
        store.in_use(&key).unwrap().contains(&tag),
        "forgetting the old image must preserve the new lease's reservation"
    );
    assert!(
        store
            .intents(&key)
            .unwrap()
            .iter()
            .any(|record| record.tag == tag),
        "an interrupted replacement build must remain recoverable"
    );
    assert!(
        responsive.is_ok(),
        "removal cannot hold the global state lock"
    );
}

#[test]
fn persistent_docker_context_selection_is_shared_by_build_and_cleanup_keys() {
    let s = TestSandbox::for_test(TestCase::component("command::worker::image_contracts::persistent_docker_context_selection_is_shared_by_build_and_cleanup_keys"));
    let config = s.work().join("docker-config");
    std::fs::create_dir(&config).unwrap();
    let mut values: std::collections::HashMap<String, String> = s
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    values.remove("DOCKER_HOST");
    values.remove("DOCKER_CONTEXT");
    values.insert(
        "DOCKER_CONFIG".into(),
        config.to_string_lossy().into_owned(),
    );
    let env = EnvSnapshot::from_full_env(values.into_iter().collect());
    let runtime = Runtime::default();
    std::fs::write(config.join("config.json"), r#"{"currentContext":"first"}"#).unwrap();
    let first = images::store_key(&runtime, &env).unwrap();
    assert_eq!(
        first,
        crate::engine::agent::image_freshness::runtime_build_store(&runtime, &env)
    );
    std::fs::write(config.join("config.json"), r#"{"currentContext":"second"}"#).unwrap();
    assert_ne!(first, images::store_key(&runtime, &env).unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn lease_dockerfiles_cannot_read_symlinks_or_paths_outside_the_checkout() {
    let s = TestSandbox::for_test(TestCase::component("command::worker::image_contracts::lease_dockerfiles_cannot_read_symlinks_or_paths_outside_the_checkout"));
    let runtime = Arc::new(Runtime::default());
    let db = db(&s);
    let lease = lease(&db, "symlink-recipe");
    let mut session = session(&s, "symlink-checkout", Some("FROM scratch\n"));
    let outside = s.tmp().join("private-recipe");
    std::fs::write(&outside, "dummy-private-content").unwrap();
    for relative in ["Dockerfile.dev", ".awman/Dockerfile.claude"] {
        let path = session.git_root().join(relative);
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        let private = s.work().join("private-symlink-build");
        std::fs::create_dir_all(&private).unwrap();
        let error = super::provision::agents_for_lease(
            &engines(&session, runtime.clone()),
            &mut session,
            &lease.item,
            &private,
            db.clone(),
            &lease,
        )
        .await
        .unwrap_err();
        assert!(!error.to_string().contains("dummy-private-content"));
        assert!(!private.join("Dockerfile.claude").exists());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "FROM scratch\n").unwrap();
    }
}
