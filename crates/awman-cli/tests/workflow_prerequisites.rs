//! WI 0149 command/frontend end-to-end regressions. All external commands
//! are disposable fixtures under AWMAN_TEST_ISOLATION; no GitHub calls.
#![cfg(unix)]
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
#[path = "../../../tests/helpers/api_process.rs"]
mod api_process;

const SIMPLE: &str = "title='prereq-test'\n[[steps]]\nname='work'\nagent='claude'\nprompt='work'\n";
const TEARDOWN: &str = "\n[[teardown]]\ntype='commit_changes'\nmessage='test commit'\n[[teardown]]\ntype='push_branch'\n[[teardown]]\ntype='create_pull_request'\ntitle='test PR'\n";
const TOKEN: &str = "ghp_fake_wi0149_never_log_me";
fn executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
struct Fixture {
    dir: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        for path in ["home", "bin", "repo/.awman", "control"] {
            std::fs::create_dir_all(fixture.path(path)).unwrap();
        }
        fixture.git(&["init", "-q"]);
        fixture.git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "initial",
        ]);
        fixture.git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/example/repo.git",
        ]);
        std::fs::write(fixture.path("repo/wf.toml"), SIMPLE).unwrap();
        executable(&fixture.path("bin/docker"), "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$WI0149_CONTROL/docker.log\"\ncase \"$1\" in\n info) echo 27.0.0;;\n image|inspect) echo '[{\"Id\":\"sha256:fake\"}]';;\n run) echo fake-container;;\n *) exit 0;;\nesac\n");
        fixture
    }
    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_awman"));
        cmd.current_dir(self.path("repo"))
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.path("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("HOME", self.path("home"))
            .env("AWMAN_CONFIG_HOME", self.path("home/.awman"))
            .env("XDG_CONFIG_HOME", self.path("home/.config"))
            .env("XDG_DATA_HOME", self.path("home/.local/share"))
            .env("XDG_CACHE_HOME", self.path("home/.cache"))
            .env("TMPDIR", std::env::temp_dir())
            .env("AWMAN_TEST_ISOLATION", "1")
            .env("GIT_CONFIG_GLOBAL", self.path("home/.gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("WI0149_CONTROL", self.path("control"))
            .stdin(Stdio::null());
        cmd
    }
    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(self.path("repo"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn teardown(&self) {
        std::fs::write(self.path("repo/wf.toml"), format!("{SIMPLE}{TEARDOWN}")).unwrap();
    }
    fn global(&self, value: Value) {
        std::fs::create_dir_all(self.path("home/.awman")).unwrap();
        std::fs::write(
            self.path("home/.awman/config.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }
    fn launches(&self) -> usize {
        std::fs::read_to_string(self.path("control/docker.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("run "))
            .count()
    }
}
fn output(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}
#[test]
fn wi0149_check_prerequisites_exit_codes_output_and_zero_container_launches() {
    let f = Fixture::new();
    let out = f.run(&["exec", "workflow", "wf.toml", "--check-prerequisites"]);
    assert_eq!(out.status.code(), Some(0), "{}", output(&out));
    assert!(output(&out).contains("prerequisites"));
    f.teardown();
    let out = f.run(&["exec", "workflow", "wf.toml", "--check-prerequisites"]);
    assert_eq!(out.status.code(), Some(1), "{}", output(&out));
    assert!(output(&out).contains("GitHub"), "{}", output(&out));
    assert_eq!(f.launches(), 0, "check-only must never start containers");
}
#[test]
fn wi0149_static_missing_token_unresolved_fail_launches_zero_containers() {
    let f = Fixture::new();
    f.teardown();
    let out = f.run(&[
        "exec",
        "workflow",
        "wf.toml",
        "--non-interactive",
        "--prerequisite-unresolved",
        "fail",
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", output(&out));
    assert!(output(&out).contains("GitHub"), "{}", output(&out));
    assert_eq!(f.launches(), 0);
    assert!(!f.path("repo/.awman/worktrees").exists());
}
#[test]
fn wi0149_non_interactive_env_token_explicit_grant_and_json_are_secret_free() {
    let f = Fixture::new();
    f.teardown();
    let out = f
        .command()
        .env("GH_TOKEN", TOKEN)
        .args([
            "exec",
            "workflow",
            "--json",
            "wf.toml",
            "--non-interactive",
            "--prerequisite-token",
            "env:GH_TOKEN",
            "--check-prerequisites",
        ])
        .output()
        .unwrap();
    assert!(!output(&out).contains(TOKEN));
    assert!(output(&out).contains("GH_TOKEN"), "{}", output(&out));
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    let token = json["prerequisites"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["prerequisite"]["type"] == "git_hub_token")
        .unwrap();
    assert_eq!(
        token["status"], "satisfied",
        "explicit env selection is consent; check-only still launches nothing: {json}"
    );
    assert_eq!(f.launches(), 0);
}
#[test]
fn wi0149_only_global_identity_needs_consent() {
    let f = Fixture::new();
    f.teardown();
    std::fs::write(
        f.path("home/.gitconfig"),
        "[user]\nname = Ada\nemail = ada@example.com\n",
    )
    .unwrap();
    let out = f.run(&["exec", "workflow", "wf.toml", "--check-prerequisites"]);
    let text = output(&out);
    assert!(text.contains("Git identity"), "{text}");
    assert!(text.contains("needs consent"), "{text}");
    assert!(
        text.contains("host-global identity requires consent"),
        "{text}"
    );
}
#[test]
fn wi0149_secret_name_is_usage_error_and_headless_never_grants() {
    let f = Fixture::new();
    let out = f.run(&[
        "exec",
        "workflow",
        "wf.toml",
        "--prerequisite-token",
        "secret:NAME",
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", output(&out));
    assert!(output(&out).contains("unsupported"), "{}", output(&out));
    f.teardown();
    let out = f
        .command()
        .env("GH_TOKEN", TOKEN)
        .args(["exec", "workflow", "wf.toml", "--non-interactive"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", output(&out));
    assert_eq!(f.launches(), 0);
    assert!(!output(&out).contains(TOKEN));
}
#[test]
fn wi0149_untrusted_repo_allow_and_acquire_never_spawn_or_mount_in_cli() {
    let f = Fixture::new();
    f.teardown();
    let marker = f.path("control/acquired");
    let acquire = f.path("bin/acquire");
    executable(
        &acquire,
        &format!("#!/bin/sh\ntouch '{}'\necho {TOKEN}\n", marker.display()),
    );
    std::fs::write(f.path("repo/.awman/config.json"), json!({"workflows":{"prerequisites":{"ssh":{"use":"allow"},"githubToken":{"use":"allow","acquire":{"argv":[acquire]}}}}}).to_string()).unwrap();
    let out = f.run(&["exec", "workflow", "wf.toml", "--non-interactive"]);
    assert_eq!(out.status.code(), Some(2), "{}", output(&out));
    assert!(!marker.exists());
    assert_eq!(f.launches(), 0);
    assert!(!output(&out).contains(TOKEN));
    assert!(!f.path("home/.awman/prerequisite-grants.json").exists());
}
#[test]
fn wi0149_architecture_layers_and_no_secretstring_in_data() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = Command::new("bash")
        .arg(root.join("tools/architecture-lint.sh"))
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", output(&out));
    fn scan(path: &Path) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scan(&path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                assert!(
                    !std::fs::read_to_string(&path)
                        .unwrap()
                        .contains("SecretString"),
                    "Layer 0 secret type: {}",
                    path.display()
                );
            }
        }
    }
    scan(&root.join("src/data"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wi0149_api_prerequisite_event_precedes_steps_and_failure_has_stage() {
    let f = Fixture::new();
    f.teardown();
    let home = api_process::ApiHome::new();
    configure_api(&home, &f, false);
    let server = home.start().await;
    let session = server
        .ready_session(json!({"session_type":"local","workdir":f.path("repo")}))
        .await;
    let record = server
        .run_command(
            &session,
            "exec workflow",
            &["wf.toml", "--prerequisite-unresolved", "fail"],
        )
        .await;
    assert_eq!(record["status"], "error", "{record}");
    assert_eq!(
        record["result"]["stage"], "workflow_prerequisites",
        "{record}"
    );
    let events = api_process::events_log(&record).await;
    let prerequisite = events
        .find("WorkflowPrerequisites")
        .expect("prerequisite event");
    let sse = reqwest::Client::new()
        .get(format!(
            "{}/v1/commands/{}/logs",
            server.url,
            record["id"].as_str().unwrap()
        ))
        .header("x-awman-session", &session)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(sse.contains("event: workflow_prerequisites"), "{sse}");
    assert!(
        !sse.contains("event: step_started"),
        "failed preflight must emit no step events: {sse}"
    );
    for name in ["StepStarted", "StepStatus", "StdoutLine"] {
        if let Some(index) = events.find(name) {
            assert!(prerequisite < index, "{events}");
        }
    }
    let typed = awman::data::execution_event::parse_event_log(&events);
    assert!(
        matches!(
            typed.first().map(|e| &e.payload),
            Some(awman::data::execution_event::EventPayload::WorkflowPrerequisites { .. })
        ),
        "prerequisite event must be first: {events}"
    );
    let report = typed
        .iter()
        .find_map(|e| match &e.payload {
            awman::data::execution_event::EventPayload::WorkflowPrerequisites {
                report, ..
            } => Some(report),
            _ => None,
        })
        .unwrap();
    assert!(report.checks.iter().any(|c| matches!(
        c.prerequisite,
        awman::data::workflow_prerequisites::WorkflowPrerequisite::GitHubToken { .. }
    ) && c.status
        == awman::data::workflow_prerequisites::PrerequisiteStatus::Missing));
    assert!(!events.contains(TOKEN));
    let docker = std::fs::read_to_string(home.control().join("docker.log")).unwrap_or_default();
    assert!(
        !docker.lines().any(|l| l.starts_with("docker run ")),
        "{docker}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wi0149_api_secret_usage_error_and_ssh_widening_gate() {
    for allow in [false, true] {
        let f = Fixture::new();
        let home = api_process::ApiHome::new();
        std::fs::create_dir_all(home.home().join(".config/awman")).unwrap();
        std::fs::write(home.home().join(".config/awman/config.json"), json!({"api":{"workDirs":[f.path("repo")]}, "workflows":{"prerequisites":{"allowApiClientGrants":allow}}}).to_string()).unwrap();
        let server = home.start().await;
        let session = server
            .ready_session(json!({"session_type":"local","workdir":f.path("repo")}))
            .await;
        for (value, expected) in [("secret:NAME", 400), ("deny", 202)] {
            let response = reqwest::Client::new().post(format!("{}/v1/commands", server.url)).header("x-awman-session", &session).json(&json!({"subcommand":"exec workflow", "args":["wf.toml","--check-prerequisites","--prerequisite-token",value]})).send().await.unwrap();
            let status = response.status().as_u16();
            let body: Value = response.json().await.unwrap();
            assert_eq!(status, expected, "{body}");
        }
        let response = reqwest::Client::new().post(format!("{}/v1/commands", server.url)).header("x-awman-session", &session).json(&json!({"subcommand":"exec workflow", "args":["wf.toml","--check-prerequisites","--prerequisite-ssh","all"]})).send().await.unwrap();
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, if allow { 202 } else { 403 }, "{body}");
        assert!(!body.to_string().contains(TOKEN));
    }
}

const RECORDING_DOCKER: &str = r#"#!/bin/sh
ctl="$WI0149_CONTROL"
printf '%s\n' "$*" >> "$ctl/docker.log"
case "$1" in
 info) echo 27.0.0 ;;
 image|images|inspect) echo '[{"Id":"sha256:fake"}]' ;;
 run)
   background=no
   for arg in "$@"; do [ "$arg" = -d ] && background=yes; done
   if [ "$background" = yes ]; then
     n=$(cat "$ctl/count" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$ctl/count"
     printf '%s' "${GH_TOKEN:-}" > "$ctl/bg-$n.token"
     printf '%s' "${AWMAN_GIT_TOKEN:-}" > "$ctl/bg-$n.push-token"
     echo "bg-$n"
   else
     printf 'agent token=%s push=%s\n' "${GH_TOKEN:-}" "${AWMAN_GIT_TOKEN:-}" >> "$ctl/agents.env"
   fi ;;
 exec)
   case "$*" in
     *'gh pr create'*)
       for arg in "$@"; do case "$arg" in bg-*) id="$arg";; esac; done
       echo "PR_CONTAINER:$id" >> "$ctl/phases"
       token=$(cat "$ctl/$id.token")
       printf 'stdout leaked %s\n' "$token"
       printf 'stderr leaked %s\n' "$token" >&2
       exit 1 ;;
     *'git commit'*)
       for arg in "$@"; do case "$arg" in bg-*) echo "COMMIT_CONTAINER:$arg" >> "$ctl/phases";; esac; done ;;
   esac ;;
 *) exit 0 ;;
esac
"#;
fn descendants(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries {
            let p = entry.unwrap().path();
            if p.is_dir() {
                out.extend(descendants(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}
#[test]
fn wi0149_acquire_token_scoped_to_pr_and_redacted_in_output_failure_file_and_agent() {
    let f = Fixture::new();
    f.git(&["config", "user.name", "Ada"]);
    f.git(&["config", "user.email", "ada@example.com"]);
    let acquire = f.path("bin/acquire-token");
    executable(&acquire, &format!("#!/bin/sh\nprintf '%s\\n' '{TOKEN}'\n"));
    f.global(json!({"workflows":{"prerequisites":{"unresolved":"warn","githubToken":{"use":"allow","acquire":{"argv":[acquire],"timeoutSecs":3}}}}}));
    std::fs::write(
        f.path("repo/wf.toml"),
        format!("{SIMPLE}{TEARDOWN}\non_failure={{prompt='repair the failure',max_attempts=1}}\n"),
    )
    .unwrap();
    executable(&f.path("bin/docker"), RECORDING_DOCKER);
    let out = f.run(&["exec", "workflow", "wf.toml", "--non-interactive", "--yolo"]);
    let text = output(&out);
    let phases = std::fs::read_to_string(f.path("control/phases"))
        .unwrap_or_else(|_| panic!("phase containers did not run: {text}"));
    let id = |prefix: &str| {
        phases
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap()
            .to_string()
    };
    assert_eq!(
        std::fs::read_to_string(f.path(&format!("control/{}.token", id("PR_CONTAINER:")))).unwrap(),
        TOKEN
    );
    assert_eq!(
        std::fs::read_to_string(f.path(&format!("control/{}.token", id("COMMIT_CONTAINER:"))))
            .unwrap(),
        ""
    );
    assert!(
        !text.contains(TOKEN),
        "captured token output escaped redaction"
    );
    assert!(
        text.contains("***"),
        "redaction should be observable: {text}"
    );
    let files: Vec<_> = descendants(f.dir.path())
        .into_iter()
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("teardown-failure-")
        })
        .collect();
    assert!(
        !files.is_empty(),
        "on_failure must capture a failure file: {text}"
    );
    for path in files {
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains(TOKEN), "{}", path.display());
        assert!(content.contains("***"), "{content}");
    }
    let agents = std::fs::read_to_string(f.path("control/agents.env")).unwrap();
    assert!(
        agents.lines().count() >= 2,
        "main and on_failure agents must both launch: {text}"
    );
    assert!(!agents.contains(TOKEN));
    let argv = std::fs::read_to_string(f.path("control/docker.log")).unwrap();
    assert!(!argv.contains(TOKEN), "ps-visible argv leaked a token");
    for path in descendants(&f.path("home/.awman")) {
        if path
            .extension()
            .is_some_and(|ext| ext == "json" || ext == "jsonl" || ext == "log")
        {
            assert!(
                !std::fs::read_to_string(&path).unwrap().contains(TOKEN),
                "{}",
                path.display()
            );
        }
    }
}

fn configure_api(home: &api_process::ApiHome, fixture: &Fixture, allow: bool) {
    std::fs::create_dir_all(home.home().join(".config/awman")).unwrap();
    std::fs::write(home.home().join(".config/awman/config.json"), json!({"api":{"workDirs":[fixture.path("repo")]},"workflows":{"prerequisites":{"allowApiClientGrants":allow}}}).to_string()).unwrap();
}

#[test]
fn wi0149_non_interactive_env_token_grants_without_prompt() {
    let f = Fixture::new();
    f.teardown();
    f.git(&["config", "user.name", "Ada"]);
    f.git(&["config", "user.email", "ada@example.com"]);
    let out = f
        .command()
        .env("GH_TOKEN", TOKEN)
        .args([
            "exec",
            "workflow",
            "wf.toml",
            "--non-interactive",
            "--prerequisite-token",
            "env:GH_TOKEN",
            "--yolo",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", output(&out));
    assert!(
        output(&out).contains("token from GH_TOKEN granted"),
        "{}",
        output(&out)
    );
    assert!(!output(&out).contains(TOKEN));
    assert!(f.launches() >= 4, "main, commit, push and PR should launch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wi0149_api_repo_allow_and_acquire_never_spawn_or_mount() {
    let f = Fixture::new();
    f.teardown();
    f.git(&[
        "remote",
        "set-url",
        "origin",
        "git@github.com:example/repo.git",
    ]);
    let marker = f.path("control/spawned");
    let program = f.path("bin/acquire");
    executable(
        &program,
        &format!("#!/bin/sh\ntouch '{}'\necho {TOKEN}\n", marker.display()),
    );
    std::fs::write(f.path("repo/.awman/config.json"), json!({"workflows":{"prerequisites":{"ssh":{"use":"allow"},"githubToken":{"use":"allow","acquire":{"argv":[program]}}}}}).to_string()).unwrap();
    let home = api_process::ApiHome::new();
    configure_api(&home, &f, false);
    let server = home.start().await;
    let session = server
        .ready_session(json!({"session_type":"local","workdir":f.path("repo")}))
        .await;
    let record = server
        .run_command(&session, "exec workflow", &["wf.toml"])
        .await;
    assert_eq!(
        record["result"]["stage"], "workflow_prerequisites",
        "{record}"
    );
    assert!(!marker.exists());
    let argv = std::fs::read_to_string(home.control().join("docker.log")).unwrap_or_default();
    assert!(!argv.lines().any(|l| l.starts_with("docker run ")));
    assert!(!argv.contains(".ssh"));
    assert!(!api_process::events_log(&record).await.contains(TOKEN));
    assert!(!server.log().contains(TOKEN));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wi0149_api_flags_applied_contains_resolved_secret_free_policy() {
    let f = Fixture::new();
    let home = api_process::ApiHome::new();
    configure_api(&home, &f, false);
    let server = home.start().await;
    let session = server
        .ready_session(json!({"session_type":"local","workdir":f.path("repo")}))
        .await;
    let response: Value = reqwest::Client::new().post(format!("{}/v1/commands", server.url)).header("x-awman-session", &session).json(&json!({"subcommand":"exec workflow", "args":["wf.toml","--check-prerequisites","--prerequisite-ssh","deny"]})).send().await.unwrap().json().await.unwrap();
    assert!(
        response["flags_applied"]["prerequisites"].is_object(),
        "{response}"
    );
    assert_eq!(
        response["flags_applied"]["prerequisites"]["unresolved"],
        "fail"
    );
    assert_eq!(
        response["flags_applied"]["prerequisites"]["ssh"]["use"],
        "deny"
    );
    assert!(!response.to_string().contains(TOKEN));
}

#[test]
fn wi0149_grants_list_revoke_persists_decisions_never_token_values() {
    use awman::data::fs::prerequisite_grants::{
        GrantKey, PrerequisiteGrantStore, RememberedGrant, RememberedKind,
    };
    let f = Fixture::new();
    std::fs::create_dir_all(f.path("home/.awman")).unwrap();
    let mut store = PrerequisiteGrantStore::load(&f.path("home/.awman"));
    store
        .remember(
            GrantKey {
                git_root: f.path("repo").canonicalize().unwrap(),
                remote_url: "https://github.com/example/repo.git".into(),
                prerequisite: RememberedKind::GithubToken,
            },
            RememberedGrant::TokenEnv {
                var: "GH_TOKEN".into(),
            },
        )
        .unwrap();
    let persisted =
        std::fs::read_to_string(f.path("home/.awman/prerequisite-grants.json")).unwrap();
    assert!(persisted.contains("GH_TOKEN"));
    assert!(!persisted.contains(TOKEN));
    let out = f
        .command()
        .env("GH_TOKEN", TOKEN)
        .args(["config", "prerequisite-grants", "list"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", output(&out));
    assert!(output(&out).contains("GH_TOKEN"));
    assert!(!output(&out).contains(TOKEN));
    let out = f.run(&["config", "prerequisite-grants", "revoke", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", output(&out));
    assert!(PrerequisiteGrantStore::load(&f.path("home/.awman"))
        .records()
        .is_empty());
}

// Admission refusal is covered by wi0149_api_secret_usage_error_and_ssh_widening_gate.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wi0149_api_acquire_output_is_redacted_in_sse_events_and_daemon_logs() {
    let f = Fixture::new();
    f.teardown();
    f.git(&["config", "user.name", "Ada"]);
    f.git(&["config", "user.email", "ada@example.com"]);
    let acquire = f.path("bin/acquire");
    executable(&acquire, &format!("#!/bin/sh\necho {TOKEN}\n"));
    let home = api_process::ApiHome::new();
    configure_api(&home, &f, false);
    std::fs::write(home.home().join(".config/awman/config.json"), json!({"api":{"workDirs":[f.path("repo")]},"workflows":{"prerequisites":{"unresolved":"warn","githubToken":{"use":"allow","acquire":{"argv":[acquire]}}}}}).to_string()).unwrap();
    executable(
        &home.home().parent().unwrap().join("bin/docker"),
        &RECORDING_DOCKER.replace("WI0149_CONTROL", "FAKE_DOCKER_CONTROL"),
    );
    let server = home.start().await;
    let session = server
        .ready_session(json!({"session_type":"local","workdir":f.path("repo")}))
        .await;
    let record = server
        .run_command(&session, "exec workflow", &["wf.toml"])
        .await;
    let events = api_process::events_log(&record).await;
    assert!(events.contains("stdout leaked ***"), "{events}");
    let typed = awman::data::execution_event::parse_event_log(&events);
    let prerequisite = typed
        .iter()
        .position(|e| {
            matches!(
                e.payload,
                awman::data::execution_event::EventPayload::WorkflowPrerequisites { .. }
            )
        })
        .expect("successful run emits prerequisites");
    let step = typed
        .iter()
        .position(|e| {
            matches!(
                e.payload,
                awman::data::execution_event::EventPayload::WorkflowStepTransition { .. }
            )
        })
        .expect("successful run emits step transitions");
    assert!(prerequisite < step);
    assert!(events.contains("stderr leaked ***"), "{events}");
    assert!(!events.contains(TOKEN));
    assert!(!server.log().contains(TOKEN));
    let sse = reqwest::Client::new()
        .get(format!(
            "{}/v1/commands/{}/logs",
            server.url,
            record["id"].as_str().unwrap()
        ))
        .header("x-awman-session", &session)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!sse.contains(TOKEN));
    assert!(sse.contains("stdout leaked ***"));
    let argv = std::fs::read_to_string(home.control().join("docker.log")).unwrap();
    assert!(!argv.contains(TOKEN));
    for path in descendants(&home.home()) {
        if path
            .extension()
            .is_some_and(|e| e == "log" || e == "jsonl" || e == "json")
        {
            assert!(
                !std::fs::read_to_string(&path).unwrap().contains(TOKEN),
                "{}",
                path.display()
            );
        }
    }
}
