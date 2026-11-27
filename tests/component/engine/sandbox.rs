//! Docker Sandboxes (`sbx`) runtime through the production `awman ready`,
//! `awman exec prompt` and `awman chat` entry points of the debug binary,
//! with a fake `sbx` (FakeCli) and fakes under every other runtime and agent
//! name that must never run.
//!
//! The runtime is enabled on Apple silicon macOS and Linux x86_64.
//! On every other host these tests assert the
//! production refusal: an explicit platform error, no `sbx` call, and no
//! fallback to another runtime. On Apple silicon they additionally drive the
//! launch sequence far enough to prove each refusal happens before the step
//! it guards. The complete fresh/reused/stale/apply sequence with a stateful
//! fake is covered through the production launch internals in
//! `engine::sandbox::dsbx::backend::launch_tests`.
//!
//! A FakeCli answer is not live kit support: the real CLI is exercised only
//! by the `boundary-sbx` suite.

use crate::support;
use awman_test_support::{
    fakes::FakeCli,
    fixtures::{ConfigTree, RepoBuilder},
    EnvMap, Exit, TestSandbox,
};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Hosts with accepted native evidence.
const QUALIFIED_HOST: bool = cfg!(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
));

/// A supported sbx version (contract §5.1).
const SBX_VERSION: &str = "sbx version v0.54.1 (component)\n";

struct World {
    s: TestSandbox,
    sbx: FakeCli,
    /// Fakes under every other runtime CLI and host agent name: any call
    /// would be a fallback or a host agent launch.
    forbidden: Vec<FakeCli>,
    repo: PathBuf,
    env: EnvMap,
}

impl World {
    fn new(id: &'static str) -> World {
        let s = support::sandbox(id);
        ConfigTree::new()
            .global(json!({
                "runtime": {"kind": "sbx"},
                "agents": {"auth": {"refresh": {"enabled": false}}}
            }))
            .write(&s, None);
        let repo = RepoBuilder::new(&s, "workspace")
            .file("README.md", "sbx component workspace\n")
            .init()
            .path()
            .to_path_buf();
        let sbx = s.install_fake(support::fake_cli_exe(), "sbx");
        sbx.on(&["--version"]).stdout(SBX_VERSION);
        sbx.on(&["--no-input", "ls", "--json"]).stdout("[]");
        sbx.on(&["--no-input", "kit", "validate"]).exit(0);
        sbx.on(&["--no-input", "inspect"]).stdout("{}");
        sbx.on(&["--no-input", "secret", "ls"]).stdout("[]");
        let forbidden = [
            "docker",
            "container",
            "claude",
            "codex",
            "copilot",
            "opencode",
            "cursor-agent",
        ]
        .into_iter()
        .map(|name| s.install_fake(support::fake_cli_exe(), name))
        .collect();
        let mut env = s.env().clone();
        // The fake lives in the sandbox bin directory; opt in so the
        // isolation resolver lets awman use it.
        env.set("AWMAN_TEST_SBX", "1");
        World {
            s,
            sbx,
            forbidden,
            repo,
            env,
        }
    }

    fn awman(&self, args: &[&str]) -> (Exit, String) {
        self.awman_with(args, &[])
    }

    fn awman_with(&self, args: &[&str], extra_env: &[(&str, &str)]) -> (Exit, String) {
        let mut command = support::awman(&self.s)
            .envs(&self.env)
            .args(args.iter().copied())
            .current_dir(&self.repo);
        for (key, value) in extra_env {
            command = command.env(key, value);
        }
        let out = command.run();
        (
            out.exit,
            format!("{}{}", out.stdout_text(), out.stderr_text()),
        )
    }

    fn sbx_calls(&self) -> Vec<Vec<String>> {
        self.sbx.calls().into_iter().map(|c| c.argv).collect()
    }

    fn sbx_verbs(&self) -> Vec<String> {
        self.sbx_calls()
            .iter()
            .map(|argv| {
                let words: Vec<&str> = argv
                    .iter()
                    .map(String::as_str)
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

    fn kit_dir(&self, handle: &str) -> PathBuf {
        self.s.awman_home().join("kits/v3").join(handle)
    }

    /// No other runtime and no host agent ran, whatever the outcome.
    fn assert_no_fallback(&self, text: &str) {
        for fake in &self.forbidden {
            assert!(
                fake.calls().is_empty(),
                "{} ran (fallback or host agent launch): {:?}\n{text}",
                fake.exe().display(),
                fake.calls()
            );
        }
    }

    fn finish(self) {
        self.s.finish();
    }
}

fn assert_failed(exit: Exit, text: &str) {
    assert!(!exit.success(), "expected a refusal, got success:\n{text}");
}

/// The production refusal on a host that is not a qualification target.
fn assert_platform_refusal(world: &World, exit: Exit, text: &str) {
    assert_failed(exit, text);
    assert!(
        text.contains("docker-sbx-experimental") && text.contains("not supported on platform"),
        "{text}"
    );
    if cfg!(target_os = "linux") {
        assert!(
            text.contains(&format!("linux/{}: not qualified", std::env::consts::ARCH)),
            "{text}"
        );
    }
    assert!(world.sbx_calls().is_empty(), "{:?}", world.sbx_calls());
    world.assert_no_fallback(text);
}

#[test]
fn ready_admits_the_cli_emits_the_v3_kit_and_validates_it_offline_or_refuses_the_platform() {
    let world = World::new(
        "engine::sandbox::ready_admits_the_cli_emits_the_v3_kit_and_validates_it_offline_or_refuses_the_platform",
    );
    let (exit, text) = world.awman(&["ready", "--agent", "claude"]);
    if !QUALIFIED_HOST {
        assert_platform_refusal(&world, exit, &text);
        assert!(!world.kit_dir("awman-claude-mixin").exists());
        return world.finish();
    }
    assert!(exit.success(), "{text}");
    assert_eq!(world.sbx_verbs(), ["--version", "ls", "kit validate"]);
    let validate = world.sbx_calls().pop().unwrap();
    let kit = world.kit_dir("awman-claude-mixin");
    assert_eq!(
        validate,
        [
            "--no-input".to_string(),
            "kit".into(),
            "validate".into(),
            kit.display().to_string(),
            "--offline".into()
        ]
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(kit.join("awman-kit.json")).unwrap()).unwrap();
    assert_eq!(manifest["kind"], "mixin");
    assert_eq!(manifest["handle"], "awman-claude-mixin");
    // ready creates nothing, registers no credential and removes nothing.
    assert!(!text.contains("sbx rm"), "{text}");
    world.assert_no_fallback(&text);

    // The owned workload selects its own kit directory.
    let (exit, text) = world.awman(&["ready", "--agent", "opencode"]);
    assert!(exit.success(), "{text}");
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(world.kit_dir("awman-opencode").join("awman-kit.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["kind"], "workload");
    world.finish();
}

#[test]
fn refused_agents_old_clients_and_signed_out_clients_never_reach_a_sandbox() {
    let world = World::new(
        "engine::sandbox::refused_agents_old_clients_and_signed_out_clients_never_reach_a_sandbox",
    );
    // Agents without a route are refused before any sbx call.
    for agent in ["cursor", "opencode-v2"] {
        let (exit, text) = world.awman(&["ready", "--agent", agent]);
        assert_failed(exit, &text);
        if QUALIFIED_HOST {
            assert!(
                text.contains("per-execution state and credential isolation"),
                "{agent}: {text}"
            );
            assert!(world.sbx_calls().is_empty(), "{agent}");
        } else {
            assert_platform_refusal(&world, exit, &text);
        }
    }
    if !QUALIFIED_HOST {
        return world.finish();
    }
    // A client older than the contract minimum is refused after --version.
    world.sbx.on(&["--version"]).stdout("sbx version v0.52.9\n");
    let (exit, text) = world.awman(&["ready", "--agent", "claude"]);
    assert_failed(exit, &text);
    assert!(text.contains("is not supported by awman"), "{text}");
    assert_eq!(world.sbx_verbs(), ["--version"]);
    // An unreadable listing (signed out) is an error, not a warning.
    world.sbx.on(&["--version"]).stdout(SBX_VERSION);
    world
        .sbx
        .on(&["--no-input", "ls", "--json"])
        .stderr("not signed in\n")
        .exit(1);
    let (exit, text) = world.awman(&["ready", "--agent", "claude"]);
    assert_failed(exit, &text);
    assert!(text.contains("run 'sbx login'"), "{text}");
    assert!(!world.sbx_verbs().iter().any(|v| v == "kit validate"));
    world.assert_no_fallback(&text);
    world.finish();
}

#[test]
fn missing_sbx_cli_is_an_error_and_never_falls_back_to_another_runtime() {
    let world = World::new(
        "engine::sandbox::missing_sbx_cli_is_an_error_and_never_falls_back_to_another_runtime",
    );
    // Without the opt-in the isolation resolver treats sbx as not installed.
    let mut env = world.env.clone();
    env.remove("AWMAN_TEST_SBX");
    let out = support::awman(&world.s)
        .envs(&env)
        .args(["exec", "prompt", "--agent", "claude", "--headless", "hello"])
        .current_dir(&world.repo)
        .run();
    let text = format!("{}{}", out.stdout_text(), out.stderr_text());
    assert_failed(out.exit, &text);
    if QUALIFIED_HOST {
        assert!(text.contains("sbx"), "{text}");
    } else {
        assert!(text.contains("not supported on platform"), "{text}");
    }
    assert!(world.sbx_calls().is_empty());
    world.assert_no_fallback(&text);
    world.finish();
}

#[test]
fn exec_prompt_and_chat_refuse_undeliverable_options_before_any_sbx_call() {
    let world = World::new(
        "engine::sandbox::exec_prompt_and_chat_refuse_undeliverable_options_before_any_sbx_call",
    );
    let cases: [&[&str]; 3] = [
        // Mode on a published workload with no awman-managed settings file.
        &[
            "exec",
            "prompt",
            "--agent",
            "codex",
            "--headless",
            "--yolo",
            "hello",
        ],
        // A prompt that would be parsed as an option in argv.
        &[
            "exec",
            "prompt",
            "--agent",
            "claude",
            "--headless",
            "--",
            "-rf now",
        ],
        &["chat", "--agent", "copilot", "--yolo"],
    ];
    for args in cases {
        let (exit, text) = world.awman(args);
        if !QUALIFIED_HOST {
            assert_platform_refusal(&world, exit, &text);
            continue;
        }
        assert_failed(exit, &text);
        assert!(text.contains("on Docker Sandboxes"), "{args:?}: {text}");
        // Admission of the options precedes even the CLI version check.
        assert!(
            world.sbx_calls().is_empty(),
            "{args:?}: {:?}",
            world.sbx_calls()
        );
        world.assert_no_fallback(&text);
    }
    world.finish();
}

#[test]
fn fresh_launch_without_credentials_or_an_acknowledged_apply_never_starts_the_agent() {
    let world = World::new(
        "engine::sandbox::fresh_launch_without_credentials_or_an_acknowledged_apply_never_starts_the_agent",
    );
    world.sbx.on(&["--no-input", "create"]).stdout("created\n");
    // The apply step answers without the exact acknowledgement for this
    // launch: the policy is not verified, so `sbx run` must never happen.
    world
        .sbx
        .on(&["--no-input", "exec", "-i", "-u", "root"])
        .read_stdin()
        .stdout("AWMAN-APPLY-OK not-this-launch\n");
    world.sbx.on(&["run"]).stdout("agent must not run\n");
    let (exit, text) = world.awman(&["exec", "prompt", "--agent", "claude", "--headless", "hello"]);
    if !QUALIFIED_HOST {
        assert_platform_refusal(&world, exit, &text);
        return world.finish();
    }
    assert_failed(exit, &text);
    assert!(text.contains("The agent was not started"), "{text}");
    let verbs = world.sbx_verbs();
    assert_eq!(&verbs[..3], ["--version", "ls", "create"], "{verbs:?}");
    assert!(verbs.iter().any(|v| v == "apply"), "{verbs:?}");
    assert!(!verbs.iter().any(|v| v == "run"), "{verbs:?}");
    // The published workload is selected by name with the mixin as --kit.
    let create = world
        .sbx_calls()
        .into_iter()
        .find(|argv| argv.iter().any(|a| a == "create"))
        .unwrap();
    let kit = world.kit_dir("awman-claude-mixin").display().to_string();
    let at = create.iter().position(|a| a == "--kit").expect("--kit");
    assert_eq!(create[at + 1], kit);
    assert_eq!(create[create.len() - 2], "claude");
    // No credential was supplied: awman says how to add a sandbox-scoped
    // one and never claims the agent is authenticated.
    assert!(!verbs.iter().any(|v| v == "secret set"), "{verbs:?}");
    assert!(text.contains("--sandbox"), "{text}");
    assert!(!text.to_lowercase().contains("authenticated"), "{text}");
    // The apply document carried policy only, never the prompt.
    let apply = world
        .sbx
        .calls()
        .into_iter()
        .find(|c| c.argv.iter().any(|a| a.ends_with("awman-apply")))
        .unwrap();
    assert!(String::from_utf8_lossy(&apply.stdin).starts_with("AWMAN-APPLY "));
    assert!(!String::from_utf8_lossy(&apply.stdin).contains("hello"));
    world.assert_no_fallback(&text);
    world.finish();
}

#[test]
fn reused_sandbox_without_an_awman_identity_is_stale_and_never_removed() {
    let world = World::new(
        "engine::sandbox::reused_sandbox_without_an_awman_identity_is_stale_and_never_removed",
    );
    if QUALIFIED_HOST {
        // A sandbox with awman's deterministic name exists, but awman has
        // no identity record for it (hand-made or pre-v3).
        let name = deterministic_name(&world.repo, "claude");
        world
            .sbx
            .on(&["--no-input", "ls", "--json"])
            .stdout(&format!(r#"[{{"name":"{name}"}}]"#));
    }
    let (exit, text) = world.awman(&["exec", "prompt", "--agent", "claude", "--headless", "hello"]);
    if !QUALIFIED_HOST {
        assert_platform_refusal(&world, exit, &text);
        return world.finish();
    }
    assert_failed(exit, &text);
    assert!(text.contains("cannot be reused"), "{text}");
    assert!(text.contains("awman will not remove it"), "{text}");
    let verbs = world.sbx_verbs();
    for never in ["create", "apply", "run", "rm", "stop"] {
        assert!(!verbs.iter().any(|v| v == never), "{never}: {verbs:?}");
    }
    // `ready` reports the same sandbox as stale and still removes nothing.
    let (exit, text) = world.awman(&["ready", "--agent", "claude", "--no-cache"]);
    assert!(exit.success(), "{text}");
    assert!(text.contains("will be refused at launch"), "{text}");
    assert!(!world.sbx_verbs().iter().any(|v| v == "rm"));
    world.assert_no_fallback(&text);
    world.finish();
}

/// `awman-<FNV-1a of the canonical workspace path, low 32 bits>-<agent>`
/// (the public naming contract in `src/engine/sandbox/naming.rs`).
fn deterministic_name(workspace: &Path, agent: &str) -> String {
    awman::engine::sandbox::naming::sandbox_name_for(&workspace.canonicalize().unwrap(), agent)
}
