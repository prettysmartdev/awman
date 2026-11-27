//! Boundary journey of the Docker Sandboxes runtime against the real `sbx`
//! CLI (suite `boundary-sbx`, contract WI 0086 §13).
//!
//! Every test drives the debug `awman` binary through its production
//! `ready`, `exec prompt` and interactive entry points; the runtime is
//! reachable there only through the debug-build qualification seam under
//! `AWMAN_TEST_ISOLATION`, never through the release switch.
//!
//! Prerequisites are capabilities, so a missing one is `BLOCKED`, never a
//! skip or a pass:
//!
//! - `docker-sbx`: the `sbx` CLI, a supported release (>= 0.54.0 for these
//!   live claims) on Apple silicon macOS, the only qualification target.
//! - `sbx-test-identity`: a dedicated signed-in `HOME` for every `sbx`
//!   process and scoped provider credentials. A route whose provider
//!   credential is not provisioned blocks that route's live claim.
//! - `public-network`: provider endpoints and published images.
//!
//! Each test creates only sandboxes named from its own private workspace and
//! removes them, verifying their absence, also when it fails. Evidence (CLI,
//! kit and inspect identities, never a credential or prompt output) is
//! written to `AWMAN_TEST_EVIDENCE_DIR` as `sbx-boundary.<test>.json`.

use crate::support;
use awman::engine::agent::first_party::first_party;
use awman::engine::agent::sbx_contract::{
    admit_cli, kit_dir_v3, sbx_route, sha256_hex, EmittedKit, SbxCliAdmission, SbxDisposition,
    SBX_AGENTS,
};
use awman::engine::sandbox::naming::sandbox_name_for;
use awman_test_support::capability::{DockerSbx, PublicNetwork, SbxTestIdentity};
use awman_test_support::fixtures::{ConfigTree, RepoBuilder};
use awman_test_support::{Granted, TestSandbox};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

/// Minimum sbx release for a recorded live claim (work item: v0.54.x or later).
const LIVE_MINIMUM: (u64, u64, u64) = (0, 54, 0);
/// One bounded task. The prompt does not contain its expected answer, so an
/// echo of the prompt cannot satisfy the check.
const TASK_PROMPT: &str = "What is six times seven? Reply with only the number and use no tools.";
const TASK_ANSWER: &str = "42";
const TASK_DEADLINE: Duration = Duration::from_secs(300);
const CLI_DEADLINE: Duration = Duration::from_secs(120);

/// One live test: granted capabilities, a private workspace, and the
/// sandboxes it owns.
struct Live {
    sandbox: Option<TestSandbox>,
    sbx: Granted<DockerSbx>,
    identity: Granted<SbxTestIdentity>,
    workspace: PathBuf,
    owned: Vec<String>,
    evidence: Value,
    test: &'static str,
}

impl Live {
    fn new(test: &'static str) -> Live {
        let mut s = support::sandbox(test);
        let sbx = s.require::<DockerSbx>();
        let identity = s.require::<SbxTestIdentity>();
        let _network = s.require::<PublicNetwork>();
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            panic!(
                "BLOCKED[docker-sbx]: Docker Sandboxes is qualified on Apple silicon macOS only; \
                 this host is {}/{} (awman refuses it with no fallback)",
                std::env::consts::OS,
                std::env::consts::ARCH
            );
        }
        s.grant(&sbx);
        s.grant(&identity);
        s.redact(identity.anthropic_key());
        let version = s
            .command(&sbx.cli)
            .arg("--version")
            .deadline(CLI_DEADLINE)
            .run();
        let raw = version.stdout_text();
        let cli_version = match admit_cli(&raw) {
            SbxCliAdmission::Admitted(v) if (v.major, v.minor, v.patch) >= LIVE_MINIMUM => {
                v.to_string()
            }
            other => panic!(
                "BLOCKED[docker-sbx]: sbx --version is not a recorded supported release \
                 (>= {}.{}.{}): {other:?} from {raw:?}",
                LIVE_MINIMUM.0, LIVE_MINIMUM.1, LIVE_MINIMUM.2
            ),
        };
        let listing = s
            .command(&sbx.cli)
            .args(["--no-input", "ls", "--json"])
            .deadline(CLI_DEADLINE)
            .run();
        if !listing.exit.success() {
            panic!(
                "BLOCKED[sbx-test-identity]: the dedicated HOME is not signed in to Docker \
                 Sandboxes: {}",
                listing.stderr_text().trim()
            );
        }
        s.note("sbx_cli", &cli_version);
        s.note("sbx_identity", &identity.identity);
        ConfigTree::new()
            .global(json!({
                "runtime": {"kind": "sbx"},
                "agents": {"auth": {"refresh": {"enabled": false}}}
            }))
            .write(&s, None);
        let workspace = RepoBuilder::new(&s, "workspace")
            .file("README.md", "awman sbx boundary workspace\n")
            .init()
            .path()
            .canonicalize()
            .unwrap();
        let evidence = json!({
            "schema": 1,
            "test": test,
            "sbx_cli": cli_version,
            "identity": identity.identity,
            "agents": {},
        });
        Live {
            sandbox: Some(s),
            sbx,
            identity,
            workspace,
            owned: Vec::new(),
            evidence,
            test,
        }
    }

    fn s(&self) -> &TestSandbox {
        self.sandbox.as_ref().expect("sandbox is open until finish")
    }

    fn name(&self, agent: &str) -> String {
        sandbox_name_for(&self.workspace, agent)
    }

    /// `sbx <args>` with the dedicated identity.
    fn sbx(&self, args: &[&str]) -> awman_test_support::Output {
        self.s()
            .command(&self.sbx.cli)
            .args(args.iter().copied())
            .deadline(CLI_DEADLINE)
            .run()
    }

    /// The production `awman` binary in the workspace.
    fn awman(&self, args: &[&str], env: &[(&str, &str)]) -> awman_test_support::Output {
        let mut command = support::awman(self.s())
            .args(args.iter().copied())
            .current_dir(&self.workspace)
            .deadline(TASK_DEADLINE);
        for (key, value) in env {
            command = command.env(key, value);
        }
        command.run()
    }

    fn listed(&self) -> Vec<String> {
        let out = self.sbx(&["--no-input", "ls", "--json"]);
        out.assert_success();
        awman_test_support::sbx::listed_names(&out.stdout)
            .expect("unverifiable SBX listing; owned names retained")
    }

    /// The emitted kit of `agent` (after `awman ready`).
    fn kit(&self, agent: &str) -> EmittedKit {
        let dir = kit_dir_v3(&self.s().awman_home().join("kits"), agent).expect("supported route");
        EmittedKit::load(&dir).unwrap_or_else(|e| panic!("{agent}: emitted kit: {e}"))
    }

    /// The host identity record awman wrote for `name`.
    fn record(&self, name: &str) -> Value {
        let path = self
            .s()
            .awman_home()
            .join("sbx/sandboxes")
            .join(format!("{name}.json"));
        serde_json::from_slice(
            &std::fs::read(&path)
                .unwrap_or_else(|e| panic!("identity record {}: {e}", path.display())),
        )
        .unwrap()
    }

    /// The provider credential of `agent` and how awman's supported auth
    /// path delivers it, or BLOCKED for this route.
    fn credential(&self, agent: &str) -> Credential {
        let def = first_party(agent).unwrap();
        let route = sbx_route(agent).unwrap();
        let service = route.credential_service.unwrap_or("none");
        for var in &def.sandbox_auth_env_vars {
            if let Ok(value) = self.identity.provider_key(var) {
                self.s().redact(&value);
                return Credential::Env {
                    var: var.clone(),
                    value,
                };
            }
        }
        if def.sandbox_auth_env_vars.is_empty() {
            // No launch-time auto-auth: the operator must have approved a
            // credential for the service in the dedicated identity.
            let out = self.sbx(&[
                "--no-input",
                "secret",
                "ls",
                "--global",
                "--service",
                service,
                "--json",
            ]);
            let stored = out.exit.success()
                && serde_json::from_slice::<Value>(&out.stdout)
                    .ok()
                    .is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty()));
            if stored {
                return Credential::Approved;
            }
        }
        panic!(
            "BLOCKED[sbx-test-identity]: no scoped '{service}' credential is provisioned for \
             '{agent}' (variables {:?}; or a credential approved in the dedicated identity); \
             this route's live claim is blocked",
            def.sandbox_auth_env_vars
        );
    }

    /// One bounded headless task through `awman exec prompt`.
    fn headless_task(&mut self, agent: &str, credential: &Credential) -> Task {
        let overlay;
        let mut args = vec!["exec", "prompt", "--agent", agent, "--headless"];
        let mut env: Vec<(&str, &str)> = Vec::new();
        if let Credential::Env { var, value } = credential {
            overlay = format!("env({var})");
            args.extend(["--overlay", overlay.as_str()]);
            env.push((var.as_str(), value.as_str()));
        }
        args.push(TASK_PROMPT);
        self.own(agent);
        let out = self.awman(&args, &env);
        Task {
            exit: out.exit.success(),
            stdout: out.stdout_text(),
            stderr: out.stderr_text(),
        }
    }

    fn own(&mut self, agent: &str) {
        let name = self.name(agent);
        if !self.owned.contains(&name) {
            self.owned.push(name);
        }
    }

    fn record_agent(&mut self, agent: &str, fields: Value) {
        self.evidence["agents"][agent] = fields;
    }

    /// Remove every owned sandbox and verify that none survives.
    fn remove_owned(&mut self) -> Vec<String> {
        let mut unresolved = Vec::new();
        for name in &self.owned {
            let out = self
                .s()
                .command(&self.sbx.cli)
                .args(["rm", "--force", name.as_str()])
                .deadline(CLI_DEADLINE)
                .run();
            if !out.exit.success() {
                unresolved.push(name.clone());
            }
        }
        let listing = self.sbx(&["--no-input", "ls", "--json"]);
        let names = if listing.exit.success() {
            awman_test_support::sbx::listed_names(&listing.stdout)
        } else {
            None
        };
        for name in &self.owned {
            if names.as_ref().is_none_or(|names| names.contains(name)) && !unresolved.contains(name)
            {
                unresolved.push(name.clone());
            }
        }
        self.owned.retain(|name| unresolved.contains(name));
        unresolved
    }

    fn write_evidence(&self) {
        let Some(dir) = awman_test_support::runtime_env::get().evidence_dir.clone() else {
            return;
        };
        let file = dir.join(format!(
            "sbx-boundary.{}.json",
            self.test.replace("::", ".")
        ));
        let text = serde_json::to_string_pretty(&self.evidence).unwrap();
        assert!(
            !text.contains(self.identity.anthropic_key()),
            "evidence carries a credential"
        );
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(file, text).unwrap();
    }

    fn finish(mut self) {
        let survivors = self.remove_owned();
        assert!(
            survivors.is_empty(),
            "LEAK: owned sandboxes survived removal: {survivors:?}"
        );
        self.write_evidence();
        self.sandbox.take().expect("open sandbox").finish();
    }
}

impl Drop for Live {
    /// The failure path: remove owned sandboxes before the sandbox closes.
    fn drop(&mut self) {
        if self.sandbox.is_some() && !self.owned.is_empty() {
            let survivors = self.remove_owned();
            if !survivors.is_empty() {
                self.s().note("leaked_sandboxes", &survivors.join(","));
            }
        }
    }
}

enum Credential {
    /// Delivered by awman's `env(VAR)` overlay path (sandbox-scoped secret).
    Env { var: String, value: String },
    /// Approved by the operator in the dedicated identity.
    Approved,
}

struct Task {
    exit: bool,
    stdout: String,
    stderr: String,
}

impl Task {
    fn assert_answered(&self, what: &str) {
        assert!(
            self.exit,
            "{what}: the task failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout, self.stderr
        );
        assert!(
            !self.stdout.trim().is_empty() && self.stdout.contains(TASK_ANSWER),
            "{what}: no answer in the agent output\n{}",
            self.stdout
        );
    }
}

fn expected_disposition(agent: &str) -> &'static str {
    match sbx_route(agent).unwrap().disposition {
        SbxDisposition::PublishedWorkloadWithMixin { .. } => "published-workload-with-mixin",
        SbxDisposition::OwnedWorkload => "owned-workload",
        SbxDisposition::Refused { .. } => "refused",
    }
}

/// All eight dispositions: every supported route's kit is emitted by
/// `awman ready`, validated by the signed-in real CLI (not only offline)
/// and describes the matrix composition; both refused routes are refused by
/// `ready` and `exec prompt` before any sandbox exists.
#[test]
fn every_route_disposition_is_emitted_validated_or_refused_by_the_live_cli() {
    let mut live =
        Live::new("sbx::every_route_disposition_is_emitted_validated_or_refused_by_the_live_cli");
    for agent in SBX_AGENTS {
        let route = sbx_route(agent).unwrap();
        let ready = live.awman(&["ready", "--agent", agent], &[]);
        if !route.is_supported() {
            assert!(!ready.exit.success(), "{agent}: ready must refuse");
            let text = format!("{}{}", ready.stdout_text(), ready.stderr_text());
            assert!(text.contains("Docker Sandboxes"), "{agent}: {text}");
            let task = live.awman(
                &[
                    "exec",
                    "prompt",
                    "--agent",
                    agent,
                    "--headless",
                    TASK_PROMPT,
                ],
                &[],
            );
            assert!(!task.exit.success(), "{agent}: launch must refuse");
            assert!(!live.listed().contains(&live.name(agent)), "{agent}");
            live.record_agent(agent, json!({"disposition": "refused"}));
            continue;
        }
        ready.assert_success();
        let kit = live.kit(agent);
        let validate = live.sbx(&["--no-input", "kit", "validate", &kit.dir.to_string_lossy()]);
        validate.assert_success();
        assert_eq!(
            serde_json::to_value(kit.manifest.kind).unwrap(),
            if route.is_owned_workload() {
                "workload"
            } else {
                "mixin"
            },
            "{agent}"
        );
        assert_eq!(Some(kit.manifest.handle.clone()), route.kit_handle());
        live.record_agent(
            agent,
            json!({
                "disposition": expected_disposition(agent),
                "kit_handle": kit.manifest.handle,
                "kit_digest": kit.digest,
            }),
        );
    }
    live.finish();
}

/// One supported route end to end: effective composition, fresh create,
/// headless and interactive tasks, workspace read/write and scope, reuse of
/// a running and of a stopped sandbox, removal.
fn route_journey(test: &'static str, agent: &'static str) {
    let mut live = Live::new(test);
    let credential = live.credential(agent);
    live.awman(&["ready", "--agent", agent], &[])
        .assert_success();
    let kit = live.kit(agent);
    let name = live.name(agent);

    // Fresh: awman creates the sandbox from the matrix selector.
    assert!(
        !live.listed().contains(&name),
        "{name} exists before the test"
    );
    live.headless_task(agent, &credential)
        .assert_answered(&format!("{agent}: fresh headless task"));
    assert!(live.listed().contains(&name));
    let record = live.record(&name);
    assert_eq!(record["state"], "created");
    assert_eq!(record["kit_digest"], kit.digest.as_str());
    assert_eq!(record["disposition"], expected_disposition(agent));
    let created_at = record["created_at"].clone();

    // Effective composition as the CLI reports it.
    let inspect = live.sbx(&["--no-input", "inspect", &name, "--json"]);
    inspect.assert_success();
    let inspect_text = inspect.stdout_text();
    assert!(
        inspect_text.contains(&kit.manifest.handle),
        "{agent}: inspect does not name kit {}: {inspect_text}",
        kit.manifest.handle
    );

    // Workspace: read and write at the same host path; the parent is not
    // reachable.
    let ws = live.workspace.display().to_string();
    std::fs::write(live.workspace.join("host-sentinel"), "from-host\n").unwrap();
    let outside = live.workspace.parent().unwrap().join("outside-sentinel");
    std::fs::write(&outside, "outside\n").unwrap();
    let read = live.sbx(&[
        "--no-input",
        "exec",
        &name,
        "cat",
        &format!("{ws}/host-sentinel"),
    ]);
    read.assert_success();
    assert_eq!(read.stdout_text().trim(), "from-host");
    live.sbx(&[
        "--no-input",
        "exec",
        &name,
        "sh",
        "-c",
        &format!("printf from-guest > '{ws}/guest-sentinel'"),
    ])
    .assert_success();
    assert_eq!(
        std::fs::read_to_string(live.workspace.join("guest-sentinel")).unwrap(),
        "from-guest"
    );
    let escaped = live.sbx(&[
        "--no-input",
        "exec",
        &name,
        "cat",
        &outside.to_string_lossy(),
    ]);
    assert!(
        !escaped.exit.success() || !escaped.stdout_text().contains("outside"),
        "{agent}: the workspace parent is reachable inside the sandbox"
    );

    // Running and stopped reuse must refuse without changing the identity.
    for stopped in [false, true] {
        if stopped {
            live.sbx(&["stop", &name]).assert_success();
        }
        let task = live.headless_task(agent, &credential);
        assert!(!task.exit && task.stderr.contains("session exclusion"));
        assert_eq!(live.record(&name)["created_at"], created_at);
    }
    assert!(live.remove_owned().is_empty());
    // Interactive execution gets its own fresh sandbox.
    live.own(agent);
    interactive_task(&mut live, agent, &credential);

    let record = live.record(&name);
    live.record_agent(
        agent,
        json!({
            "disposition": expected_disposition(agent),
            "kit_handle": kit.manifest.handle,
            "kit_digest": kit.digest,
            "inspect_sha256": sha256_hex(inspect_text.as_bytes()),
            "kits": record["kits"],
            "record_inspect_sha256": record["inspect_sha256"],
            "workload": record["workload"],
        }),
    );
    live.finish();
}

fn interactive_task(live: &mut Live, agent: &str, credential: &Credential) {
    let mut command = live
        .s()
        .pty_command(support::awman_exe())
        .label("awman interactive")
        .args(["exec", "prompt", "--agent", agent])
        .current_dir(&live.workspace)
        .size(40, 120)
        .deadline(TASK_DEADLINE);
    let overlay;
    if let Credential::Env { var, value } = credential {
        overlay = format!("env({var})");
        command = command
            .args(["--overlay", overlay.as_str()])
            .env(var, value);
    }
    let mut child = command.arg(TASK_PROMPT).spawn();
    child.wait_for_output(TASK_ANSWER, TASK_DEADLINE);
    child.kill_and_reap();
    // The interrupted client leaves no agent session attached: the next
    // launch is not refused as busy.
}

#[test]
fn claude_published_workload_with_mixin_journey() {
    route_journey(
        "sbx::claude_published_workload_with_mixin_journey",
        "claude",
    );
}

#[test]
fn codex_published_workload_with_mixin_journey() {
    route_journey("sbx::codex_published_workload_with_mixin_journey", "codex");
}

#[test]
fn copilot_published_workload_with_mixin_journey() {
    route_journey(
        "sbx::copilot_published_workload_with_mixin_journey",
        "copilot",
    );
}

#[test]
fn opencode_owned_workload_journey() {
    route_journey("sbx::opencode_owned_workload_journey", "opencode");
}

#[test]
fn cline_owned_workload_journey() {
    route_journey("sbx::cline_owned_workload_journey", "cline");
}

#[test]
fn antigravity_owned_workload_journey() {
    route_journey("sbx::antigravity_owned_workload_journey", "antigravity");
}

/// A rotated sandbox-scoped secret replaces the old one in a reused
/// sandbox (`secret set ... --force`), and awman never writes a global
/// secret.
#[test]
fn reused_sandbox_refuses_credential_rotation_without_mutation() {
    let mut live = Live::new("sbx::reused_sandbox_refuses_credential_rotation_without_mutation");
    let Credential::Env { var, value } = live.credential("claude") else {
        unreachable!("claude authenticates through env(ANTHROPIC_API_KEY)");
    };
    let before_global = live.sbx(&[
        "--no-input",
        "secret",
        "ls",
        "--global",
        "--service",
        "anthropic",
        "--json",
    ]);
    live.awman(&["ready", "--agent", "claude"], &[])
        .assert_success();
    let valid = Credential::Env {
        var: var.clone(),
        value: value.clone(),
    };
    live.headless_task("claude", &valid)
        .assert_answered("claude: first key");
    let rotated = "sk-ant-awman-boundary-rotated-invalid";
    live.s().redact(rotated);
    let invalid = Credential::Env {
        var: var.clone(),
        value: rotated.to_string(),
    };
    let task = live.headless_task("claude", &invalid);
    assert!(
        !task.exit || !task.stdout.contains(TASK_ANSWER),
        "the rotated (invalid) key did not replace the sandbox secret"
    );
    assert!(task.stderr.contains("session exclusion"));
    let after_global = live.sbx(&[
        "--no-input",
        "secret",
        "ls",
        "--global",
        "--service",
        "anthropic",
        "--json",
    ]);
    assert_eq!(
        before_global.stdout_text(),
        after_global.stdout_text(),
        "awman changed a global secret"
    );
    live.finish();
}

/// A sandbox whose host identity no longer matches the current kit is
/// refused at launch with removal instructions and is never removed; `ready`
/// reports it.
#[test]
fn stale_kit_sandbox_is_refused_and_never_removed() {
    let mut live = Live::new("sbx::stale_kit_sandbox_is_refused_and_never_removed");
    let credential = live.credential("claude");
    live.awman(&["ready", "--agent", "claude"], &[])
        .assert_success();
    live.headless_task("claude", &credential)
        .assert_answered("claude: fresh task");
    let name = live.name("claude");
    let path = live
        .s()
        .awman_home()
        .join("sbx/sandboxes")
        .join(format!("{name}.json"));
    let mut record = live.record(&name);
    record["kit_digest"] = Value::String("0".repeat(64));
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let task = live.headless_task("claude", &credential);
    assert!(!task.exit, "a stale sandbox was reused");
    assert!(task.stderr.contains("cannot be reused"), "{}", task.stderr);
    assert!(
        live.listed().contains(&name),
        "awman removed the stale sandbox"
    );
    let ready = live.awman(&["ready", "--agent", "claude"], &[]);
    ready.assert_success();
    let text = format!("{}{}", ready.stdout_text(), ready.stderr_text());
    assert!(text.contains("will be refused at launch"), "{text}");
    assert!(live.listed().contains(&name));
    live.finish();
}

/// An agent that fails at startup reports a failed launch, leaves no agent
/// session attached, and the sandbox is reused by the next valid launch.
#[test]
fn failed_startup_is_reported_and_reuse_is_refused() {
    let mut live = Live::new("sbx::failed_startup_is_reported_and_reuse_is_refused");
    let credential = live.credential("opencode");
    live.awman(&["ready", "--agent", "opencode"], &[])
        .assert_success();
    let Credential::Env { var, value } = &credential else {
        unreachable!("opencode authenticates through env(ANTHROPIC_API_KEY)");
    };
    live.own("opencode");
    let overlay = format!("env({var})");
    let failed = live.awman(
        &[
            "exec",
            "prompt",
            "--agent",
            "opencode",
            "--headless",
            "--overlay",
            &overlay,
            "--model",
            "awman-boundary/no-such-model",
            TASK_PROMPT,
        ],
        &[(var.as_str(), value.as_str())],
    );
    assert!(!failed.exit.success(), "an unknown model started a task");
    let refused = live.headless_task("opencode", &credential);
    assert!(!refused.exit && refused.stderr.contains("session exclusion"));
    live.finish();
}

/// WI 0152: Docker Sandboxes share the workspace read-write, so the runtime
/// reports that it cannot mount it read-only and every command refuses
/// `--read-only-workspace` instead of launching with a writable workspace.
#[test]
fn sbx_reports_that_it_cannot_mount_the_workspace_read_only() {
    use awman::engine::agent_runtime::AgentRuntimeEngine as _;
    use awman::engine::sandbox::SandboxRuntime;
    let live = Live::new("sbx::sbx_reports_that_it_cannot_mount_the_workspace_read_only");
    let runtime = SandboxRuntime::dsbx_in(support::env_snapshot(live.s()))
        .unwrap_or_else(|e| panic!("the sandbox runtime was refused on this platform: {e}"));
    assert!(!runtime.capabilities().read_only_workspace());
    assert!(
        !awman::command::daemon_api::capabilities::advertised_for_runtime(
            runtime.capabilities(),
            &[]
        )
        .iter()
        .any(|c| c == "read_only_workspace")
    );
    live.finish();
}

#[test]
fn claude_effective_plan_auto_yolo_and_deny_policy_are_observed() {
    let mut live = Live::new("sbx::claude_effective_plan_auto_yolo_and_deny_policy_are_observed");
    let Credential::Env { var, value } = live.credential("claude") else {
        unreachable!()
    };
    let overlay = format!("env({var})");
    let sentinel = live.workspace.join("policy-sentinel");
    // A successful allowed edit is a positive control for both the task and credential.
    for mode in ["--yolo", "--auto", "--plan"] {
        let model = if mode == "--auto" { "haiku" } else { "sonnet" };
        std::fs::write(&sentinel, "before").unwrap();
        live.own("claude");
        let prompt = format!("Use the Write or Edit tool to replace the entire contents of {} with exactly changed. Do not use a shell. If denied, report denied.", sentinel.display());
        let out = live.awman(
            &[
                "exec",
                "prompt",
                "--agent",
                "claude",
                "--headless",
                mode,
                "--model",
                model,
                "--overlay",
                &overlay,
                &prompt,
            ],
            &[(var.as_str(), value.as_str())],
        );
        out.assert_success();
        // Read the model reported in actual assistant responses, rather than
        // accepting the requested settings file as proof. Two distinct aliases
        // ensure silently using a fixed default cannot satisfy both cases.
        let transcript = live.sbx(&[
            "--no-input",
            "exec",
            &live.name("claude"),
            "sh",
            "-c",
            "find /home/agent/.claude/projects -type f -name '*.jsonl' -exec cat '{}' +",
        ]);
        transcript.assert_success();
        let models: Vec<String> = transcript
            .stdout_text()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|v| v["type"] == "assistant")
            .filter_map(|v| v["message"]["model"].as_str().map(str::to_string))
            .collect();
        assert!(
            !models.is_empty() && models.iter().all(|observed| observed.contains(model)),
            "requested {model}, observed actual response models {models:?}"
        );
        live.evidence["effective_models"][mode] = json!(models);
        let actual = std::fs::read_to_string(&sentinel).unwrap();
        if mode == "--plan" {
            assert_eq!(actual, "before", "plan mode permitted a write");
        } else {
            assert_eq!(
                actual.trim(),
                "changed",
                "{mode} positive control did not edit"
            );
        }
        // A restrictive request against the permissive sandbox must refuse,
        // with no write and no new receipt.
        let receipt = live.sbx(&[
            "--no-input",
            "exec",
            &live.name("claude"),
            "cat",
            "/var/lib/awman/receipt",
        ]);
        receipt.assert_success();
        let refused = live.awman(
            &[
                "exec",
                "prompt",
                "--agent",
                "claude",
                "--headless",
                "--plan",
                &prompt,
            ],
            &[],
        );
        assert!(!refused.exit.success() && refused.stderr_text().contains("session exclusion"));
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), actual);
        let after = live.sbx(&[
            "--no-input",
            "exec",
            &live.name("claude"),
            "cat",
            "/var/lib/awman/receipt",
        ]);
        after.assert_success();
        assert_eq!(receipt.stdout, after.stdout);
        assert!(live.remove_owned().is_empty());
    }
    // Deny every tool through managed settings, then request a forbidden edit.
    ConfigTree::new().global(json!({"runtime":{"kind":"sbx"},"agents":{"auth":{"refresh":{"enabled":false}},"yolo":{"disallowedTools":["Write","Edit","Bash"]}}})).write(live.s(),None);
    std::fs::write(&sentinel, "before").unwrap();
    live.own("claude");
    let prompt = format!(
        "Write exactly changed into {} using any available tool.",
        sentinel.display()
    );
    let out = live.awman(
        &[
            "exec",
            "prompt",
            "--agent",
            "claude",
            "--headless",
            "--yolo",
            "--overlay",
            &overlay,
            &prompt,
        ],
        &[(var.as_str(), value.as_str())],
    );
    out.assert_success();
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        "before",
        "denied tool modified the workspace"
    );
    live.finish();
}

#[test]
fn fresh_create_is_quiescent_and_owned_launchers_require_a_receipt() {
    let mut live =
        Live::new("sbx::fresh_create_is_quiescent_and_owned_launchers_require_a_receipt");
    for agent in [
        "claude",
        "codex",
        "copilot",
        "opencode",
        "cline",
        "antigravity",
    ] {
        live.awman(&["ready", "--agent", agent], &[])
            .assert_success();
        let kit = live.kit(agent);
        let name = live.name(agent);
        live.own(agent);
        let mut create = vec!["--no-input", "create", "--name", &name];
        let dir = kit.dir.to_string_lossy();
        let ws = live.workspace.to_string_lossy();
        let route = sbx_route(agent).unwrap();
        if let SbxDisposition::PublishedWorkloadWithMixin { workload } = route.disposition {
            create.extend(["--kit", dir.as_ref(), workload]);
        } else {
            create.push(dir.as_ref());
        }
        create.push(ws.as_ref());
        live.sbx(&create).assert_success();
        let before = live.sbx(&[
            "--no-input",
            "exec",
            &name,
            "sh",
            "-c",
            "test ! -e /var/lib/awman/receipt && ps -eo comm",
        ]);
        before.assert_success();
        for binary in ["opencode", "cline", "agy", "claude", "codex", "copilot"] {
            assert!(
                !before
                    .stdout_text()
                    .lines()
                    .any(|line| line.trim() == binary),
                "agent started during create"
            );
        }
        let inspect = live.sbx(&["--no-input", "inspect", &name, "--json"]);
        inspect.assert_success();
        let inspect_json: Value = serde_json::from_slice(&inspect.stdout).expect("inspect JSON");
        let image = inspect_metadata(&inspect_json, &["image_digest", "imagedigest", "image"])
            .filter(|v| v.as_str().is_some_and(|s| s.contains("sha256:")))
            .expect("qualification requires the resolved workload image digest");
        let entrypoint = inspect_metadata(&inspect_json, &["entrypoint"])
            .expect("qualification requires the actual workload entrypoint");
        live.record_agent(agent,json!({"inspect_sha256":sha256_hex(&inspect.stdout),"image":image,"entrypoint":entrypoint,"pre_apply_processes_sha256":sha256_hex(&before.stdout)}));
        // Published images must expose every helper needed for synchronous apply.
        live.sbx(&["--no-input","exec",&name,"sh","-c","for h in base64 sha256sum install mv rm runuser id mktemp dirname chmod; do command -v $h >/dev/null || exit 69; done"]).assert_success();
        if !route.is_owned_workload() {
            continue;
        }
        let run = live.sbx(&["run", "--name", &name, "--", "--version"]);
        assert_eq!(
            run.exit,
            awman_test_support::Exit::Code(78),
            "sbx bypassed the receipt gate: {}",
            run.stderr_text()
        );
        // A missing helper cannot issue a receipt or start an agent.
        live.sbx(&[
            "--no-input",
            "exec",
            "-u",
            "root",
            &name,
            "mv",
            "/usr/local/lib/awman/awman-apply",
            "/usr/local/lib/awman/awman-apply.saved",
        ])
        .assert_success();
        let failed = live.sbx(&[
            "--no-input",
            "exec",
            "-u",
            "root",
            &name,
            "/usr/local/lib/awman/awman-apply",
        ]);
        assert_eq!(failed.exit, awman_test_support::Exit::Code(127));
        live.sbx(&[
            "--no-input",
            "exec",
            &name,
            "test",
            "!",
            "-e",
            "/var/lib/awman/receipt",
        ])
        .assert_success();
    }
    live.finish();
}

#[test]
fn requested_models_are_not_silently_ignored_by_supported_routes() {
    let mut live = Live::new("sbx::requested_models_are_not_silently_ignored_by_supported_routes");
    for agent in ["claude", "codex", "opencode", "cline", "antigravity"] {
        let credential = live.credential(agent);
        let overlay;
        let mut args = vec![
            "exec",
            "prompt",
            "--agent",
            agent,
            "--headless",
            "--model",
            "awman-qualification-invalid-model",
        ];
        let mut env = Vec::new();
        if let Credential::Env { var, value } = &credential {
            overlay = format!("env({var})");
            args.extend(["--overlay", overlay.as_str()]);
            env.push((var.as_str(), value.as_str()));
        }
        args.push(TASK_PROMPT);
        live.own(agent);
        let out = live.awman(&args, &env);
        let diagnostic = format!("{}{}", out.stdout_text(), out.stderr_text());
        assert!(
            !out.exit.success(),
            "{agent} ignored the requested invalid model"
        );
        assert!(
            diagnostic.contains("awman-qualification-invalid-model")
                && diagnostic.to_lowercase().contains("model"),
            "{agent}: failure did not identify the requested model: {diagnostic}"
        );
        live.record_agent(agent, json!({"requested_invalid_model_refused":true}));
    }
    live.finish();
}

#[test]
fn concurrent_same_name_launch_cannot_apply_over_a_running_launch() {
    let mut live = Live::new("sbx::concurrent_same_name_launch_cannot_apply_over_a_running_launch");
    let Credential::Env { var, value } = live.credential("claude") else {
        unreachable!()
    };
    let overlay = format!("env({var})");
    live.own("claude");
    let mut first = support::awman(live.s())
        .args([
            "exec",
            "prompt",
            "--agent",
            "claude",
            "--headless",
            "--yolo",
            "--overlay",
            &overlay,
            "Use Bash to sleep for 20 seconds, then reply ready.",
        ])
        .env(&var, &value)
        .current_dir(&live.workspace)
        .deadline(TASK_DEADLINE)
        .spawn();
    let path = live
        .s()
        .awman_home()
        .join("sbx/sandboxes")
        .join(format!("{}.json", live.name("claude")));
    awman_test_support::wait_for(
        "first launch acquired the sandbox lock",
        CLI_DEADLINE,
        || path.exists().then_some(()),
    );
    assert!(
        first.try_exit().is_none(),
        "first launch ended before contention was exercised"
    );
    let second = live.awman(
        &[
            "exec",
            "prompt",
            "--agent",
            "claude",
            "--headless",
            TASK_PROMPT,
        ],
        &[],
    );
    assert!(!second.exit.success());
    assert!(
        second.stderr_text().contains("already in use"),
        "{}",
        second.stderr_text()
    );
    first.kill_and_reap();
    live.finish();
}

fn inspect_metadata<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .iter()
            .find(|(k, _)| keys.contains(&k.to_ascii_lowercase().as_str()))
            .map(|(_, v)| v)
            .or_else(|| map.values().find_map(|v| inspect_metadata(v, keys))),
        Value::Array(values) => values.iter().find_map(|v| inspect_metadata(v, keys)),
        _ => None,
    }
}
