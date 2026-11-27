//! Component tests of the SBX launch sequence through the production
//! `run_interactive` path, with a fake `sbx` executable and a prepared kit
//! directory (so no real templates, daemon or provider are needed).
//!
//! The fake records every argv (one line per call), the stdin of `secret
//! set` and of the apply step, and the session environment `sbx run` saw. Its
//! behaviour is steered by files in its private state directory.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use awman_test_support::{TestCase, TestSandbox};

use super::*;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::engine::agent::sbx_contract::{
    kit_digest, KitKind, KitManifest, KIT_ID_REL, KIT_MANIFEST_FILE, KIT_MANIFEST_SCHEMA,
};
use crate::engine::agent_runtime::frontend::{AgentIo, AgentProgress};
use crate::engine::container::options::EnvVar;
use crate::engine::sandbox::options::SandboxOption;

const FAKE_SBX: &str = r#"#!/bin/sh
S="$FAKE_SBX_STATE"
n=$(cat "$S/count" 2>/dev/null || echo 0)
n=$((n+1))
echo "$n" > "$S/count"
printf '%s\n' "$*" >> "$S/argv.log"
[ "$1" = "--no-input" ] && shift
case "$1" in
  --version) cat "$S/version"; exit 0 ;;
  ls)
    if [ -f "$S/swap-workspace" ]; then
      workspace=$(cat "$S/swap-workspace")
      mv "$workspace" "$workspace.original"
      ln -s "$S" "$workspace"
    fi
    cat "$S/ls.json"; exit 0 ;;
  kit) exit 0 ;;
  create)
    code=$(cat "$S/create.exit" 2>/dev/null || echo 0)
    if [ "$code" != 0 ]; then echo "create failed" >&2; exit "$code"; fi
    printf '[{"name":"%s"}]' "$3" > "$S/ls.json"
    echo "created $3"
    exit 0 ;;
  inspect) cat "$S/inspect.json" 2>/dev/null || echo '{}'; exit 0 ;;
  secret)
    if [ "$2" = set ]; then
      cat > "$S/secret.last"
      cat "$S/secret.last" >> "$S/secret.in"
      if [ -f "$S/echo-secret" ]; then cat "$S/secret.last"; cat "$S/secret.last" >&2; exit 1; fi
      exit 0
    fi
    cat "$S/secret_ls.json" 2>/dev/null || echo '[]'
    exit 0 ;;
  exec)
    case "$*" in
      *awman-apply*)
        cat > "$S/apply.$n.in"
        mode=$(cat "$S/apply.mode" 2>/dev/null || echo ok)
        launch=$(sed -n 's/^launch //p' "$S/apply.$n.in")
        kit=$(sed -n 's/^kit //p' "$S/apply.$n.in")
        files=$(grep -c '^file ' "$S/apply.$n.in")
        case "$mode" in
          ok) echo "AWMAN-APPLY-OK $launch $kit $files"; exit 0 ;;
          stale) echo "AWMAN-APPLY-STALE oldkit"; exit 3 ;;
          missing) echo "awman-apply: not found" >&2; exit 127 ;;
          helper) echo "awman-apply: base64 is missing" >&2; exit 69 ;;
          noack) exit 0 ;;
          hang) sleep 30; exit 0 ;;
          pipehold) (sleep 30) & exit 0 ;;
        esac ;;
      *) exit 1 ;;
    esac ;;
  run)
    printf 'launch=%s\nplain=%s\n' "$AWMAN_LAUNCH_ID" "${PLAIN-}" > "$S/run.$n.env"
    echo "agent output"
    exit 0 ;;
  stop|rm) exit 0 ;;
esac
echo "unexpected: $*" >&2
exit 2
"#;

const SECRET: &str = "sk-ant-test-secret-value";

struct Fixture {
    sandbox: TestSandbox,
    state: PathBuf,
    snapshot: crate::data::config::env::EnvSnapshot,
}

impl Fixture {
    fn new(test: &'static str) -> Self {
        let sandbox = TestSandbox::for_test(TestCase::component(test));
        let bin = sandbox.tmp().join("fakebin");
        let state = sandbox.tmp().join("fake-sbx");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let exe = bin.join("sbx");
        std::fs::write(&exe, FAKE_SBX).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(state.join("version"), "sbx version v0.54.1 (test)\n").unwrap();
        std::fs::write(state.join("ls.json"), "[]").unwrap();
        let mut env: std::collections::BTreeMap<String, String> = sandbox
            .env()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        let path = env.get("PATH").cloned().unwrap_or_default();
        env.insert("PATH".into(), format!("{}:{path}", bin.display()));
        env.insert("FAKE_SBX_STATE".into(), state.display().to_string());
        env.insert("ANTHROPIC_API_KEY".into(), SECRET.into());
        env.insert("PLAIN".into(), "plain-value".into());
        let snapshot = crate::data::config::env::EnvSnapshot::from_full_env(env);
        Self {
            sandbox,
            state,
            snapshot,
        }
    }

    fn set(&self, file: &str, content: &str) {
        std::fs::write(self.state.join(file), content).unwrap();
    }

    fn workspace(&self) -> PathBuf {
        self.sandbox.work()
    }

    fn name(&self, agent: &str) -> String {
        sandbox_name_for(&self.workspace(), agent)
    }

    fn data_home(&self) -> PathBuf {
        crate::data::fs::data_paths::data_home(&self.snapshot).unwrap()
    }

    /// Calls since the last `reset_calls`, one argv string per call.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.state.join("argv.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn reset_calls(&self) {
        let _ = std::fs::remove_file(self.state.join("argv.log"));
    }

    /// The subcommand of each call, `--no-input` dropped (`secret set`,
    /// `kit validate` keep two words).
    fn verbs(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|line| {
                let words: Vec<&str> = line
                    .split_whitespace()
                    .filter(|w| *w != "--no-input")
                    .collect();
                match words.as_slice() {
                    ["secret", sub, ..] | ["kit", sub, ..] => format!("{} {sub}", words[0]),
                    ["exec", "-i", "-u", "root", ..] => "apply".into(),
                    [first, ..] => (*first).to_string(),
                    [] => String::new(),
                }
            })
            .collect()
    }

    fn files_with_prefix(&self, prefix: &str) -> Vec<String> {
        let mut names: Vec<(u32, String)> = std::fs::read_dir(&self.state)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let n = name.strip_prefix(prefix)?.split('.').next()?.parse().ok()?;
                Some((n, std::fs::read_to_string(e.path()).unwrap()))
            })
            .collect();
        names.sort();
        names.into_iter().map(|(_, text)| text).collect()
    }

    /// Write a complete v3 kit for `agent` where production emits it.
    /// `variant` changes the kit content and therefore its digest.
    fn kit(&self, agent: &str, variant: &str) -> EmittedKit {
        let dir = kit_dir(&self.snapshot, agent).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let route = sbx_route(agent).unwrap();
        let handle = route.kit_handle().unwrap();
        std::fs::create_dir_all(dir.join("rootfs/usr/local/lib/awman")).unwrap();
        let manifest = KitManifest {
            schema: KIT_MANIFEST_SCHEMA,
            agent: agent.into(),
            kind: if route.is_owned_workload() {
                KitKind::Workload
            } else {
                KitKind::Mixin
            },
            handle: handle.clone(),
            descriptor: format!("{handle}.yaml"),
            dockerfile: format!("{handle}.dockerfile"),
            apply_protocol: crate::engine::agent::sbx_contract::APPLY_PROTOCOL,
        };
        std::fs::write(
            dir.join(KIT_MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{handle}.yaml")),
            format!("# syntax=docker/sandbox-kit:3\nschemaVersion: \"3\"\n# {variant}\n"),
        )
        .unwrap();
        std::fs::write(dir.join(format!("{handle}.dockerfile")), "FROM scratch\n").unwrap();
        std::fs::write(
            dir.join("rootfs/usr/local/lib/awman/awman-apply"),
            "#!/bin/sh\n",
        )
        .unwrap();
        let digest = kit_digest(&dir).unwrap();
        std::fs::write(dir.join(KIT_ID_REL), format!("{digest}\n")).unwrap();
        EmittedKit::load(&dir).unwrap()
    }

    fn options(&self, agent: &str, extra: Vec<SandboxOption>) -> ResolvedSandboxOptions {
        let mut options = vec![
            SandboxOption::AgentId(agent.into()),
            SandboxOption::WorkspaceDir(self.workspace()),
            SandboxOption::Interactive(false),
            SandboxOption::SeededPrompt("hello".into()),
            SandboxOption::EnvPassthrough(EnvVar("ANTHROPIC_API_KEY".into())),
            SandboxOption::EnvPassthrough(EnvVar("PLAIN".into())),
        ];
        options.extend(extra);
        ResolvedSandboxOptions {
            host_env: Some(self.snapshot.clone()),
            mount_guard: Some(crate::data::mount_authority::MountSourceGuard {
                authority: crate::data::mount_authority::MountAuthority::local_invocation(
                    &self.workspace(),
                ),
                sources: vec![(
                    self.workspace(),
                    crate::data::mount_authority::SourceIdentity::of(&self.workspace()).unwrap(),
                )],
            }),
            ..ResolvedSandboxOptions::resolve(options)
        }
    }

    fn finish(self) {
        self.sandbox.finish();
    }
}

#[derive(Clone, Default)]
struct Messages(Arc<Mutex<Vec<UserMessage>>>);

impl Messages {
    fn text(&self) -> String {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

struct TestFrontend {
    messages: Messages,
    stdout: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    stderr: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
}

impl UserMessageSink for TestFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.messages.0.lock().unwrap().push(msg);
    }
    fn replay_queued(&mut self) {}
}

#[async_trait::async_trait]
impl AgentFrontend for TestFrontend {
    fn report_status(&mut self, _status: AgentStatus) {}
    fn report_progress(&mut self, _progress: AgentProgress) {}
    fn take_io(&mut self) -> AgentIo {
        let (stdout, stdout_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stderr, stderr_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        self.stdout = Some(stdout_rx);
        self.stderr = Some(stderr_rx);
        AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn prepared_kit(
    snapshot: &crate::data::config::env::EnvSnapshot,
    agent: &str,
) -> Result<EmittedKit, EngineError> {
    Ok(EmittedKit::load(&kit_dir(snapshot, agent)?)?)
}

/// Launch and wait. Returns the exit code and the messages.
fn launch(
    rt: &tokio::runtime::Runtime,
    options: ResolvedSandboxOptions,
) -> (Result<i32, EngineError>, Messages) {
    let messages = Messages::default();
    let frontend = TestFrontend {
        messages: messages.clone(),
        stdout: None,
        stderr: None,
    };
    let result = rt.block_on(async move {
        let mut execution = run_with_kit_loader(options, Box::new(frontend), &prepared_kit)?;
        execution.wait().await.map(|info| info.exit_code)
    });
    (result, messages)
}

fn apply_doc_launch_id(doc: &str) -> String {
    doc.lines()
        .find_map(|l| l.strip_prefix("launch "))
        .unwrap()
        .to_string()
}

#[test]
fn fresh_launch_creates_registers_applies_and_only_then_runs_the_agent() {
    let fx = Fixture::new("engine::sandbox::dsbx::backend::launch_tests::fresh_launch_creates_registers_applies_and_only_then_runs_the_agent");
    let kit = fx.kit("claude", "v1");
    let rt = runtime();
    let name = fx.name("claude");
    let (result, messages) = launch(
        &rt,
        fx.options("claude", vec![SandboxOption::PermissionMode("plan".into())]),
    );
    assert_eq!(result.unwrap(), 0, "{}", messages.text());
    assert_eq!(
        fx.verbs(),
        [
            "--version",
            "ls",
            "create",
            "inspect",
            "secret set",
            "apply",
            "run"
        ]
    );
    let calls = fx.calls();
    assert_eq!(
        calls[2],
        format!(
            "--no-input create --name {name} --kit {} claude {}",
            kit.dir.display(),
            fx.workspace().display()
        )
    );
    assert_eq!(
        calls[4],
        format!("secret set anthropic --sandbox {name} --force")
    );
    assert_eq!(
        calls[6],
        format!("run --name {name} -e AWMAN_LAUNCH_ID -e PLAIN -e TERM -- --print hello")
    );
    // The secret travelled on stdin only, never in argv or any message.
    assert_eq!(
        std::fs::read_to_string(fx.state.join("secret.in")).unwrap(),
        format!("{SECRET}\n")
    );
    assert!(calls.iter().all(|c| !c.contains(SECRET)));
    let text = messages.text();
    assert!(!text.contains(SECRET), "{text}");
    assert!(text.contains("(value piped via stdin)"), "{text}");
    assert!(!text.to_lowercase().contains("authenticated"), "{text}");
    // The apply document carries this launch's policy, and the agent
    // session received the same launch id; no credential reached it.
    let applies = fx.files_with_prefix("apply.");
    assert_eq!(applies.len(), 1);
    assert!(applies[0].starts_with("AWMAN-APPLY 2\n"));
    assert!(applies[0].contains("agent claude\n"));
    assert!(applies[0].contains(&format!("kit {}\n", kit.digest)));
    assert!(!applies[0].contains(SECRET));
    let run_env = fx.files_with_prefix("run.");
    assert_eq!(
        run_env[0],
        format!(
            "launch={}\nplain=plain-value\n",
            apply_doc_launch_id(&applies[0])
        )
    );
    // Identity recorded; nothing written into the workspace.
    let record = identity::load(&fx.data_home(), &name).unwrap();
    assert_eq!(record.state, identity::RecordState::Created);
    assert_eq!(record.kit_digest, kit.digest);
    assert_eq!(record.workload.as_deref(), Some("claude"));
    assert_eq!(record.sbx_cli, "0.54.1");
    assert!(!fx.workspace().join(".awman").exists());
    fx.finish();
}

#[test]
fn reuse_refuses_every_inspect_state_before_credentials_apply_or_run() {
    let fx = Fixture::new("engine::sandbox::dsbx::backend::launch_tests::reuse_refuses_every_inspect_state_before_credentials_apply_or_run");
    fx.kit("claude", "v1");
    let rt = runtime();
    let (first, m) = launch(&rt, fx.options("claude", vec![]));
    assert_eq!(first.unwrap(), 0, "{}", m.text());
    for state in [
        r#"{"status":"running"}"#,
        r#"{"status":"stopped"}"#,
        r#"{"sessions":[]}"#,
        r#"{"sessions":[{"id":"survivor"}]}"#,
        r#"{"sessions":{"current":"active"}}"#,
        "malformed",
    ] {
        fx.set("inspect.json", state);
        fx.reset_calls();
        let (result, _) = launch(&rt, fx.options("claude", vec![]));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("session exclusion"));
        assert_eq!(fx.verbs(), ["--version", "ls"]);
    }
    assert_eq!(fx.files_with_prefix("apply.").len(), 1);
    fx.finish();
}

#[test]
fn a_pre_v3_sandbox_with_the_same_name_is_refused_untouched_and_replaced_only_by_the_user() {
    let fx = Fixture::new("engine::sandbox::dsbx::backend::launch_tests::a_pre_v3_sandbox_with_the_same_name_is_refused_untouched_and_replaced_only_by_the_user");
    fx.kit("claude", "v1");
    let rt = runtime();
    let name = fx.name("claude");
    // Same deterministic name, created by an older awman: no record.
    fx.set("ls.json", &format!(r#"[{{"name":"{name}"}}]"#));
    let (result, _) = launch(&rt, fx.options("claude", vec![]));
    let error = result.unwrap_err();
    assert!(
        matches!(error, EngineError::SbxSandboxStale { .. }),
        "{error:?}"
    );
    let text = error.to_string();
    assert!(text.contains("not created by this awman"), "{text}");
    assert!(text.contains(&format!("sbx rm {name}")), "{text}");
    assert!(text.contains("Save anything you need"), "{text}");
    // Nothing was created, applied, run, stopped or removed.
    assert_eq!(fx.verbs(), ["--version", "ls"]);
    assert!(identity::load(&fx.data_home(), &name).is_none());

    // The user removed it; the next launch creates a new one, same name.
    fx.set("ls.json", "[]");
    fx.reset_calls();
    let (result, m) = launch(&rt, fx.options("claude", vec![]));
    assert_eq!(result.unwrap(), 0, "{}", m.text());
    assert!(fx.verbs().contains(&"create".to_string()));
    fx.finish();
}

#[test]
fn a_changed_kit_makes_the_existing_sandbox_stale_on_host_and_in_guest() {
    let fx = Fixture::new("engine::sandbox::dsbx::backend::launch_tests::a_changed_kit_makes_the_existing_sandbox_stale_on_host_and_in_guest");
    fx.kit("claude", "v1");
    let rt = runtime();
    let (first, m) = launch(&rt, fx.options("claude", vec![]));
    assert_eq!(first.unwrap(), 0, "{}", m.text());
    // Host record check: the kit was re-emitted with different content.
    fx.kit("claude", "v2");
    fx.reset_calls();
    let (result, _) = launch(&rt, fx.options("claude", vec![]));
    let text = result.unwrap_err().to_string();
    assert!(text.contains("kit contents"), "{text}");
    assert_eq!(fx.verbs(), ["--version", "ls"]);
    // Guest check: the record matches but the image has another kit-id.
    fx.kit("claude", "v1");
    fx.set("ls.json", "[]");
    fx.set("apply.mode", "stale");
    fx.reset_calls();
    let (result, _) = launch(&rt, fx.options("claude", vec![]));
    let text = result.unwrap_err().to_string();
    assert!(text.contains("baked kit oldkit"), "{text}");
    assert!(!fx.verbs().contains(&"run".to_string()));
    // A sandbox with no awman v3 kit at all.
    fx.set("ls.json", "[]");
    fx.set("apply.mode", "missing");
    fx.reset_calls();
    let (result, _) = launch(&rt, fx.options("claude", vec![]));
    assert!(matches!(result, Err(EngineError::SbxSandboxStale { .. })));
    assert!(!fx.verbs().contains(&"run".to_string()));
    fx.finish();
}

#[test]
fn a_failed_or_unacknowledged_apply_never_starts_the_agent() {
    let fx = Fixture::new("engine::sandbox::dsbx::backend::launch_tests::a_failed_or_unacknowledged_apply_never_starts_the_agent");
    fx.kit("claude", "v1");
    let rt = runtime();
    for (mode, needle) in [
        ("helper", "awman-apply: base64 is missing"),
        ("noack", "without acknowledging this launch"),
    ] {
        fx.set("ls.json", "[]");
        fx.set("apply.mode", mode);
        fx.reset_calls();
        let (result, _) = launch(&rt, fx.options("claude", vec![]));
        let error = result.unwrap_err();
        assert!(
            matches!(error, EngineError::SbxPolicyApplyFailed { .. }),
            "{mode}: {error:?}"
        );
        let text = error.to_string();
        assert!(text.contains(needle), "{mode}: {text}");
        assert!(text.contains("The agent was not started"), "{text}");
        assert_eq!(
            fx.verbs().last().map(String::as_str),
            Some("apply"),
            "{mode}"
        );
    }
    fx.finish();
}

#[test]
fn incompatible_or_missing_cli_is_refused_before_any_other_call() {
    let fx = Fixture::new("engine::sandbox::dsbx::backend::launch_tests::incompatible_or_missing_cli_is_refused_before_any_other_call");
    fx.kit("claude", "v1");
    let rt = runtime();
    fx.set("version", "sbx version v0.52.3\n");
    let (result, _) = launch(&rt, fx.options("claude", vec![]));
    let error = result.unwrap_err();
    assert!(
        matches!(error, EngineError::SbxCliIncompatible { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("0.52.3"));
    assert_eq!(fx.verbs(), ["--version"]);
    fx.set("version", "Usage: sbx [command]\n");
    fx.reset_calls();
    let (result, _) = launch(&rt, fx.options("claude", vec![]));
    assert!(matches!(
        result,
        Err(EngineError::SbxCliVersionUnreadable { .. })
    ));
    assert_eq!(fx.verbs(), ["--version"]);

    // No `sbx` on PATH: the launch fails; nothing else is tried.
    let mut options = fx.options("claude", vec![]);
    let mut env: std::collections::BTreeMap<String, String> = fx
        .sandbox
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    env.insert("FAKE_SBX_STATE".into(), fx.state.display().to_string());
    options.host_env = Some(crate::data::config::env::EnvSnapshot::from_full_env(env));
    fx.reset_calls();
    let (result, _) = launch(&rt, options);
    let text = result.unwrap_err().to_string();
    assert!(text.contains("not found"), "{text}");
    assert!(fx.calls().is_empty());
    fx.finish();
}

#[test]
fn concurrent_launches_into_one_sandbox_cannot_consume_each_others_policy() {
    let fx = Fixture::new("engine::sandbox::dsbx::backend::launch_tests::concurrent_launches_into_one_sandbox_cannot_consume_each_others_policy");
    fx.kit("claude", "v1");
    let rt = runtime();
    let messages = Messages::default();
    let frontend = |m: &Messages| TestFrontend {
        messages: m.clone(),
        stdout: None,
        stderr: None,
    };
    // The first execution holds the launch lock until it is waited/dropped.
    let first = rt
        .block_on(async {
            run_with_kit_loader(
                fx.options("claude", vec![]),
                Box::new(frontend(&messages)),
                &prepared_kit,
            )
        })
        .unwrap();
    // The first agent's `sbx run` is spawned asynchronously; wait for its
    // call record so it cannot land in the second launch's log.
    awman_test_support::wait_for(
        "the first launch's sbx run",
        std::time::Duration::from_secs(10),
        || fx.verbs().iter().any(|verb| verb == "run").then_some(()),
    );
    fx.reset_calls();
    let second = rt.block_on(async {
        run_with_kit_loader(
            fx.options("claude", vec![]),
            Box::new(frontend(&messages)),
            &prepared_kit,
        )
    });
    assert!(matches!(second, Err(EngineError::SbxSandboxBusy { .. })));
    // Refused before it looked at, applied to or ran in the sandbox.
    assert_eq!(fx.verbs(), ["--version"]);
    // Another agent's sandbox in the same workspace is independent.
    fx.kit("codex", "v1");
    let codex = launch(
        &rt,
        fx.options(
            "codex",
            vec![SandboxOption::EnvPassthrough(EnvVar("PLAIN".into()))],
        ),
    );
    assert_eq!(codex.0.unwrap(), 0, "{}", codex.1.text());
    let mut first = first;
    rt.block_on(async { first.wait().await }).unwrap();
    drop(first);
    let (third, m) = launch(&rt, fx.options("claude", vec![]));
    assert_eq!(third.unwrap(), 0, "{}", m.text());
    fx.finish();
}

#[test]
fn owned_workloads_are_created_from_their_kit_directory_with_the_approval_warning() {
    let fx = Fixture::new("engine::sandbox::dsbx::backend::launch_tests::owned_workloads_are_created_from_their_kit_directory_with_the_approval_warning");
    let kit = fx.kit("opencode", "v1");
    let rt = runtime();
    let name = fx.name("opencode");
    let (result, messages) = launch(&rt, fx.options("opencode", vec![]));
    assert_eq!(result.unwrap(), 0, "{}", messages.text());
    let calls = fx.calls();
    let create = calls.iter().find(|c| c.contains(" create ")).unwrap();
    assert_eq!(
        *create,
        format!(
            "--no-input create --name {name} {} {}",
            kit.dir.display(),
            fx.workspace().display()
        )
    );
    assert!(!create.contains("--kit"));
    let text = messages.text();
    assert!(
        text.contains("'awman-opencode' kit may use the 'anthropic' credential"),
        "{text}"
    );
    // The agent major probe runs (advisory here) and the tail is the
    // definition's headless form after the gated entrypoint.
    assert!(calls.contains(&format!("--no-input exec {name} opencode --version")));
    assert!(calls.last().unwrap().ends_with("-- run hello"));
    let record = identity::load(&fx.data_home(), &name).unwrap();
    assert_eq!(record.disposition, identity::DISPOSITION_OWNED);
    assert_eq!(record.workload, None);
    fx.finish();
}

#[test]
fn changed_workspace_is_refused_after_resolution_and_during_cli_preflight() {
    for during_preflight in [false, true] {
        let fx = Fixture::new("engine::sandbox::dsbx::backend::launch_tests::changed_workspace_is_refused_after_resolution_and_during_cli_preflight");
        fx.kit("claude", "v1");
        let options = fx.options("claude", vec![]);
        let workspace = fx.workspace();
        let original = workspace.with_extension("original");
        if during_preflight {
            fx.set("swap-workspace", workspace.to_str().unwrap());
        } else {
            std::fs::rename(&workspace, &original).unwrap();
            std::os::unix::fs::symlink(&fx.state, &workspace).unwrap();
        }
        let (result, _) = launch(&runtime(), options);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("workspace refused"));
        assert!(!fx
            .verbs()
            .iter()
            .any(|v| ["create", "secret set", "apply", "run"].contains(&v.as_str())));
        std::fs::remove_file(&workspace).unwrap();
        std::fs::rename(original, workspace).unwrap();
        fx.finish();
    }
}

#[test]
fn credential_normalization_and_echoed_failures_never_publish_tokens() {
    for value in [
        "synthetic-provider-key",
        "synthetic-provider-key\n",
        "synthetic-provider-key\r\n",
        "synthetic-provider-key\nsecond-line",
        "synthetic-provider-key\rsecond-line",
    ] {
        let fx = Fixture::new("engine::sandbox::dsbx::backend::launch_tests::credential_normalization_and_echoed_failures_never_publish_tokens");
        fx.kit("claude", "v1");
        fx.set("echo-secret", "yes");
        let mut options = fx.options("claude", vec![]);
        options.env_passthrough.clear();
        options.agent_credentials = vec![("ANTHROPIC_API_KEY".into(), value.into())];
        let (result, messages) = launch(&runtime(), options);
        let error = result.unwrap_err().to_string();
        assert!(!error.contains("synthetic-provider-key"), "{error}");
        assert!(!messages.text().contains("synthetic-provider-key"));
        if value.trim().contains(['\r', '\n']) {
            assert!(!fx.verbs().contains(&"secret set".into()));
        } else {
            assert_eq!(
                std::fs::read_to_string(fx.state.join("secret.last")).unwrap(),
                "synthetic-provider-key\n"
            );
            assert!(messages.text().contains("***"));
        }
        fx.finish();
    }
}

#[test]
fn apply_deadline_includes_descendant_pipes_after_the_cli_exits() {
    use std::time::{Duration, Instant};
    for mode in ["hang", "pipehold"] {
        let fx = Fixture::new("engine::sandbox::dsbx::backend::launch_tests::apply_deadline_includes_descendant_pipes_after_the_cli_exits");
        fx.set("apply.mode", mode);
        let start = Instant::now();
        let error = SbxCommand::new(cli::apply_argv("test"))
            .in_env(&fx.snapshot)
            .with_stdin(b"input".to_vec())
            .with_deadline(Duration::from_millis(40))
            .run_quiet()
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("remote apply completion is unknown"));
        assert!(start.elapsed() < Duration::from_secs(1));
        fx.finish();
    }
}
