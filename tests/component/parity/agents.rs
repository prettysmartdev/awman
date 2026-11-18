//! WI 0157: one agent identity selects one version-specific definition through
//! image naming, `ready`, the in-image version probe and launch, identically
//! on the builtin and daemon backends; and a retired built-in `gemini`
//! selection is refused before any build or launch on every entry point.
//!
//! Everything runs through the production `awman` binary against a fake
//! `docker`. `AWMAN_TEST_DOCKER=1` admits the in-image version probe, which
//! is answered by the fake (`docker run --rm --network none … --version`),
//! so no real image or agent ever runs. Host agent binaries on `PATH` are
//! fakes that must never be invoked.
use crate::support::{
    self,
    backend::{Backend, BackendKind},
};
use awman_test_support::{
    fakes::FakeCli,
    fixtures::{ConfigTree, RepoBuilder},
    EnvMap, Exit, TestSandbox,
};
use serde_json::{json, Value};
use std::path::Path;

/// The retired-Gemini note's stable opening (full text: design 6.1).
const GEMINI_NOTE: &str =
    "'gemini' is no longer a built-in agent, and awman has not changed your selection.";

/// (agent, image suffix, probe binary, pinned version, a version that is the
/// wrong major or shape for this agent).
const PINNED: [(&str, &str, &str, &str); 3] = [
    ("opencode", "opencode", "1.19.6", "2.3.2"),
    ("opencode-v2", "opencode", "2.3.2", "1.19.6"),
    ("cursor", "cursor-agent", "2026.11.17-f532c45", ""),
];

struct World {
    docker: FakeCli,
    host_agents: Vec<FakeCli>,
}

fn world(s: &TestSandbox) -> World {
    ConfigTree::new()
        .global(
            json!({"runtime":{"kind":"docker"},"agents":{"auth":{"refresh":{"enabled":false}}}}),
        )
        .write(s, None);
    let docker = s.install_fake(support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    // Every project and agent image is present; its id is stable so the
    // version probe cache has a key. Launches exit successfully.
    docker.on(&["image", "inspect"]).exit(0);
    docker
        .on(&["image", "inspect", "--format", "{{.Id}}"])
        .stdout("sha256:fixture\n");
    docker.on(&["run"]).stdout("agent finished\n");
    docker.on(&["build"]).exit(0);
    let host_agents = ["opencode", "cursor-agent", "gemini", "claude", "codex"]
        .into_iter()
        .map(|name| s.install_fake(support::fake_cli_exe(), name))
        .collect();
    World {
        docker,
        host_agents,
    }
}

fn env(backend: &Backend, s: &TestSandbox) -> EnvMap {
    let mut env = backend.env();
    for key in ["TMPDIR", "TMP", "TEMP"] {
        env.set(key, s.root());
    }
    env.set("AWMAN_TEST_DOCKER", "1");
    env
}

/// Answer the in-image version probe with `version` from now on.
fn probe_answers(docker: &FakeCli, version: &str) {
    let rule = docker.on(&["run", "--rm", "--network", "none"]);
    if version.is_empty() {
        rule.exit(0);
    } else {
        rule.stdout(&format!("{version}\n"));
    }
}

fn run(s: &TestSandbox, env: &EnvMap, cwd: &Path, args: &[&str]) -> (Exit, String) {
    let out = support::awman(s)
        .envs(env)
        .args(args.iter().copied())
        .current_dir(cwd)
        .run();
    (
        out.exit,
        format!("{}{}", out.stdout_text(), out.stderr_text()),
    )
}

fn mark(docker: &FakeCli) -> usize {
    docker.calls().len()
}

fn calls_since(docker: &FakeCli, mark: usize) -> Vec<Vec<String>> {
    docker
        .calls()
        .into_iter()
        .skip(mark)
        .map(|call| call.argv)
        .collect()
}

fn is_probe(argv: &[String]) -> bool {
    argv.starts_with(&[
        "run".into(),
        "--rm".into(),
        "--network".into(),
        "none".into(),
    ])
}

/// Agent launches (`docker run` that is not the version probe).
fn launches(calls: &[Vec<String>]) -> Vec<&Vec<String>> {
    calls
        .iter()
        .filter(|argv| argv.first().is_some_and(|a| a == "run") && !is_probe(argv))
        .collect()
}

fn no_build_or_launch(calls: &[Vec<String>]) -> bool {
    !calls.iter().any(|argv| {
        matches!(
            argv.first().map(String::as_str),
            Some("run" | "build" | "pull" | "create" | "buildx")
        )
    })
}

/// The stable, session-independent shape of one agent launch: the image, the
/// container-side storage mounts, and the argv after the image.
fn launch_shape(argv: &[String]) -> Value {
    let image = argv
        .iter()
        .position(|a| a.starts_with("awman-") && a.ends_with(":latest"))
        .expect("launch names an awman agent image");
    let mounts: Vec<&str> = argv[..image]
        .windows(2)
        .filter(|w| w[0] == "-v")
        .filter_map(|w| w[1].rsplit_once(':').map(|(_, target)| target))
        .filter(|target| *target != "/workspace")
        .collect();
    json!({
        "image": argv[image],
        "argv": argv[image + 1..],
        "mounts": mounts,
    })
}

fn image_for(repo: &Path, agent: &str) -> String {
    awman::data::image_tags::agent_image_tag(repo, agent)
}

/// One agent per test keeps independent subprocess-heavy scenarios within the
/// component deadline; the returned trace is compared across backends.
pub async fn pinned_agent_selects_its_own_definition(
    kind: BackendKind,
    id: &'static str,
    agent: &str,
    // `false` skips the ready and version-mismatch checks (each backend's own
    // test runs them) so the differential run fits the component budget.
    full: bool,
) -> Value {
    let s = support::sandbox(id);
    let w = world(&s);
    let backend = Backend::start(kind, &s).await;
    let env = env(&backend, &s);
    let repo = RepoBuilder::new(&s, "agents")
        .file("README.md", "fixture\n")
        .file("Dockerfile.dev", "FROM scratch\n")
        .init();
    let repo = repo.path().to_path_buf();
    let (_, binary, pinned, wrong) = PINNED
        .into_iter()
        .find(|(name, ..)| *name == agent)
        .expect("known pinned agent");
    let tag = image_for(&repo, agent);
    assert!(tag.ends_with(&format!("-{agent}:latest")), "{tag}");

    if full {
        // ready: the probe runs the pinned binary inside this agent's image.
        probe_answers(&w.docker, pinned);
        let m = mark(&w.docker);
        let (exit, text) = run(&s, &env, &repo, &["ready", "--agent", agent, "--json"]);
        assert_eq!(exit, Exit::Code(0), "{agent} ready: {text}");
        assert!(text.contains("verified inside"), "{agent} ready: {text}");
        let calls = calls_since(&w.docker, m);
        let probes: Vec<_> = calls.iter().filter(|a| is_probe(a)).collect();
        assert!(
            !probes.is_empty(),
            "{agent}: ready did not probe: {calls:#?}"
        );
        for probe in &probes {
            assert!(probe.contains(&tag), "{agent}: probe ran on {probe:?}");
            assert!(probe.iter().any(|arg| arg == binary), "{probe:?}");
            if agent == "cursor" {
                assert_eq!(
                    probe.last().map(String::as_str),
                    Some("--disable-auto-update")
                );
            }
        }

        // A wrong major in the image fails ready and refuses launch.
        probe_answers(&w.docker, wrong);
        let (exit, text) = run(&s, &env, &repo, &["ready", "--agent", agent, "--json"]);
        assert_ne!(exit, Exit::Code(0), "{agent} ready with {wrong:?}: {text}");
        assert!(text.contains("ready --build"), "{agent}: {text}");
        let m = mark(&w.docker);
        let (exit, text) = run(
            &s,
            &env,
            &repo,
            &["exec", "prompt", "--agent", agent, "--headless", "hello"],
        );
        assert_ne!(
            exit,
            Exit::Code(0),
            "{agent} launched with {wrong:?}: {text}"
        );
        assert!(
            launches(&calls_since(&w.docker, m)).is_empty(),
            "{agent}: launched despite a version mismatch"
        );
    }

    // Launch: headless, then headless --yolo, with the pinned binary.
    probe_answers(&w.docker, pinned);
    let mut shapes = Vec::new();
    for extra in [&[][..], &["--yolo"][..]] {
        let m = mark(&w.docker);
        let mut args = vec!["exec", "prompt", "--agent", agent, "--headless"];
        args.extend_from_slice(extra);
        args.push("hello");
        let (exit, text) = run(&s, &env, &repo, &args);
        assert_eq!(exit, Exit::Code(0), "{agent} {extra:?}: {text}");
        let calls = calls_since(&w.docker, m);
        let launched = launches(&calls);
        assert_eq!(launched.len(), 1, "{agent}: {calls:#?}");
        let shape = launch_shape(launched[0]);
        assert_eq!(shape["image"], tag.as_str());
        let yolo = !extra.is_empty();
        let expected: Vec<&str> = match (agent, yolo) {
            // 1.x: unchanged; --yolo has never mapped to a flag.
            ("opencode", _) => vec!["opencode", "run", "hello"],
            // 2.x: always standalone; the seed arrives on stdin.
            ("opencode-v2", false) => vec!["opencode", "--standalone", "run"],
            ("opencode-v2", true) => vec!["opencode", "--standalone", "run", "--auto"],
            ("cursor", false) => vec![
                "cursor-agent",
                "--disable-auto-update",
                "--print",
                "--trust",
                "--output-format",
                "text",
                "hello",
            ],
            ("cursor", true) => vec![
                "cursor-agent",
                "--disable-auto-update",
                "--print",
                "--trust",
                "--output-format",
                "text",
                "--force",
                "hello",
            ],
            _ => unreachable!(),
        };
        assert_eq!(shape["argv"], json!(expected), "{agent} yolo={yolo}");
        let notice = text.contains("private copy of ~/.config/opencode");
        assert_eq!(
            notice,
            agent == "opencode-v2",
            "{agent}: launch note: {text}"
        );
        shapes.push(json!({"flags": extra, "launch": shape, "launch_note": notice}));
    }

    // Modes the agent cannot honour are refused before any launch.
    let mut refused = Vec::new();
    for mode in ["--plan", "--auto"] {
        let m = mark(&w.docker);
        let (exit, text) = run(
            &s,
            &env,
            &repo,
            &[
                "exec",
                "prompt",
                "--agent",
                agent,
                "--headless",
                mode,
                "hello",
            ],
        );
        let calls = calls_since(&w.docker, m);
        // Only OpenCode 1.x keeps an `--auto` mapping; nothing maps --plan.
        let expect_refused = !(agent == "opencode" && mode == "--auto");
        assert_eq!(
            exit != Exit::Code(0),
            expect_refused,
            "{agent} {mode}: {text}"
        );
        if expect_refused {
            assert!(launches(&calls).is_empty(), "{agent} {mode}: {calls:#?}");
        }
        refused.push(json!({"mode": mode, "refused": exit != Exit::Code(0)}));
    }
    let trace = json!({
        "agent": agent,
        "image_suffix": tag.rsplit_once('-').map(|(_, s)| s).unwrap_or_default(),
        "probe_binary": binary,
        "launches": shapes,
        "modes": refused,
    });
    for agent in &w.host_agents {
        assert!(
            agent.calls().is_empty(),
            "host agent invoked: {:?}",
            agent.calls()
        );
    }
    backend.shutdown().await;
    s.finish();
    // Repo paths differ per sandbox; the image's repo stem is the same.
    trace
}

/// Every entry point that selects `gemini` with no custom definition fails
/// with the migration note before any image build, download or launch.
pub async fn retired_gemini_is_refused_before_build_or_launch(
    kind: BackendKind,
    id: &'static str,
) -> Value {
    let s = support::sandbox(id);
    let w = world(&s);
    probe_answers(&w.docker, "");
    let backend = Backend::start(kind, &s).await;
    let env = env(&backend, &s);
    let repo = RepoBuilder::new(&s, "gemini")
        .file("README.md", "fixture\n")
        .file("Dockerfile.dev", "FROM scratch\n")
        .file(
            "file-default.toml",
            "title = 'g'\nagent = 'gemini'\n\n[[setup]]\ntype = 'run_shell'\ncommand = 'touch setup-ran'\n\n[[step]]\nname = 'one'\nprompt = 'look'\n",
        )
        .file(
            "step-agent.toml",
            "title = 'g'\nagent = 'claude'\n\n[[step]]\nname = 'one'\nprompt = 'look'\n\n[[step]]\nname = 'two'\nagent = 'gemini'\ndepends_on = ['one']\nprompt = 'look'\n",
        )
        .init();
    let repo = repo.path().to_path_buf();
    let repo_default = RepoBuilder::new(&s, "gemini-default")
        .file("README.md", "fixture\n")
        .file("Dockerfile.dev", "FROM scratch\n")
        .file(
            ".awman/config.json",
            "{\"agents\":{\"default\":\"gemini\"}}\n",
        )
        .init();
    let repo_default = repo_default.path().to_path_buf();

    let mut cases: Vec<(&str, &Path, Vec<&str>)> = vec![
        (
            "exec prompt --agent",
            &repo,
            vec!["exec", "prompt", "--agent", "gemini", "--headless", "hi"],
        ),
        ("chat --agent", &repo, vec!["chat", "--agent", "gemini"]),
        (
            "repo default exec prompt",
            &repo_default,
            vec!["exec", "prompt", "--headless", "hi"],
        ),
        ("repo default chat", &repo_default, vec!["chat"]),
        (
            "workflow file default",
            &repo,
            vec!["exec", "workflow", "file-default.toml", "--headless"],
        ),
        (
            "workflow step agent",
            &repo,
            vec!["exec", "workflow", "step-agent.toml", "--headless"],
        ),
        (
            "workflow --agent",
            &repo,
            vec![
                "exec",
                "workflow",
                "step-agent.toml",
                "--agent",
                "gemini",
                "--headless",
            ],
        ),
        (
            "ready --agent",
            &repo,
            vec!["ready", "--agent", "gemini", "--json"],
        ),
        ("repo default ready", &repo_default, vec!["ready", "--json"]),
        ("init --agent", &repo, vec!["init", "--agent", "gemini"]),
    ];
    let mut trace = Vec::new();
    for (label, cwd, args) in cases.drain(..) {
        let m = mark(&w.docker);
        let (exit, text) = run(&s, &env, cwd, &args);
        let calls = calls_since(&w.docker, m);
        assert_ne!(exit, Exit::Code(0), "{label}: accepted gemini: {text}");
        assert!(
            text.contains(GEMINI_NOTE),
            "{label}: no migration note: {text}"
        );
        assert!(
            no_build_or_launch(&calls),
            "{label}: built or launched: {calls:#?}"
        );
        assert!(
            !cwd.join(".awman/Dockerfile.gemini").exists(),
            "{label}: provisioned Dockerfile.gemini"
        );
        trace.push(json!({"case": label, "exit": format!("{exit:?}")}));
    }
    assert!(
        !repo.join("setup-ran").exists(),
        "a workflow setup step ran"
    );

    // Global default: the global config names gemini; the repo has no default.
    let global = s.awman_home().join("config.json");
    let mut doc: Value = serde_json::from_slice(&std::fs::read(&global).unwrap()).unwrap();
    awman::data::config::config_json::set_config_field(&mut doc, "agents.default", json!("gemini"));
    std::fs::write(&global, serde_json::to_vec(&doc).unwrap()).unwrap();
    let m = mark(&w.docker);
    let (exit, text) = run(&s, &env, &repo, &["exec", "prompt", "--headless", "hi"]);
    assert_ne!(
        exit,
        Exit::Code(0),
        "global default accepted gemini: {text}"
    );
    assert!(text.contains(GEMINI_NOTE), "global default: {text}");
    assert!(no_build_or_launch(&calls_since(&w.docker, m)));
    // Nothing rewrote the selection.
    let doc: Value = serde_json::from_slice(&std::fs::read(&global).unwrap()).unwrap();
    assert_eq!(doc.pointer("/agents/default"), Some(&json!("gemini")));
    assert_eq!(
        std::fs::read_to_string(repo_default.join(".awman/config.json")).unwrap(),
        "{\"agents\":{\"default\":\"gemini\"}}\n"
    );
    trace.push(json!({"case": "global default exec prompt", "exit": format!("{exit:?}")}));

    // `new agent gemini` is no longer a reserved name (it proceeds to the
    // brief check); the new first-party names are reserved.
    let (exit, text) = run(&s, &env, &repo, &["new", "agent", "gemini", "--headless"]);
    assert_ne!(exit, Exit::Code(0));
    assert!(
        !text.contains("first-party agent"),
        "gemini still reserved: {text}"
    );
    for reserved in ["opencode-v2", "cursor"] {
        let (exit, text) = run(&s, &env, &repo, &["new", "agent", reserved, "--headless"]);
        assert_ne!(exit, Exit::Code(0));
        assert!(text.contains("first-party agent"), "{reserved}: {text}");
    }
    let (exit, _) = run(
        &s,
        &env,
        &repo,
        &["new", "agent", "gemini", "--from", "gemini", "--headless"],
    );
    assert_ne!(exit, Exit::Code(0), "a shipped gemini example exists");
    trace.push(json!({"case": "new agent", "gemini_reserved": false}));

    for agent in &w.host_agents {
        assert!(
            agent.calls().is_empty(),
            "host agent invoked: {:?}",
            agent.calls()
        );
    }
    backend.shutdown().await;
    s.finish();
    json!(trace)
}

pub async fn cursor_env_output_is_redacted(kind: BackendKind, id: &'static str) {
    let s = support::sandbox(id);
    let w = world(&s);
    let backend = Backend::start(kind, &s).await;
    let mut env = env(&backend, &s);
    const KEY: &str = "synthetic-cursor-output-canary";
    env.set("CURSOR_API_KEY", KEY);
    let repo = RepoBuilder::new(&s, "redaction")
        .file("Dockerfile.dev", "FROM scratch\n")
        .init();
    for code in [0, 1] {
        w.docker
            .on(&["run"])
            .stdout(&format!("diagnostic {KEY}\n"))
            .stderr(&format!("failure {KEY}\n"))
            .exit(code);
        probe_answers(&w.docker, "2026.11.17-f532c45");
        let m = mark(&w.docker);
        let (exit, text) = run(
            &s,
            &env,
            repo.path(),
            &[
                "exec",
                "prompt",
                "--agent",
                "cursor",
                "--headless",
                "--overlay",
                "env(CURSOR_API_KEY)",
                "hello",
            ],
        );
        assert_eq!(exit == Exit::Code(0), code == 0, "{text}");
        assert!(!text.contains(KEY), "{text}");
        assert!(text.contains("diagnostic ***"), "{text}");
        let calls = calls_since(&w.docker, m);
        assert!(!calls.iter().flatten().any(|v| v.contains(KEY)));
        assert_eq!(launches(&calls).len(), 1);
    }
    backend.shutdown().await;
    s.finish();
}

pub async fn pinned_readiness_contract(kind: BackendKind, id: &'static str) -> Value {
    let s = support::sandbox(id);
    let w = world(&s);
    let backend = Backend::start(kind, &s).await;
    let env = env(&backend, &s);
    let repo = RepoBuilder::new(&s, "ready-contract")
        .file("Dockerfile.dev", "FROM scratch\n")
        .init();
    let mut trace = Vec::new();
    for (agent, binary, pin, wrong) in PINNED {
        for (version, expected_ready) in [(pin, true), (wrong, false)] {
            probe_answers(&w.docker, version);
            let m = mark(&w.docker);
            let (exit, text) = run(&s, &env, repo.path(), &["ready", "--agent", agent]);
            assert_eq!(exit == Exit::Code(0), expected_ready, "{text}");
            assert!(text.contains("host check skipped"), "{text}");
            let tag = image_for(repo.path(), agent);
            let calls = calls_since(&w.docker, m);
            let probes: Vec<_> = calls.iter().filter(|c| is_probe(c)).collect();
            assert!(!probes.is_empty());
            assert!(probes
                .iter()
                .all(|c| c.contains(&tag) && c.iter().any(|a| a == binary)));
            trace.push(json!({"agent": agent, "binary":binary, "version":version, "ready":expected_ready, "host_ping":false}));
        }
    }
    for agent in &w.host_agents {
        assert!(agent.calls().is_empty());
    }
    let docs = include_str!("../../../docs/03-agent-sessions.md");
    assert!(docs.contains("skips the former host authentication ping"));
    assert!(docs.contains("existing mixin sandboxes need"));
    backend.shutdown().await;
    s.finish();
    json!(trace)
}
