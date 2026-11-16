//! `init` over the API (WI 0148): the production worker command route in a
//! local session, and dynamic-workflow preflight before/after `init`.
use super::support::Api;
use crate::support;
use awman::command::daemon_api::keys;
use awman_test_support::capability::LocalGit;
use awman_test_support::fakes::FakeCli;
use awman_test_support::fixtures::{ConfigTree, RepoBuilder};
use awman_test_support::{wait_for_async, TestSandbox};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

const TEMPLATE: &str = include_str!("../../../templates/Dockerfile.project");

fn worker_config(workdir: &Path) -> Value {
    json!({"daemon":{"features":{"worker":{"enabled":true,
            "push":{"enabled":true,"workDirs":{"allow":[workdir]}}}},
        "network":{"listeners":{"component":{"features":["engine","worker"]}}}}})
}

struct Fixture {
    server: Api,
    /// The fake `docker` every session and command runs.
    docker: FakeCli,
    key: String,
    session: String,
    root: std::path::PathBuf,
}

impl Fixture {
    /// Start a daemon and a local session on a fresh repo. Session setup's ready
    /// checks provision a missing `Dockerfile.dev` and agent Dockerfile themselves,
    /// so `scrub` names the files removed once the session is ready: the repo
    /// then looks to `init` the way an unprepared checkout does.
    ///
    /// hygiene-allow(H1): a local session resolves `docker` and its temp root from the daemon process environment; nextest gives each test its own process, and the values point into this test's sandbox.
    /// hygiene-allow(H7): reads the process PATH only to prepend this sandbox's fake `bin` to it.
    async fn start(
        s: &mut TestSandbox,
        repo_config: Value,
        files: &[(&str, &str)],
        scrub: &[&str],
    ) -> Self {
        s.require::<LocalGit>();
        let mut builder = RepoBuilder::new(s, "project");
        for (path, body) in files {
            builder = builder.file(path, body);
        }
        let repo = builder.init();
        let root = repo.path().to_path_buf();
        ConfigTree::new()
            .global(json!({"agents":{"auth":{"refresh":{"enabled":false}}}}))
            .repo(repo_config)
            .write(s, Some(&root));
        // A local session opens in the daemon's process environment, so the
        // sandbox's fake `docker` must lead this process's PATH (nextest runs
        // each test in its own process). Every image is reported present, so
        // session setup performs no builds.
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", s.bin().display()));
        // Host-CLI isolation admits a fake only inside the snapshot's temp root.
        let temp_root = s.root().display().to_string();
        for key in ["TMPDIR", "TMP", "TEMP"] {
            s.env_mut().set(key, &temp_root);
            std::env::set_var(key, &temp_root);
        }
        let mut fake = None;
        let server = Api::with_docker(s, worker_config(&root), |docker| {
            docker.on(&["image", "inspect"]).stdout("[]\n").exit(0);
            fake = Some(docker.clone());
        })
        .await;
        let key = keys::keys_create(&server.api, "worker").unwrap();
        s.redact(&key.key);
        let (code, created) = server
            .request(
                "POST",
                "/v1/worker/sessions",
                Some(&key.key),
                None,
                Some(json!({"session_type":"local","workdir":root})),
            )
            .await;
        assert!(code == 201 || code == 202, "{created}");
        let session = created["id"].as_str().unwrap().to_owned();
        wait_for_async(
            "session setup finished",
            Duration::from_secs(30),
            || async {
                let (_, v) = server
                    .get(&format!("/v1/worker/sessions/{session}"), Some(&key.key))
                    .await;
                matches!(v["setup_status"].as_str(), Some("ready" | "failed")).then_some(())
            },
        )
        .await;
        for rel in scrub {
            let path = root.join(rel);
            let _ = std::fs::remove_file(&path).or_else(|_| std::fs::remove_dir_all(&path));
        }
        Self {
            server,
            docker: fake.expect("fake docker installed"),
            key: key.key,
            session,
            root,
        }
    }

    /// Submit a command and wait for its terminal record.
    async fn run(&self, subcommand: &str, args: Value) -> Value {
        let (code, created) = self
            .server
            .request(
                "POST",
                "/v1/worker/commands",
                Some(&self.key),
                None,
                Some(json!({"session_id":self.session,"subcommand":subcommand,"args":args})),
            )
            .await;
        assert!(code == 201 || code == 202, "{created}");
        let path = format!("/v1/worker/commands/{}", created["id"].as_str().unwrap());
        wait_for_async("command finished", Duration::from_secs(20), || async {
            let (code, v) = self.server.get(&path, Some(&self.key)).await;
            assert_eq!(code, 200, "{v}");
            matches!(
                v["status"].as_str(),
                Some("done" | "error" | "failed" | "cancelled")
            )
            .then_some(v)
        })
        .await
    }

    async fn init(&self) -> Value {
        let done = self.run("init", json!(["--agent", "claude"])).await;
        assert_eq!(done["status"], "done", "{done}");
        done
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.root.join(rel)).unwrap()
    }

    async fn session(&self) -> Value {
        self.server
            .get(
                &format!("/v1/worker/sessions/{}", self.session),
                Some(&self.key),
            )
            .await
            .1
    }

    /// Commit `paths` the way a client commits init's result.
    fn commit(&self, s: &TestSandbox, paths: &[String]) {
        for args in [
            [
                &["add", "--"][..],
                &paths.iter().map(String::as_str).collect::<Vec<_>>(),
            ]
            .concat(),
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@awman.invalid",
                "commit",
                "-q",
                "-m",
                "setup",
            ],
        ] {
            s.command("git")
                .args(&args)
                .current_dir(&self.root)
                .label("git")
                .run()
                .assert_success();
        }
    }
}

fn files(done: &Value) -> Vec<Value> {
    done["result"]["data"]["files"].as_array().unwrap().to_vec()
}

fn entry(done: &Value, path: &str) -> Value {
    files(done)
        .into_iter()
        .find(|f| f["path"] == path)
        .unwrap_or_else(|| panic!("no {path} in {done}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_advertises_api_init() {
    let s = support::sandbox("daemon::init_api::status_advertises_api_init");
    let server = Api::with_config(&s, worker_config(&s.work())).await;
    let key = keys::keys_create(&server.api, "reader").unwrap();
    s.redact(&key.key);
    let (code, status) = server.get("/v1/status", Some(&key.key)).await;
    assert_eq!(code, 200, "{status}");
    let count = status["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| **c == json!("api_init"))
        .count();
    assert_eq!(count, 1, "{status}");
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_on_empty_repo_creates_dockerfile_config_and_agent_dockerfile() {
    let mut s = support::sandbox(
        "daemon::init_api::init_on_empty_repo_creates_dockerfile_config_and_agent_dockerfile",
    );
    let f = Fixture::start(
        &mut s,
        json!({}),
        &[("README.md", "hi")],
        &[
            "Dockerfile.dev",
            ".awman/Dockerfile.claude",
            ".awman/config.json",
        ],
    )
    .await;
    let done = f.init().await;
    assert_eq!(f.read("Dockerfile.dev"), TEMPLATE.as_bytes());
    assert!(f.root.join(".awman/config.json").is_file());
    assert!(f.root.join(".awman/Dockerfile.claude").is_file());
    let listed: Vec<(String, String)> = files(&done)
        .iter()
        .map(|e| {
            (
                e["path"].as_str().unwrap().to_owned(),
                e["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            ("Dockerfile.dev", "created"),
            (".awman/config.json", "created"),
            (".awman/Dockerfile.claude", "created"),
        ]
        .map(|(p, st)| (p.to_owned(), st.to_owned()))
    );
    assert_eq!(done["result"]["data"]["summary"]["dockerfile"], "Done");
    // No question reached the client.
    assert!(!done.to_string().contains("pending_question"), "{done}");
    f.server.shutdown().await;
}

/// Session setup leaves the seeded Dockerfile alone; only the awman files it
/// provisions beside it are scrubbed.
async fn keeps_existing_dockerfile(s: &mut TestSandbox, body: &str) {
    let f = Fixture::start(
        s,
        json!({}),
        &[("Dockerfile.dev", body)],
        &[".awman/Dockerfile.claude", ".awman/config.json"],
    )
    .await;
    let done = f.init().await;
    assert_eq!(f.read("Dockerfile.dev"), body.as_bytes());
    assert_eq!(entry(&done, "Dockerfile.dev")["status"], "kept", "{done}");
    assert_eq!(entry(&done, ".awman/config.json")["status"], "created");
    assert_eq!(
        entry(&done, ".awman/Dockerfile.claude")["status"],
        "created"
    );
    assert_eq!(done["result"]["data"]["summary"]["dockerfile"], "Done");
    f.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_keeps_existing_dockerfile_byte_for_byte() {
    let mut s = support::sandbox("daemon::init_api::init_keeps_existing_dockerfile_byte_for_byte");
    keeps_existing_dockerfile(&mut s, "FROM scratch\n# mine\n").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_keeps_existing_empty_dockerfile() {
    let mut s = support::sandbox("daemon::init_api::init_keeps_existing_empty_dockerfile");
    keeps_existing_dockerfile(&mut s, "").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_creates_template_at_configured_relative_dockerfile_then_keeps_it() {
    let mut s = support::sandbox(
        "daemon::init_api::init_creates_template_at_configured_relative_dockerfile_then_keeps_it",
    );
    // `docker/` does not exist: session setup creates it with the template,
    // then the fixture removes both so init does the same.
    let f = Fixture::start(
        &mut s,
        json!({"runtime":{"dockerfile":"docker/Project.Dockerfile"}}),
        &[("README.md", "hi")],
        &["docker", ".awman/Dockerfile.claude"],
    )
    .await;
    let first = f.init().await;
    assert_eq!(f.read("docker/Project.Dockerfile"), TEMPLATE.as_bytes());
    assert!(!f.root.join("Dockerfile.dev").exists());
    assert_eq!(
        entry(&first, "docker/Project.Dockerfile")["status"],
        "created",
        "{first}"
    );
    let second = f.init().await;
    assert_eq!(
        entry(&second, "docker/Project.Dockerfile")["status"],
        "kept",
        "{second}"
    );
    assert_eq!(f.read("docker/Project.Dockerfile"), TEMPLATE.as_bytes());
    assert!(!f.root.join("Dockerfile.dev").exists());
    f.server.shutdown().await;
}

/// A configured Dockerfile whose directory does not exist yet: session setup
/// creates it (it used to fail with an io error, leaving init unreachable
/// behind `session_not_ready`), and init reports it as something to commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_setup_creates_a_configured_dockerfile_in_a_missing_directory() {
    let mut s = support::sandbox(
        "daemon::init_api::session_setup_creates_a_configured_dockerfile_in_a_missing_directory",
    );
    let f = Fixture::start(
        &mut s,
        json!({"runtime":{"dockerfile":"missing/Project.Dockerfile"}}),
        &[("README.md", "hi")],
        &[],
    )
    .await;
    let session = f.session().await;
    assert_eq!(session["setup_status"], "ready", "{session}");
    assert_eq!(f.read("missing/Project.Dockerfile"), TEMPLATE.as_bytes());
    let done = f.init().await;
    let e = entry(&done, "missing/Project.Dockerfile");
    assert_eq!(e["status"], "kept", "{done}");
    assert_eq!(e["uncommitted"], true, "{done}");
    f.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn neither_session_setup_nor_init_creates_a_dockerfile_outside_the_git_root() {
    let mut s = support::sandbox(
        "daemon::init_api::neither_session_setup_nor_init_creates_a_dockerfile_outside_the_git_root",
    );
    let dir = s.root().join("outside-root");
    std::fs::create_dir_all(&dir).unwrap();
    let outside = dir.join("Dockerfile");
    // Nothing is removed after setup: what the client sees is what setup did.
    let f = Fixture::start(
        &mut s,
        json!({"runtime":{"dockerfile":outside}}),
        &[("README.md", "hi")],
        &[],
    )
    .await;
    assert!(
        !outside.exists(),
        "session setup wrote outside the git root"
    );
    let session = f.session().await;
    assert_eq!(session["setup_status"], "failed", "{session}");
    let reason = session["setup_error"]["message"].as_str().unwrap();
    assert!(reason.contains("outside the git root"), "{session}");
    // The session cannot run commands; init is refused with setup's reason
    // (it would not create the file either: see the engine contract).
    let (code, refused) = f
        .server
        .request(
            "POST",
            "/v1/worker/commands",
            Some(&f.key),
            None,
            Some(json!({"session_id":f.session,"subcommand":"init","args":["--agent","claude"]})),
        )
        .await;
    assert_eq!(code, 409, "{refused}");
    assert!(
        refused.to_string().contains("outside the git root"),
        "{refused}"
    );
    assert!(!outside.exists());
    assert!(!f.root.join("Dockerfile.dev").exists());
    f.server.shutdown().await;
}

/// The whole first-use story with nothing removed behind the client's back:
/// session setup provisions the Dockerfiles, init lists them (and the repo
/// config) as uncommitted, the client commits exactly those, and the dynamic
/// workflow then gets past preflight to launching its leader.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_repo_init_reports_setup_files_to_commit_and_the_workflow_reaches_its_leader() {
    let mut s = support::sandbox(
        "daemon::init_api::fresh_repo_init_reports_setup_files_to_commit_and_the_workflow_reaches_its_leader",
    );
    let f = Fixture::start(
        &mut s,
        json!({"workItems":{"source":"local"}}),
        &[("aspec/work-items/0007-seven.md", "# Seven\n\nbody\n")],
        &[],
    )
    .await;
    assert_eq!(f.session().await["setup_status"], "ready");
    let done = f.init().await;
    let listed: Vec<(String, String, bool)> = files(&done)
        .iter()
        .map(|e| {
            (
                e["path"].as_str().unwrap().to_owned(),
                e["status"].as_str().unwrap().to_owned(),
                e["uncommitted"].as_bool().unwrap(),
            )
        })
        .collect();
    // Setup created all three before init ran, so init keeps them — but none
    // is committed yet, and the result says so.
    assert_eq!(
        listed,
        [
            ("Dockerfile.dev", "kept", true),
            (".awman/config.json", "kept", true),
            (".awman/Dockerfile.claude", "kept", true),
        ]
        .map(|(p, st, u)| (p.to_owned(), st.to_owned(), u))
    );
    assert_eq!(f.read("Dockerfile.dev"), TEMPLATE.as_bytes());

    let to_commit: Vec<String> = files(&done)
        .iter()
        .filter(|e| e["uncommitted"] == true)
        .map(|e| e["path"].as_str().unwrap().to_owned())
        .collect();
    f.commit(&s, &to_commit);
    let again = f.init().await;
    assert!(
        files(&again).iter().all(|e| e["uncommitted"] == false),
        "{again}"
    );

    let launches_before = leader_launches(&f.docker);
    let run = f
        .run(
            "exec workflow",
            json!(["--work-item", "7", "--dynamic", "--agent", "claude"]),
        )
        .await;
    assert!(
        !run.to_string().contains("no Dockerfile in the project"),
        "{run}"
    );
    assert!(
        leader_launches(&f.docker) > launches_before,
        "the dynamic workflow never launched its leader: {run}\n{:#?}",
        f.docker.calls()
    );
    // The fake leader writes no workflow.toml: the expected downstream failure.
    assert!(run.to_string().contains("leader agent"), "{run}");
    f.server.shutdown().await;
}

/// Agent container launches the fake `docker` has seen.
fn leader_launches(docker: &FakeCli) -> usize {
    docker
        .calls()
        .iter()
        .filter(|c| matches!(c.argv.first().map(String::as_str), Some("run" | "create")))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_workflow_fails_preflight_until_api_init_then_proceeds() {
    let mut s = support::sandbox(
        "daemon::init_api::dynamic_workflow_fails_preflight_until_api_init_then_proceeds",
    );
    let f = Fixture::start(
        &mut s,
        json!({"workItems":{"source":"local"}}),
        &[("aspec/work-items/0007-seven.md", "# Seven\n\nbody\n")],
        &["Dockerfile.dev", ".awman/Dockerfile.claude"],
    )
    .await;
    let args = json!(["--work-item", "7", "--dynamic", "--agent", "claude"]);
    let before = f.run("exec workflow", args.clone()).await;
    assert_ne!(before["status"], "done", "{before}");
    let text = before.to_string();
    assert!(text.contains("no Dockerfile in the project"), "{text}");
    assert!(text.contains(".awman/Dockerfile.claude"), "{text}");
    assert!(
        text.contains(r#"\"subcommand\":\"init\""#) && text.contains("--agent"),
        "init remedy missing: {text}"
    );
    // Failed before the leader: no launch, no repair attempts, nothing written.
    assert_eq!(leader_launches(&f.docker), 0, "{:#?}", f.docker.calls());
    assert!(!text.contains("repair attempts"), "{text}");
    assert!(!f.root.join(".awman/Dockerfile.claude").exists());
    assert!(!f.root.join("Dockerfile.dev").exists());

    f.init().await;
    let after = f.run("exec workflow", args).await;
    // Past the Dockerfile preflight to the leader launch; the fake leader then
    // writes no workflow.toml, the expected downstream failure.
    assert!(
        !after.to_string().contains("no Dockerfile in the project"),
        "{after}"
    );
    assert!(leader_launches(&f.docker) > 0, "{after}");
    assert!(after.to_string().contains("leader agent"), "{after}");
    f.server.shutdown().await;
}
