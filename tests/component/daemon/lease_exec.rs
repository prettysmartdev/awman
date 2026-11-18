//! WI 0162: lease exec parity, end to end through the daemon's pull worker.
//!
//! The production daemon registers with the awman-wsp mock work server,
//! claims, clones over loopback Git HTTP and runs every step through the fake
//! `docker` (which records each invocation's argv and environment).
//! Assertions read what the server received (registration labels, events,
//! result, release), what the fake runtime was given, and every file the
//! daemon left under the private home.
#![cfg(unix)]
use super::support::Api;
use crate::support;
use awman::command::daemon_api::{keys, worker_pull};
use awman::data::fs::data_paths::DataPaths;
use awman::data::ids::{CommandId, LeaseId};
use awman::data::worker::wire::LeaseResource;
use awman::data::worker::{lease_store::LeaseStore, link_store::LinkStore, LeaseState};
use awman_test_support::capability::{LocalGit, Loopback};
use awman_test_support::fakes::{Call, FakeCli, FakeGitHttp, FakeWorkServer};
use awman_test_support::fixtures::RepoBuilder;
use awman_test_support::{wait_for_async, TestSandbox};
use awman_wsp::mock::MockWorkServer;
use awman_wsp::{secret::SecretString, ExecSecrets, WorkItem, WorkKind};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SECRET: &str = "test-secret-lease-exec-requested";
const UNUSED: &str = "test-secret-lease-exec-unrequested";
const ALLOWED_VALUE: &str = "test-secret-lease-exec-worker-allowed";
const DENIED_VALUE: &str = "test-secret-lease-exec-worker-denied";

/// One daemon with pull enabled, linked to one mock work server (`mock`).
struct Host {
    s: TestSandbox,
    seed: PathBuf,
    http: FakeGitHttp,
    wsp: FakeWorkServer,
    server: Option<Api>,
    docker: FakeCli,
    key: String,
    url: String,
    config: Value,
}

struct Options {
    mock: MockWorkServer,
    /// Merged into the `workServers.mock` entry.
    server: Value,
    /// Merged into the global config.
    extra: Value,
    pull: bool,
    /// Worker (host) variables, set before the daemon captures its
    /// environment.
    vars: Vec<(&'static str, &'static str)>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mock: MockWorkServer::new(),
            server: json!({}),
            extra: json!({}),
            pull: true,
            vars: Vec::new(),
        }
    }
}

impl Host {
    /// `prepare` runs on the private home before the daemon boots
    /// (registration reads installed skills); `rules` add fake `docker`
    /// behavior on top of a stateful image store and successful builds/agents.
    ///
    /// hygiene-allow(H1): the daemon resolves `docker`, its temp root and worker variables from its process environment; nextest gives each test its own process, and every value points into this test's sandbox (as `daemon::init_api`).
    /// hygiene-allow(H7): reads the process PATH only to prepend this sandbox's fake `bin` to it.
    async fn start(
        id: &'static str,
        options: Options,
        prepare: impl FnOnce(&TestSandbox),
        rules: impl FnOnce(&FakeCli),
    ) -> Host {
        let mut s = support::sandbox(id);
        s.require::<LocalGit>();
        s.require::<Loopback>();
        for value in [SECRET, UNUSED, ALLOWED_VALUE, DENIED_VALUE] {
            s.redact(value);
        }
        // The daemon resolves `docker` and worker variables from this
        // process (nextest runs each test in its own process), as
        // `daemon::init_api` does.
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", s.bin().display()));
        let temp_root = s.root().display().to_string();
        for key in ["TMPDIR", "TMP", "TEMP"] {
            s.env_mut().set(key, &temp_root);
            std::env::set_var(key, &temp_root);
        }
        let endpoint = format!("unix://{}", s.work().join("fake-engine.sock").display());
        s.env_mut().set("DOCKER_HOST", &endpoint);
        std::env::set_var("DOCKER_HOST", &endpoint);
        for (name, value) in &options.vars {
            s.env_mut().set(name, value);
            std::env::set_var(name, value);
        }
        let repo = RepoBuilder::new(&s, "seed").file("a", "a").init();
        repo.bare_remote("origin");
        let seed = repo.path().to_path_buf();
        drop(repo);
        let http = FakeGitHttp::builder(&s, &s.root().join("remotes")).start();
        let url = http.url("origin.git");
        let wsp = FakeWorkServer::start_with(&s, std::sync::Arc::new(options.mock)).await;
        let link = wsp.server().link();
        wsp.server().approve_link(&link.link_id, BTreeMap::new());
        let token = awman_wsp::Client::new(wsp.base_url(), String::new())
            .unwrap()
            .poll_link(&link.link_id)
            .await
            .unwrap()
            .expect("approved link");
        s.redact(&token.refresh_token);
        LinkStore::new(&DataPaths::from_env(&support::env_snapshot(&s)).unwrap())
            .write("mock", &token)
            .unwrap();
        prepare(&s);
        let mut entry = json!({"url": wsp.base_url(), "claim": "manual", "maxConcurrentJobs": 2});
        super::support::merge(&mut entry, options.server);
        let mut config = json!({"daemon":{"features":{"worker":{"enabled":true,
                "push":{"enabled":false},
                "pull":{"enabled":options.pull,"interaction":"headless"}}},
            "network":{"listeners":{"component":{"features":["engine","worker"]}}}},
            "workServers":{"mock": entry}});
        super::support::merge(&mut config, options.extra);
        let mut fake = None;
        let api = Api::with_docker(&s, config.clone(), |docker| {
            docker.enable_docker_image_store();
            docker.on(&["build"]).stdout("lease image built\n");
            docker.on(&["rmi"]);
            docker.on(&["run"]).stdout("agent finished\n");
            // Recovery and cancellation look for and stop owned containers.
            docker.on(&["ps"]);
            docker.on(&["stop"]);
            docker.on(&["rm"]);
            docker.on(&["kill"]);
            rules(docker);
            fake = Some(docker.clone());
        })
        .await;
        let key = keys::keys_create(&api.api, "lease-exec").unwrap();
        s.redact(&key.key);
        let host = Host {
            s,
            seed,
            http,
            wsp,
            server: Some(api),
            docker: fake.expect("fake docker installed"),
            key: key.key,
            url,
            config,
        };
        if options.pull {
            host.registered().await;
            host.accepting().await;
        }
        host
    }

    fn api(&self) -> &Api {
        self.server.as_ref().expect("daemon running")
    }

    async fn registered(&self) -> worker_pull::ServerStatus {
        wait_for_async(
            "work server registered",
            Duration::from_secs(15),
            || async {
                worker_pull::servers_get(&self.api().api, "mock")
                    .ok()
                    .filter(|status| status.state == "registered")
            },
        )
        .await
    }

    /// The pull worker registers during startup recovery and starts claiming
    /// just after. A claim of an item the server does not have reaches the
    /// server (`item_not_found`) only once it does; nothing is leased.
    async fn accepting(&self) {
        wait_for_async(
            "the pull worker accepts claims",
            Duration::from_secs(15),
            || async {
                match worker_pull::queue_claim(&self.api().api, "readiness-probe", Some("mock"))
                    .await
                {
                    Err(error) if error.code() == "pull_disabled" => None,
                    Err(error) => {
                        assert_eq!(error.code(), "item_not_found", "{error}");
                        Some(())
                    }
                    Ok(lease) => panic!("unexpected lease {}", lease.id),
                }
            },
        )
        .await
    }

    fn item(&self, id: &str) -> WorkItem {
        let mut item = awman_wsp::conformance::work_item(id);
        item.repo.url = self.url.clone();
        item.exec.agent = Some("claude".into());
        item.exec.timeout_ms = None;
        item
    }

    fn workflow(&self, id: &str, toml: &str) -> WorkItem {
        let mut item = self.item(id);
        item.kind = WorkKind::Workflow;
        item.exec.workflow_toml = Some(toml.into());
        item
    }

    fn dynamic(&self, id: &str) -> WorkItem {
        let mut item = self.item(id);
        item.kind = WorkKind::Workflow;
        item.exec.dynamic = true;
        item.exec.agent = None;
        item
    }

    /// Enqueue and claim `item` manually; `Err` is the claim refusal's code
    /// and message.
    async fn claim(&self, item: WorkItem) -> Result<LeaseResource, (String, String)> {
        let id = item.id.clone();
        self.wsp.server().enqueue(item, chrono::Utc::now()).unwrap();
        worker_pull::queue_claim(&self.api().api, &id, Some("mock"))
            .await
            .map_err(|error| (error.code().to_string(), error.to_string()))
    }

    /// The release the server recorded for the latest refused claim. (The
    /// mock numbers lease IDs `server_<n>` in issue order and records
    /// releases by lease ID.)
    fn refused(&self, item: &str) -> Value {
        let mock = self.wsp.server();
        let release = (0..10_000)
            .rev()
            .find_map(|n| mock.release_for(&format!("server_{n}")))
            .unwrap_or_else(|| panic!("{item}: no release"));
        serde_json::to_value(release).unwrap()
    }

    /// Wait for the server to receive the lease's terminal report.
    async fn terminal(&self, lease: &LeaseResource) -> Terminal {
        let mock = self.wsp.server();
        wait_for_async("lease reported", Duration::from_secs(30), || async {
            match (
                mock.result_for(&lease.item_id),
                mock.release_for(&lease.server_lease_id),
            ) {
                (None, None) => None,
                (result, release) => Some(Terminal {
                    result: result.map(|r| serde_json::to_value(r).unwrap()),
                    release: release.map(|r| serde_json::to_value(r).unwrap()),
                    events: Value::Array(mock.events_for(&lease.server_lease_id)),
                }),
            }
        })
        .await
    }

    /// Agent containers started so far (not probes).
    fn runs(&self) -> Vec<Call> {
        self.docker
            .calls()
            .into_iter()
            .filter(|call| call.argv.first().map(String::as_str) == Some("run"))
            .filter(|call| !call.argv.iter().any(|a| a == "--entrypoint"))
            .collect()
    }

    fn leases(&self) -> LeaseStore {
        LeaseStore::open(self.api().api.worker.as_ref().unwrap().store.clone()).unwrap()
    }

    async fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.shutdown().await;
        }
    }

    /// Stop everything; `check` reads the private state before cleanup.
    async fn finish(mut self, check: impl FnOnce(&Path, &Path)) {
        self.stop().await;
        check(self.s.root(), self.docker.dir());
        self.wsp.shutdown().await;
        drop(self.http);
        self.s.finish();
    }
}

struct Terminal {
    result: Option<Value>,
    release: Option<Value>,
    events: Value,
}

fn secrets(pairs: &[(&str, &str)]) -> ExecSecrets {
    ExecSecrets::new(
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), SecretString::new(*value))),
    )
    .unwrap()
}

/// The host directory mounted at `/workspace` (the lease's clone).
fn workspace_mount(call: &Call) -> PathBuf {
    call.argv
        .windows(2)
        .find_map(|pair| {
            (pair[0] == "-v")
                .then(|| pair[1].strip_suffix(":/workspace"))
                .flatten()
        })
        .map(PathBuf::from)
        .expect("a /workspace mount")
}

/// The names passed with `-e` (never `NAME=value` for a secret).
fn env_args(call: &Call) -> Vec<&str> {
    call.argv
        .windows(2)
        .filter(|pair| pair[0] == "-e")
        .map(|pair| pair[1].as_str())
        .collect()
}

fn mounts(call: &Call) -> Vec<&str> {
    call.argv
        .windows(2)
        .filter(|pair| pair[0] == "-v")
        .map(|pair| pair[1].as_str())
        .collect()
}

fn image(call: &Call) -> &str {
    call.argv
        .iter()
        .find(|arg| arg.starts_with("awman-") && arg.ends_with(":latest"))
        .expect("an agent image")
}

/// No file under `root` holds `needle`, except the fake runtime's record of
/// the environment it was started with (the container's own environment).
fn assert_not_persisted(root: &Path, fake: &Path, needle: &str) {
    let mut stack = vec![root.to_path_buf()];
    let mut scanned = 0;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if path != fake {
                    stack.push(path);
                }
            } else if kind.is_file() {
                scanned += 1;
                let bytes = std::fs::read(&path).unwrap_or_default();
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                    "{} holds a lease secret value",
                    path.display()
                );
            }
        }
    }
    assert!(scanned > 0, "nothing was scanned under {}", root.display());
}

const TWO_STEPS: &str = r#"
[[steps]]
name = "plan"
prompt = "plan the change"

[[steps]]
name = "build"
prompt = "build the change"
depends_on = ["plan"]
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_secret_reaches_every_step_by_name_and_is_persisted_or_posted_nowhere() {
    let host = Host::start(
        "daemon::lease_exec::a_lease_secret_reaches_every_step_by_name_and_is_persisted_or_posted_nowhere",
        Options::default(),
        |_| {},
        // Each agent prints both values: the requested and the unrequested
        // secret must be redacted from everything the server receives.
        |docker| {
            docker
                .on(&["run"])
                .stdout(&format!("agent saw {SECRET} and {UNUSED}\n"));
        },
    )
    .await;
    let mut item = host.workflow("secret-steps", TWO_STEPS);
    item.exec.secrets = secrets(&[("TOKEN", SECRET), ("UNUSED_TOKEN", UNUSED)]);
    item.exec.overlays = vec!["env(TOKEN)".into()];
    let lease = host.claim(item).await.expect("admitted");
    let done = host.terminal(&lease).await;
    let result = done.result.expect("a result, not a release");
    assert!(done.release.is_none(), "{:?}", done.release);
    assert_eq!(result["result"]["outcome"], "done", "{result}");

    // Every step container asked for TOKEN by name and got its value only in
    // its environment; the unrequested secret reached no container.
    let runs = host.runs();
    assert_eq!(runs.len(), 2, "one container per step: {runs:#?}");
    for run in &runs {
        assert!(env_args(run).contains(&"TOKEN"), "{:?}", run.argv);
        assert_eq!(run.env.get("TOKEN").map(String::as_str), Some(SECRET));
        assert!(!run.env.contains_key("UNUSED_TOKEN"));
    }
    // No process the worker started had a value in its argv (what `ps`
    // shows), and only agent containers had one in their environment.
    for call in host.docker.calls() {
        for value in [SECRET, UNUSED] {
            assert!(
                !call.argv.iter().any(|arg| arg.contains(value)),
                "{:?}",
                call.argv
            );
        }
        if !runs.contains(&call) {
            assert!(!call.env.values().any(|v| v.contains(SECRET)));
        }
    }
    // Neither value reached the server; the agent output did, redacted.
    let posted = format!("{} {result}", done.events);
    for value in [SECRET, UNUSED] {
        assert!(!posted.contains(value), "posted: {posted}");
    }
    assert!(posted.contains("agent saw *** and ***"), "{posted}");
    // The lease store keeps names only; no value is left in memory.
    let record = host
        .leases()
        .get(&LeaseId::parse(&lease.id).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(record.secret_names, ["TOKEN", "UNUSED_TOKEN"]);
    let job = CommandId::parse(record.job_id.as_deref().expect("a job")).unwrap();
    assert!(host.api().api.engines.ephemeral_secrets.get(&job).is_none());
    // The clone's `.git/config` never held a value while the run used it.
    let clone = workspace_mount(&runs[0]);
    assert!(!clone.exists(), "the clone is removed after the lease");
    host.finish(|root, fake| {
        // Lease store, job store and command rows, command logs, outputs,
        // clones and every other file under the private home.
        for value in [SECRET, UNUSED] {
            assert_not_persisted(root, fake, value);
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_clone_never_holds_a_secret_while_the_run_uses_it() {
    let host = Host::start(
        "daemon::lease_exec::the_clone_never_holds_a_secret_while_the_run_uses_it",
        Options::default(),
        |_| {},
        |docker| {
            docker.on(&["run"]).hold_until_released();
        },
    )
    .await;
    let mut item = host.item("secret-clone");
    item.exec.secrets = secrets(&[("TOKEN", SECRET)]);
    item.exec.overlays = vec!["env(TOKEN)".into()];
    let lease = host.claim(item).await.expect("admitted");
    let held = host.docker.wait_until_held(Duration::from_secs(20));
    let clone = workspace_mount(&held);
    let git_config = std::fs::read_to_string(clone.join(".git/config")).unwrap();
    assert!(!git_config.contains(SECRET), "{git_config}");
    // While running, the in-memory value exists and the lease store holds
    // only its name.
    let record = host
        .leases()
        .get(&LeaseId::parse(&lease.id).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(record.state, LeaseState::Running);
    assert_eq!(record.secret_names, ["TOKEN"]);
    assert_not_persisted(&clone, host.docker.dir(), SECRET);
    host.docker.release();
    let done = host.terminal(&lease).await;
    assert_eq!(done.result.expect("result")["result"]["outcome"], "done");
    host.finish(|root, fake| assert_not_persisted(root, fake, SECRET))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_variables_need_allow_env_and_other_overlay_kinds_fail_closed_by_name() {
    let host = Host::start(
        "daemon::lease_exec::worker_variables_need_allow_env_and_other_overlay_kinds_fail_closed_by_name",
        Options {
            server: json!({"allowEnv": ["LEASE_EXEC_ALLOWED", "LEASE_EXEC_UNSET"]}),
            vars: vec![
                ("LEASE_EXEC_ALLOWED", ALLOWED_VALUE),
                ("LEASE_EXEC_DENIED", DENIED_VALUE),
            ],
            ..Options::default()
        },
        |_| {},
        |_| {},
    )
    .await;
    // An allowed, set worker variable is injected by name.
    let mut item = host.item("allowed-var");
    item.exec.overlays = vec!["env(LEASE_EXEC_ALLOWED)".into()];
    let lease = host.claim(item).await.expect("admitted");
    let done = host.terminal(&lease).await;
    assert_eq!(done.result.expect("result")["result"]["outcome"], "done");
    let runs = host.runs();
    assert_eq!(runs.len(), 1);
    assert!(env_args(&runs[0]).contains(&"LEASE_EXEC_ALLOWED"));
    assert_eq!(
        runs[0].env.get("LEASE_EXEC_ALLOWED").map(String::as_str),
        Some(ALLOWED_VALUE)
    );
    // The docker client inherits the worker's environment; only `-e NAME`
    // passes a variable into the container.
    assert!(!env_args(&runs[0]).contains(&"LEASE_EXEC_DENIED"));
    assert!(!runs[0].argv.iter().any(|a| a.contains(ALLOWED_VALUE)));

    // Refusals are `ineligible`, name what was refused, never echo a host
    // value, and start nothing.
    let cases: &[(&str, &str, &str)] = &[
        ("denied-var", "env(LEASE_EXEC_DENIED)", "LEASE_EXEC_DENIED"),
        ("unset-var", "env(LEASE_EXEC_UNSET)", "LEASE_EXEC_UNSET"),
        (
            "lower-var",
            "env(lease_exec_allowed)",
            "env(lease_exec_allowed)",
        ),
        ("dir-kind", "dir(/etc)", "'dir'"),
        ("ssh-kind", "ssh()", "'ssh'"),
        ("bare-mount", "/etc:/host-etc", "/etc:/host-etc"),
        ("context-kind", "context()", "'context'"),
        ("unknown-kind", "custom(thing)", "'custom'"),
        (
            "list",
            "env(LEASE_EXEC_ALLOWED),env(LEASE_EXEC_DENIED)",
            "env(",
        ),
    ];
    for (id, overlay, named) in cases {
        let mut item = host.item(id);
        item.exec.overlays = vec![(*overlay).into()];
        let (code, message) = host.claim(item).await.expect_err(id);
        assert_eq!(code, "ineligible", "{id}: {message}");
        assert!(message.contains(named), "{id}: {message}");
        let release = host.refused(id);
        assert_eq!(release["reason"], "ineligible", "{id}: {release}");
        let text = release["message"].as_str().unwrap_or_default();
        assert!(text.contains(named), "{id}: {text}");
        for value in [ALLOWED_VALUE, DENIED_VALUE] {
            assert!(!text.contains(value) && !message.contains(value), "{id}");
        }
    }
    assert_eq!(host.runs().len(), 1, "no refused lease started a container");
    host.finish(|root, fake| {
        assert_not_persisted(root, fake, DENIED_VALUE);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manually_picked_foreground_leases_refuse_every_exec_feature() {
    let host = Host::start(
        "daemon::lease_exec::manually_picked_foreground_leases_refuse_every_exec_feature",
        Options {
            server: json!({"allowEnv": ["LEASE_EXEC_ALLOWED"]}),
            ..Options::default()
        },
        |_| {},
        |_| {},
    )
    .await;
    let mut with_secret = host.item("fg-secret");
    with_secret.exec.secrets = secrets(&[("TOKEN", SECRET)]);
    let mut with_prereq = host.workflow("fg-prereq", TWO_STEPS);
    with_prereq.exec.prerequisites = Some(awman_wsp::ExecPrerequisites {
        git_identity: None,
        ssh: Some(awman_wsp::ExecCredentialChoice::Deny),
        github_token: None,
    });
    for (item, feature) in [
        (with_secret, "lease_secrets"),
        (with_prereq, "lease_prerequisites"),
    ] {
        let id = item.id.clone();
        host.wsp.server().enqueue(item, chrono::Utc::now()).unwrap();
        let error = worker_pull::queue_prepare_foreground(&host.api().api, &id, Some("mock"))
            .await
            .err()
            .unwrap_or_else(|| panic!("{id} was prepared"));
        assert_eq!(error.code(), "ineligible", "{id}: {error}");
        assert!(error.to_string().contains(feature), "{id}: {error}");
        let release = host.refused(&id);
        assert_eq!(release["reason"], "ineligible");
        assert!(!release.to_string().contains(SECRET));
    }
    assert!(host.runs().is_empty());
    host.finish(|root, fake| assert_not_persisted(root, fake, SECRET))
        .await;
}

fn install_skill(s: &TestSandbox, name: &str) {
    let dir = s.awman_home().join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: test skill\n---\nUse it.\n"),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewed_skills_are_labelled_mounted_and_missing_or_unlisted_ones_refused() {
    let host = Host::start(
        "daemon::lease_exec::reviewed_skills_are_labelled_mounted_and_missing_or_unlisted_ones_refused",
        Options {
            server: json!({"allowSkills": ["planner", "absent", "team"]}),
            ..Options::default()
        },
        |s| {
            install_skill(s, "planner");
            install_skill(s, "unlisted");
            let library = s.awman_home().join("skills/.library/team");
            std::fs::create_dir_all(library.join("skills/planner")).unwrap();
            std::fs::write(library.join("skills/planner/SKILL.md"), "team planner").unwrap();
            awman::data::fs::skill_library::write_library_meta(&library,
                &awman::data::fs::skill_library::SkillLibraryMeta {
                    source: "https://github.com/example/team".into(),
                    owner: "example".into(), repo: "team".into(),
                    subdir: "skills".into(), sparse: true,
                }).unwrap();

        },
        |_| {},
    )
    .await;
    // The worker registers a reserved label per allowed, installed skill and
    // none for an unlisted or missing one.
    let status = host.registered().await;
    assert_eq!(
        status.labels.get("awman/skill.planner").map(String::as_str),
        Some("true"),
        "{:?}",
        status.labels
    );
    assert!(!status.labels.contains_key("awman/skill.unlisted"));
    assert!(!status.labels.contains_key("awman/skill.absent"));
    assert_eq!(
        status
            .labels
            .get("awman/skill.team.planner")
            .map(String::as_str),
        Some("true")
    );
    assert!(!status.labels.contains_key("awman/skill.team.missing"));
    // The server matches on the label it received at registration.

    let mut targeted = host.item("needs-planner");
    targeted.exec.overlays = vec!["skill(planner)".into(), "skill(team/planner)".into()];
    targeted.affinity = serde_json::from_value(json!({"required":[
        {"key":"awman/skill.team.planner","op":"In","values":["true"]}]}))
    .unwrap();
    let mut elsewhere = host.item("needs-unlisted");
    elsewhere.affinity = serde_json::from_value(json!({"required":[
        {"key":"awman/skill.unlisted","op":"Exists"}]}))
    .unwrap();
    host.wsp
        .server()
        .enqueue(elsewhere, chrono::Utc::now())
        .unwrap();
    let error = worker_pull::queue_claim(&host.api().api, "needs-unlisted", Some("mock"))
        .await
        .expect_err("a label this worker never reported");
    assert_eq!(error.code(), "ineligible", "{error}");

    let lease = host.claim(targeted).await.expect("admitted");
    let done = host.terminal(&lease).await;
    assert_eq!(done.result.expect("result")["result"]["outcome"], "done");
    let runs = host.runs();
    assert_eq!(runs.len(), 1);
    let source = host.s.awman_home().join("skills/planner");
    assert!(
        mounts(&runs[0])
            .iter()
            .any(|m| m.starts_with(&format!("{}:", source.display()))),
        "{:?}",
        runs[0].argv
    );
    assert!(!mounts(&runs[0]).iter().any(|m| m.contains("unlisted")));
    assert!(mounts(&runs[0])
        .iter()
        .any(|m| m.contains(".library/team/skills/planner")));

    for (id, skill, named) in [
        ("missing-skill", "skill(absent)", "absent is not installed"),
        (
            "unlisted-skill",
            "skill(unlisted)",
            "unlisted is not in the default skill library or allowSkills",
        ),
        ("wildcard-skill", "skill(*)", "skill(*)"),
        ("escaping-skill", "skill(../planner)", "skill(../planner)"),
    ] {
        let mut item = host.item(id);
        item.exec.overlays = vec![skill.into()];
        let (code, message) = host.claim(item).await.expect_err(id);
        assert_eq!(code, "ineligible", "{id}: {message}");
        assert!(message.contains(named), "{id}: {message}");
        assert_eq!(host.refused(id)["reason"], "ineligible");
    }
    assert_eq!(host.runs().len(), 1);
    host.finish(|_, _| {}).await;
}

const ONE_STEP: &str = r#"
[[steps]]
name = "change"
prompt = "make the change"
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_commit_author_follows_exec_prerequisites_and_widening_is_refused() {
    let host = Host::start(
        "daemon::lease_exec::the_commit_author_follows_exec_prerequisites_and_widening_is_refused",
        Options {
            // Cover the whole component test budget, including the two-
            // heartbeat push margin. This tests commit identity, not whether
            // several real Git processes finish in the mock's one-second
            // push window on every runner.
            mock: MockWorkServer::new().with_lease_ttl(chrono::Duration::seconds(30)),
            ..Options::default()
        },
        |_| {},
        |docker| {
            docker.on(&["run"]).hold_until_released();
        },
    )
    .await;
    let identity = |name: &str, email: &str| {
        Some(awman_wsp::ExecGitIdentity {
            name: name.into(),
            email: email.into(),
        })
    };
    let mut item = host.workflow("authored", ONE_STEP);
    item.push = Some(awman_wsp::Push {
        enabled: true,
        branch: String::new(),
    });
    item.exec.prerequisites = Some(awman_wsp::ExecPrerequisites {
        git_identity: identity("Lease Author", "lease-author@example.invalid"),
        ssh: Some(awman_wsp::ExecCredentialChoice::Deny),
        github_token: Some(awman_wsp::ExecCredentialChoice::Deny),
    });
    let branch = item.repo.branch.clone();
    let lease = host.claim(item).await.expect("admitted");
    // The agent leaves a change in the clone; the worker commits it.
    let held = host.docker.wait_until_held(Duration::from_secs(20));
    std::fs::write(workspace_mount(&held).join("leftover.txt"), "agent work").unwrap();
    host.docker.release();
    let done = host.terminal(&lease).await;
    let result = done.result.expect("result");
    assert_eq!(result["result"]["outcome"], "done", "{result}");
    // The run reported its prerequisite resolution to the server.
    assert!(
        done.events
            .to_string()
            .contains("\"workflow_prerequisites\""),
        "{}",
        done.events
    );
    let git = |args: &[&str]| {
        host.s
            .command("git")
            .arg("-C")
            .arg(&host.seed)
            .args(args)
            .label("git")
            .run()
            .stdout_text()
    };
    git(&["fetch", "origin", &branch]);
    assert_eq!(
        git(&["log", "-1", "--format=%an <%ae>|%cn <%ce>", "FETCH_HEAD"]).trim(),
        "Lease Author <lease-author@example.invalid>|Lease Author <lease-author@example.invalid>"
    );

    // Widening the worker's own policy is refused before any clone or run.
    let runs = host.runs().len();
    let mut wider = host.workflow("wider", ONE_STEP);
    wider.exec.prerequisites = Some(awman_wsp::ExecPrerequisites {
        git_identity: None,
        ssh: Some(awman_wsp::ExecCredentialChoice::Allow),
        github_token: None,
    });
    let (code, message) = host.claim(wider).await.expect_err("widening");
    assert_eq!(code, "ineligible", "{message}");
    assert!(
        message.contains("prerequisite_widening_refused: exec.prerequisites.ssh=allow"),
        "{message}"
    );
    let release = host.refused("wider");
    assert_eq!(release["reason"], "ineligible");
    assert!(release["message"]
        .as_str()
        .unwrap()
        .starts_with("prerequisite_widening_refused: "));
    // Prerequisites apply to workflows only.
    let mut prompt = host.item("prompt-prereq");
    prompt.exec.prerequisites = Some(awman_wsp::ExecPrerequisites {
        git_identity: identity("Lease Author", "lease-author@example.invalid"),
        ssh: None,
        github_token: None,
    });
    let (code, _) = host.claim(prompt).await.expect_err("prompt");
    assert_eq!(code, "ineligible");
    assert_eq!(host.runs().len(), runs);
    host.finish(|_, _| {}).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fixed_identity_is_widening_where_the_worker_denies_identities() {
    let host = Host::start(
        "daemon::lease_exec::a_fixed_identity_is_widening_where_the_worker_denies_identities",
        Options {
            extra: json!({"workflows":{"prerequisites":{"gitIdentity":{"use":"deny"}}}}),
            ..Options::default()
        },
        |_| {},
        |_| {},
    )
    .await;
    let mut item = host.workflow("denied-identity", ONE_STEP);
    item.exec.prerequisites = Some(awman_wsp::ExecPrerequisites {
        git_identity: Some(awman_wsp::ExecGitIdentity {
            name: "Lease Author".into(),
            email: "lease-author@example.invalid".into(),
        }),
        ssh: None,
        github_token: None,
    });
    let (code, message) = host.claim(item).await.expect_err("widening");
    assert_eq!(code, "ineligible");
    assert!(
        message.contains("prerequisite_widening_refused: "),
        "{message}"
    );
    assert!(host.runs().is_empty());
    host.finish(|_, _| {}).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_workflows_dynamic_changes_the_leader_and_mapping_for_one_item_only() {
    let host = Host::start(
        "daemon::lease_exec::exec_workflows_dynamic_changes_the_leader_and_mapping_for_one_item_only",
        Options {
            extra: json!({"workflows":{"dynamic":{
                "defaultLeader":"claude::worker-model",
                "agentsToModels":{"claude":["worker-step-model"]},
                "guidance":["worker guidance"]}}}),
            ..Options::default()
        },
        |_| {},
        |docker| {
            docker.on(&["run"]).read_stdin();
        },
    )
    .await;
    let mut item = host.dynamic("per-item-dynamic");
    item.exec.workflows_dynamic = Some(awman_wsp::ExecWorkflowsDynamic {
        default_leader: Some("codex::lease-model".into()),
        agents_to_models: Some(BTreeMap::from([(
            "codex".into(),
            vec!["lease-step-model".into()],
        )])),
        guidance: Some(vec!["lease guidance".into()]),
    });
    let lease = host.claim(item).await.expect("admitted");
    host.terminal(&lease).await;
    let first = host.runs();
    assert!(!first.is_empty(), "the leader started");
    let leader = &first[0];
    assert!(
        image(leader).ends_with("-codex:latest"),
        "{:?}",
        leader.argv
    );
    let seen = format!(
        "{:?} {}",
        leader.argv,
        String::from_utf8_lossy(&leader.stdin)
    );
    // The leader runs the item's model and plans with the item's mapping;
    // guidance is appended to the worker's own.
    assert!(leader
        .argv
        .windows(2)
        .any(|p| p == ["--model", "lease-model"]));
    for expected in [
        "  - codex: lease-step-model",
        "lease guidance",
        "worker guidance",
    ] {
        assert!(seen.contains(expected), "{expected}: {seen}");
    }
    assert!(!seen.contains("worker-model") && !seen.contains("worker-step-model"));

    // The next item sees the worker's own configuration again.
    let lease = host
        .claim(host.dynamic("worker-dynamic"))
        .await
        .expect("admitted");
    host.terminal(&lease).await;
    let second = &host.runs()[first.len()];
    assert!(
        image(second).ends_with("-claude:latest"),
        "{:?}",
        second.argv
    );
    let seen = format!(
        "{:?} {}",
        second.argv,
        String::from_utf8_lossy(&second.stdin)
    );
    assert!(second
        .argv
        .windows(2)
        .any(|p| p == ["--model", "worker-model"]));
    for expected in ["  - claude: worker-step-model", "worker guidance"] {
        assert!(seen.contains(expected), "{expected}: {seen}");
    }
    for leaked in ["lease-model", "lease-step-model", "lease guidance"] {
        assert!(!seen.contains(leaked), "{leaked}: {seen}");
    }
    // Nothing was written to the worker's config file.
    let config = std::fs::read_to_string(
        awman::data::config::file::global_config_path(&support::env_snapshot(&host.s)).unwrap(),
    )
    .unwrap();
    assert!(!config.contains("lease-model"));
    host.finish(|_, _| {}).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_mid_lease_releases_credentials_lost_and_reruns_nothing() {
    let mut host = Host::start(
        "daemon::lease_exec::a_restart_mid_lease_releases_credentials_lost_and_reruns_nothing",
        Options::default(),
        |_| {},
        |docker| {
            docker.on(&["run"]).hold_until_released();
        },
    )
    .await;
    let mut item = host.item("restarted");
    item.exec.secrets = secrets(&[("TOKEN", SECRET)]);
    item.exec.overlays = vec!["env(TOKEN)".into()];
    let lease = host.claim(item).await.expect("admitted");
    host.docker.wait_until_held(Duration::from_secs(20));
    let id = LeaseId::parse(&lease.id).unwrap();
    let crashed = host.leases().get(&id).unwrap().unwrap();
    assert_eq!(crashed.state, LeaseState::Running);
    assert_eq!(crashed.secret_names, ["TOKEN"]);
    // Model a crash: the server still holds the lease, and the record is
    // left as the running process wrote it. (A graceful stop would release
    // the lease itself; the server re-issues the same lease ID to this
    // worker, and the record is put back with that epoch.)
    let mock = host.wsp.server().clone();
    let worker = mock
        .worker_id_for_lease(&lease.server_lease_id)
        .expect("live lease");
    mock.expire_lease_reusing_id(&lease.server_lease_id);
    host.docker.release();
    host.stop().await;
    let reissued = mock
        .claim(
            &worker,
            Some("restarted"),
            chrono::Utc::now(),
            chrono::Duration::minutes(5),
        )
        .unwrap()
        .expect("re-issued");
    assert_eq!(reissued.id, lease.server_lease_id);
    let mut record = crashed.clone();
    record.epoch = reissued.epoch;
    record.expires_at = reissued.expires_at;
    {
        let paths = DataPaths::from_env(&support::env_snapshot(&host.s)).unwrap();
        let store = awman::data::fs::api_db::SqliteSessionStore::open_from_paths(
            &awman::data::fs::api_paths::ApiPaths::from_env(&support::env_snapshot(&host.s))
                .unwrap(),
        )
        .unwrap();
        let _ = paths;
        LeaseStore::open(std::sync::Arc::new(store))
            .unwrap()
            .update(&record)
            .unwrap();
    }
    let runs = host.runs().len();
    host.server = Some(Api::restart_with_config(&host.s, host.config.clone()).await);
    let release = wait_for_async(
        "the recovered lease is released",
        Duration::from_secs(20),
        || async { mock.release_for(&lease.server_lease_id) },
    )
    .await;
    let release = serde_json::to_value(release).unwrap();
    assert_eq!(release["reason"], "error", "{release}");
    assert!(release["message"]
        .as_str()
        .unwrap()
        .starts_with("credentials_lost: "));
    assert_eq!(release["result"]["failure"]["code"], "credentials_lost");
    assert_eq!(release["result"]["failure"]["stage"], "preflight");
    assert!(mock.result_for("restarted").is_none());
    let recovered = host.leases().get(&id).unwrap().unwrap();
    assert_eq!(recovered.state, LeaseState::Released);
    // Nothing re-ran after the restart.
    assert_eq!(host.runs().len(), runs);
    host.finish(|root, fake| assert_not_persisted(root, fake, SECRET))
        .await;
}

fn lease_names(capabilities: &Value) -> Vec<&str> {
    capabilities
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|c| {
            [
                "lease_secrets",
                "lease_skills",
                "lease_prerequisites",
                "lease_workflows_dynamic",
            ]
            .contains(c)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_lists_lease_exec_capabilities_only_while_the_pull_worker_reports_them() {
    // Pull enabled: every name the pull worker registers, exactly once.
    let host = Host::start(
        "daemon::lease_exec::status_lists_lease_exec_capabilities_only_while_the_pull_worker_reports_them",
        Options::default(),
        |_| {},
        |_| {},
    )
    .await;
    let (code, status) = host.api().get("/v1/status", Some(&host.key)).await;
    assert_eq!(code, 200, "{status}");
    let capabilities = &status["capabilities"];
    assert_eq!(
        lease_names(capabilities),
        [
            "lease_secrets",
            "lease_skills",
            "lease_prerequisites",
            "lease_workflows_dynamic"
        ]
    );
    assert_eq!(
        capabilities
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| **c == "workflow_prerequisites")
            .count(),
        1
    );
    // The same names went to the work server at registration: an item using
    // each feature is offered to this worker.
    let mut item = host.dynamic("offered");
    item.exec.secrets = secrets(&[("TOKEN", SECRET)]);
    item.exec.overlays = vec!["env(TOKEN)".into(), "skill(planner)".into()];
    item.exec.prerequisites = Some(awman_wsp::ExecPrerequisites {
        git_identity: None,
        ssh: None,
        github_token: None,
    });
    item.exec.workflows_dynamic = Some(awman_wsp::ExecWorkflowsDynamic {
        default_leader: None,
        agents_to_models: None,
        guidance: None,
    });
    host.wsp.server().enqueue(item, chrono::Utc::now()).unwrap();
    let queue = worker_pull::queue_list(
        &host.api().api,
        Some("mock"),
        &awman::data::wire::ListQuery::default(),
    )
    .await
    .unwrap();
    assert!(
        serde_json::to_string(&queue)
            .unwrap()
            .contains("\"offered\""),
        "{}",
        serde_json::to_string(&queue).unwrap()
    );
    host.finish(|_, _| {}).await;

    // Without a pull worker: none, while `workflow_prerequisites` stays.
    let host = Host::start(
        "daemon::lease_exec::status_lists_lease_exec_capabilities_only_while_the_pull_worker_reports_them::no_pull",
        Options {
            pull: false,
            ..Options::default()
        },
        |_| {},
        |_| {},
    )
    .await;
    let (_, status) = host.api().get("/v1/status", Some(&host.key)).await;
    assert!(lease_names(&status["capabilities"]).is_empty(), "{status}");
    assert!(status["capabilities"]
        .as_array()
        .unwrap()
        .contains(&json!("workflow_prerequisites")));
    host.finish(|_, _| {}).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_secrets_cannot_reconfigure_the_host_runtime_or_terminal() {
    let host = Host::start(
        "daemon::lease_exec::lease_secrets_cannot_reconfigure_the_host_runtime_or_terminal",
        Options::default(),
        |_| {},
        |_| {},
    )
    .await;
    for name in [
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "DOCKER_CONFIG",
        "CONTAINER_HOST",
        "PATH",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "SSH_ASKPASS",
        "PYTHONPATH",
        "NODE_OPTIONS",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "SSL_CERT_FILE",
        "TERM",
        "TMUX",
    ] {
        let mut item = host.item(&format!("reserved-{name}"));
        item.exec.secrets = secrets(&[(name, SECRET)]);
        item.exec.overlays = vec![format!("env({name})")];
        let (code, message) = host.claim(item).await.expect_err(name);
        assert_eq!(code, "ineligible");
        assert!(
            message.contains(name) && message.contains("reserved"),
            "{message}"
        );
        assert!(!message.contains(SECRET));
    }
    assert!(
        host.runs().is_empty(),
        "no runtime launch receives these values"
    );
    // Ordinary tokens still follow the name-only 0145 delivery path.
    let mut item = host.item("safe-token");
    item.exec.secrets = secrets(&[("TOKEN", SECRET)]);
    item.exec.overlays = vec!["env(TOKEN)".into()];
    let lease = host.claim(item).await.unwrap();
    host.terminal(&lease).await;
    let runs = host.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].env.get("TOKEN").map(String::as_str), Some(SECRET));
    for name in ["DOCKER_HOST", "PATH", "LD_PRELOAD", "HTTP_PROXY"] {
        assert_ne!(runs[0].env.get(name).map(String::as_str), Some(SECRET));
    }
    host.finish(|_, _| {}).await;
}

async fn generated_overlay_review(
    id: &'static str,
    overlay: &str,
    top_level: bool,
    permitted: bool,
) {
    let host = Host::start(
        id,
        Options {
            server: json!({"allowSkills":["planner"]}),
            vars: vec![("LEASE_EXEC_DENIED", DENIED_VALUE)],
            ..Options::default()
        },
        |s| {
            install_skill(s, "planner");
            install_skill(s, "unlisted");
        },
        |docker| {
            docker.on(&["run"]).hold_until_released();
        },
    )
    .await;
    let mut item = host.dynamic("generated");
    item.exec.secrets = secrets(&[("TOKEN", SECRET)]);
    let lease = host.claim(item).await.expect("dynamic item admitted");
    let leader = host.docker.wait_until_held(Duration::from_secs(20));
    let context = mounts(&leader)
        .into_iter()
        .find_map(|mount| {
            mount
                .strip_suffix(":/awman/context/workflow")
                .map(PathBuf::from)
        })
        .expect("leader workflow context mount");
    let reviewed_skill = if permitted {
        ", \"skill(planner)\""
    } else {
        ""
    };
    let text = if top_level {
        format!("overlays = [{overlay:?}]\n{ONE_STEP}")
    } else {
        format!("{ONE_STEP}\noverlays = [{overlay:?}{reviewed_skill}]\n")
    };
    std::fs::write(
        context.join("workflow.toml"),
        format!("agent = \"claude\"\n{text}"),
    )
    .unwrap();
    host.docker.release();
    let done = host.terminal(&lease).await;
    let result = done.result.expect("terminal result");
    let runs = host.runs();
    if permitted {
        assert_eq!(result["result"]["outcome"], "done", "{result}");
        assert_eq!(runs.len(), 2, "leader and permitted step");
        assert_eq!(runs[1].env.get("TOKEN").map(String::as_str), Some(SECRET));
        assert!(mounts(&runs[1])
            .iter()
            .any(|mount| mount.contains("skills/planner")));
    } else {
        assert_eq!(result["result"]["outcome"], "error", "{result}");
        assert!(result.to_string().contains("lease overlay"), "{result}");
        assert_eq!(runs.len(), 1, "only the leader may launch");
        assert!(!env_args(&runs[0]).contains(&"LEASE_EXEC_DENIED"));
    }
    host.finish(|_, _| {}).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_workflow_cannot_read_an_unlisted_worker_variable() {
    generated_overlay_review(
        "daemon::lease_exec::generated_workflow_cannot_read_an_unlisted_worker_variable",
        "env(LEASE_EXEC_DENIED)",
        true,
        false,
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_step_cannot_mount_an_unlisted_skill() {
    generated_overlay_review(
        "daemon::lease_exec::generated_step_cannot_mount_an_unlisted_skill",
        "skill(unlisted)",
        false,
        false,
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_step_cannot_mount_wildcard_skills() {
    generated_overlay_review(
        "daemon::lease_exec::generated_step_cannot_mount_wildcard_skills",
        "skill(*)",
        false,
        false,
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_step_can_request_an_item_secret() {
    generated_overlay_review(
        "daemon::lease_exec::generated_step_can_request_an_item_secret",
        "env(TOKEN)",
        false,
        true,
    )
    .await;
}
