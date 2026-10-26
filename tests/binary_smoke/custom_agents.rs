//! WI0130 binary contracts; all remotes and host state are private fixtures.
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn git(root: &Path, argv: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(argv)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {argv:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn cli(root: &Path, home: &Path) -> Command {
    let mut command = Command::new(crate::awman_bin());
    command
        .current_dir(root)
        .env("HOME", home)
        .env("AWMAN_CONFIG_HOME", home.join(".awman"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("AWMAN_TEST_ISOLATION", "1")
        .stdin(Stdio::null());
    command
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn chat_maki_without_definition_exits_four_with_migration_hint() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "--quiet"]);
    let output = cli(root.path(), home.path())
        .args(["chat", "--agent", "maki", "--non-interactive"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("unknown agent 'maki'"), "{text}");
    assert!(text.contains("awman new agent maki --from maki"), "{text}");
}

#[test]
fn new_agent_shipped_examples_match_the_binary_schema_and_are_approved() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "--quiet"]);
    let skills = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/create-custom-agent");
    for name in [
        "SKILL.md",
        "reference.md",
        "examples/crush.toml",
        "examples/maki.toml",
        "examples/maki.Dockerfile",
    ] {
        assert!(
            skills.join(name).is_file(),
            "missing shipped skill file {name}"
        );
    }
    for name in ["crush", "maki"] {
        let output = cli(root.path(), home.path())
            .args(["new", "agent", name, "--from", name])
            .output()
            .unwrap();
        success(&output);
        let path = root.path().join(format!(".awman/agents/{name}.toml"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            std::fs::read_to_string(skills.join(format!("examples/{name}.toml"))).unwrap()
        );
        awman::data::config::agent_definition::load_agent_definition(&path).unwrap();
        let env = awman::data::config::EnvSnapshot::with_overrides([(
            "AWMAN_CONFIG_HOME",
            home.path().join(".awman").to_str().unwrap(),
        )]);
        let agent = awman::data::session::AgentName::new(name).unwrap();
        awman::engine::agent::AgentRegistry::resolve(&env, &agent, Some(root.path()))
            .expect("new --from approves the exact written recipe");
        assert!(String::from_utf8_lossy(&output.stdout).contains("ON YOUR HOST"));
    }
}

#[test]
fn real_git_new_skill_pull_builtin_checks_out_only_skills_from_local_bare_remote() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let bare = fixture.path().join("awman.git");
    let repo = fixture.path().join("project");
    let home = fixture.path().join("home");
    for dir in [&source, &repo, &home] {
        std::fs::create_dir_all(dir).unwrap();
    }
    git(&source, &["init", "--quiet"]);
    git(&source, &["config", "user.name", "WI0130 tests"]);
    git(&source, &["config", "user.email", "tests@example.invalid"]);
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/create-custom-agent");
    let skill = source.join("skills/create-custom-agent");
    std::fs::create_dir_all(skill.join("examples")).unwrap();
    for name in [
        "SKILL.md",
        "reference.md",
        "examples/crush.toml",
        "examples/maki.toml",
        "examples/maki.Dockerfile",
    ] {
        std::fs::copy(shipped.join(name), skill.join(name)).unwrap();
    }
    std::fs::create_dir_all(source.join("src")).unwrap();
    std::fs::write(source.join("src/not-checked-out.rs"), "production code").unwrap();
    std::fs::write(source.join("README.md"), "root files remain in cone mode\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "--quiet", "-m", "fixture"]);
    git(
        fixture.path(),
        &[
            "clone",
            "--quiet",
            "--bare",
            source.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    git(&repo, &["init", "--quiet"]);
    let output = cli(&repo, &home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "1")
        .env(
            "GIT_CONFIG_KEY_0",
            format!("url.file://{}.insteadOf", bare.display()),
        )
        .env(
            "GIT_CONFIG_VALUE_0",
            "https://github.com/prettysmartdev/awman.git",
        )
        .args(["new", "skill", "--pull", "builtin"])
        .output()
        .unwrap();
    success(&output);
    let library = home.join(".awman/skills/.library/awman");
    assert!(library
        .join("skills/create-custom-agent/SKILL.md")
        .is_file());
    assert!(
        !library.join("src").exists(),
        "sparse checkout must exclude production tree"
    );
    let meta = awman::data::fs::skill_library::read_library_meta(&library).unwrap();
    assert!(meta.sparse);
    assert_eq!(meta.subdir, "skills");
    let sparse = Command::new("git")
        .current_dir(&library)
        .args(["sparse-checkout", "list"])
        .output()
        .unwrap();
    success(&sparse);
    assert_eq!(String::from_utf8(sparse.stdout).unwrap().trim(), "skills");
    for name in ["crush", "maki"] {
        awman::data::config::agent_definition::load_agent_definition(
            &library.join(format!("skills/create-custom-agent/examples/{name}.toml")),
        )
        .unwrap();
    }
}

#[test]
fn no_retired_builtin_templates_or_first_party_entries_remain() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src");
    fn check(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                check(&path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                // Include invocations can span lines; inspect each macro argument.
                for argument in text.split("include_str!").skip(1) {
                    let argument = argument.split(')').next().unwrap();
                    for retired in [
                        "Dockerfile.crush",
                        "Dockerfile.maki",
                        "sbx-kit.crush",
                        "sbx-kit.maki",
                        "sbx-apply.crush",
                        "sbx-apply.maki",
                    ] {
                        assert!(
                            !argument.contains(retired),
                            "{} includes retired {retired}",
                            path.display()
                        );
                    }
                }
            }
        }
    }
    check(&source);
    let first_party = std::fs::read_to_string(source.join("engine/agent/first_party.rs")).unwrap();
    for retired in ["\"crush\"", "\"maki\""] {
        assert!(!first_party.contains(retired));
    }
    let matrix =
        std::fs::read_to_string(source.join("engine/container/builtin/backend/matrix_tests.rs"))
            .unwrap();
    assert!(
        matrix.contains("for agent in FIRST_PARTY_AGENTS"),
        "builtin matrix must keep iterating first-party agents"
    );
}

#[test]
fn sbx_custom_crush_ready_cli_refuses_without_writing_kit() {
    if std::env::var("AWMAN_TEST_SBX").as_deref() != Ok("1")
        || !cfg!(all(target_os = "macos", target_arch = "aarch64"))
    {
        eprintln!("SKIP: requires AWMAN_TEST_SBX=1 on Apple Silicon macOS");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "--quiet"]);
    success(
        &cli(root.path(), home.path())
            .args(["new", "agent", "crush", "--from", "crush"])
            .output()
            .unwrap(),
    );
    std::fs::write(
        root.path().join(".awman/config.json"),
        r#"{"runtime":"docker-sbx-experimental"}"#,
    )
    .unwrap();
    let output = cli(root.path(), home.path())
        .args(["ready", "--agent", "crush", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("custom agent") && text.contains("docker-sbx-experimental"),
        "{text}"
    );
    assert!(!home.path().join(".awman/kits").exists());
}

fn opted_in(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// Remove only the private tags created for this fixture, including on panic.
struct Images {
    program: &'static str,
    tags: Vec<String>,
}
impl Drop for Images {
    fn drop(&mut self) {
        for tag in &self.tags {
            let args = if self.program == "docker" {
                vec!["image", "rm", "-f", tag]
            } else {
                vec!["image", "delete", tag]
            };
            let _ = Command::new(awman::engine::host_cli::program(self.program))
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn cached_base(program: &str) -> Option<String> {
    // Deliberately never pull: the runtime harness must provide a local base.
    let base = std::env::var("AWMAN_TEST_CUSTOM_AGENT_BASE_IMAGE")
        .unwrap_or_else(|_| "debian:bookworm-slim".into());
    let found = Command::new(awman::engine::host_cli::program(program))
        .args(["image", "inspect", &base])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if found {
        Some(base)
    } else {
        eprintln!("BLOCKED: load {base} into {program}, or set AWMAN_TEST_CUSTOM_AGENT_BASE_IMAGE to a cached Debian image with bash and useradd; no network pull is permitted");
        None
    }
}

fn smoke_container_runtime(program: &'static str, runtime: &str) {
    let Some(base) = cached_base(program) else {
        return;
    };
    let root = tempfile::Builder::new()
        .prefix("wi0130-")
        .tempdir()
        .unwrap();
    let home = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "--quiet"]);
    // Use a global no-ping definition: this path requires no trust prompt.
    let global = home.path().join(".awman/agents");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(
        global.join("pi.toml"),
        crate::helpers::custom_agents::RUNTIME_SMOKE,
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join(".awman")).unwrap();
    std::fs::write(
        root.path().join(".awman/config.json"),
        serde_json::to_vec(&serde_json::json!({"agent":"pi", "auth":"none", "runtime":runtime}))
            .unwrap(),
    )
    .unwrap();
    std::fs::write(root.path().join("Dockerfile.dev"), format!("FROM {base}\n")).unwrap();
    let _images = Images {
        program,
        tags: vec![
            awman::data::image_tags::agent_image_tag(root.path(), "pi"),
            awman::data::image_tags::project_image_tag(root.path()),
        ],
    };
    let ready = cli(root.path(), home.path())
        .args(["ready", "--agent", "pi", "--json"])
        .output()
        .unwrap();
    success(&ready);
    let json: serde_json::Value = serde_json::from_slice(&ready.stdout).unwrap();
    assert_eq!(json["ready"], true, "{json}");
    assert!(root.path().join(".awman/Dockerfile.pi").is_file());
    let output = cli(root.path(), home.path())
        .args(["chat", "--agent", "pi", "--non-interactive"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(37), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("WI0130_CUSTOM_AGENT"),
        "{output:?}"
    );
    eprintln!("EXECUTED: {runtime}: custom pi guest exited 37 with expected marker");
}

#[test]
fn docker_custom_pi_ready_and_chat_runtime_smoke() {
    if !opted_in("AWMAN_TEST_DOCKER") || !crate::helpers::docker_available() {
        eprintln!("SKIP: custom pi requires AWMAN_TEST_DOCKER=1 and a running Docker daemon");
        return;
    }
    smoke_container_runtime("docker", "docker");
}

#[test]
fn apple_custom_pi_ready_and_chat_runtime_smoke() {
    if !opted_in("AWMAN_TEST_APPLE_CONTAINER")
        || !cfg!(all(target_os = "macos", target_arch = "aarch64"))
    {
        eprintln!("SKIP: custom pi requires AWMAN_TEST_APPLE_CONTAINER=1 on Apple Silicon macOS");
        return;
    }
    smoke_container_runtime("container", "apple-containers");
}

#[test]
fn docker_new_agent_crush_from_example_ready_builds_and_smokes_without_network() {
    if !opted_in("AWMAN_TEST_DOCKER") || !crate::helpers::docker_available() {
        eprintln!("SKIP: crush image e2e requires AWMAN_TEST_DOCKER=1 and Docker");
        return;
    }
    let Some(base) = cached_base("docker") else {
        return;
    };
    let root = tempfile::Builder::new()
        .prefix("wi0130-crush-")
        .tempdir()
        .unwrap();
    let home = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "--quiet"]);
    success(
        &cli(root.path(), home.path())
            .args(["new", "agent", "crush", "--from", "crush"])
            .output()
            .unwrap(),
    );
    // Keep the shipped definition byte-for-byte. Its dependency commands are
    // satisfied by local fixture tools in the project base, so ready exercises
    // real rendering, image builds, runtime-user smoke and launch without npm,
    // apt or NodeSource contacting an external service.
    let tools = root.path().join("fixture-tools");
    std::fs::create_dir_all(&tools).unwrap();
    let scripts = [
        ("apt-get", "#!/bin/sh\nexit 0\n"),
        ("curl", "#!/bin/sh\nprintf ':\\n'\n"),
        ("npm", "#!/bin/sh\nset -eu\ntest \"$*\" = 'install -g --prefix=/usr/local @charmland/crush'\ncp /fixture/crush /usr/local/bin/crush\nchmod 755 /usr/local/bin/crush\n"),
        ("crush", "#!/bin/sh\nprintf 'WI0130_CRUSH_SMOKE\\n'\n"),
    ];
    for (name, body) in scripts {
        let path = tools.join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    std::fs::write(root.path().join("Dockerfile.dev"), format!("FROM {base}\nCOPY fixture-tools/ /fixture/\nCOPY fixture-tools/apt-get fixture-tools/curl fixture-tools/npm /usr/local/bin/\n")).unwrap();
    std::fs::write(
        root.path().join(".awman/config.json"),
        r#"{"agent":"crush","runtime":"docker","auth":"none"}"#,
    )
    .unwrap();
    let tag = awman::data::image_tags::agent_image_tag(root.path(), "crush");
    let _images = Images {
        program: "docker",
        tags: vec![
            tag.clone(),
            awman::data::image_tags::project_image_tag(root.path()),
        ],
    };
    let ready = cli(root.path(), home.path())
        .env(
            "PATH",
            std::env::join_paths(std::iter::once(tools.clone()).chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            )))
            .unwrap(),
        )
        .args(["ready", "--agent", "crush", "--json"])
        .output()
        .unwrap();
    success(&ready);
    let json: serde_json::Value = serde_json::from_slice(&ready.stdout).unwrap();
    assert_eq!(json["ready"], true, "{json}");
    let rendered = std::fs::read_to_string(root.path().join(".awman/Dockerfile.crush")).unwrap();
    assert!(rendered.contains("@charmland/crush"));
    assert!(
        rendered.contains("command -v crush"),
        "render includes runtime-user smoke check"
    );
    let smoke = Command::new(awman::engine::host_cli::program("docker"))
        .args([
            "run",
            "--rm",
            "--network",
            "none",
            "--entrypoint",
            "crush",
            &tag,
            "run",
            "Hello",
        ])
        .output()
        .unwrap();
    success(&smoke);
    assert_eq!(
        String::from_utf8_lossy(&smoke.stdout).trim(),
        "WI0130_CRUSH_SMOKE"
    );
    eprintln!("EXECUTED: Docker built the shipped Crush recipe using local dependency fixtures and smoke-tested the image");
}

/// No unattended or "assume yes" invocation approves a repo definition or
/// runs its host `[ping]`: each fails, the trust store holds nothing, and the
/// sentinel the ping would create never appears.
#[cfg(unix)]
#[test]
fn unattended_invocations_never_approve_or_ping() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "--quiet"]);
    let sentinel = home.path().join("ping-ran");
    let agents = root.path().join(".awman/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("pi.toml"),
        format!(
            "schema=1\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[ping]\ncommand=['/bin/sh', '-c', 'touch {}']\n[image]\ninstall=['true']\n",
            sentinel.display()
        ),
    )
    .unwrap();
    for argv in [
        &["ready", "--agent", "pi"][..],
        &["ready", "--agent", "pi", "-n"],
        &["ready", "--agent", "pi", "--json"],
        &["ready", "--agent", "pi", "--yes"],
        &["chat", "--agent", "pi", "--non-interactive"],
        &["exec", "prompt", "--agent", "pi", "-n", "hi"],
    ] {
        let output = cli(root.path(), home.path()).args(argv).output().unwrap();
        assert!(!output.status.success(), "{argv:?}: {output:?}");
        let text = String::from_utf8_lossy(&output.stderr).to_string()
            + &String::from_utf8_lossy(&output.stdout);
        // `--yes` is not a flag of these commands: it is refused before
        // anything runs. Every other form reaches the trust refusal.
        if argv.contains(&"--yes") {
            assert_eq!(output.status.code(), Some(2), "{argv:?}: {text}");
        } else {
            assert!(text.contains("has not been approved"), "{argv:?}: {text}");
            assert_eq!(output.status.code(), Some(2), "{argv:?}: {text}");
        }
        let store = home.path().join(".awman/trusted-agents.json");
        let stored = std::fs::read_to_string(&store).unwrap_or_default();
        assert!(!stored.contains("pi"), "{argv:?} approved: {stored}");
        assert!(!sentinel.exists(), "{argv:?} ran the host ping");
    }
}
