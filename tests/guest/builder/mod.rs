//! Build/import/rebuild/cached execution. Real builder stores are explicit grants.
use crate::support;
use awman_test_support::capability::{AppleContainers, Docker};

fn run(id: &'static str, apple: bool) {
    let mut s = support::sandbox(id);
    let archive = support::fixture(&s);
    let (cli, source) = if apple {
        let store = s.require::<AppleContainers>();
        s.grant(&store);
        (store.cli.clone(), serde_json::json!({"type":"apple-local"}))
    } else {
        let store = s.require::<Docker>();
        s.grant(&store);
        (
            store.cli.clone(),
            serde_json::json!({"type":"docker-local", "host":store.host}),
        )
    };
    // This is an immutable, prebuilt fixture; loading it never builds or pulls.
    s.command(&cli)
        .args(["image", "load", "--input"])
        .arg(&archive)
        .run()
        .assert_success();
    support::watch(&s);
    let repo = support::session::prepare(
        &s,
        &archive,
        "claude",
        "#!/bin/sh\nIFS= read -r p\necho BUILDER_UID=$(id -u)\ncat /etc/builder-marker\n",
    );
    std::fs::write(
        repo.join("Dockerfile.dev"),
        "FROM awman-test/guest:fixture\nRUN printf 'v1\\n' > /etc/builder-marker\n",
    )
    .unwrap();
    let mut global: serde_json::Value =
        serde_json::from_slice(&std::fs::read(s.awman_home().join("config.json")).unwrap())
            .unwrap();
    global["runtime"]["builtin"]["builder"] = serde_json::json!({"enabled":true});
    global["runtime"]["builtin"]["imageSource"] = source;
    std::fs::write(
        s.awman_home().join("config.json"),
        serde_json::to_vec(&global).unwrap(),
    )
    .unwrap();
    // A descriptor ping stand-in only accepts the fixed readiness invocation;
    // it can never execute a user task on the host.
    let fake_exe = support::fake_cli_exe();
    s.register_exe("fake-cli", fake_exe);
    let ping = s.install_fake(fake_exe, "claude");
    ping.on(&["--model", "haiku", "--print"]).stdout("hello\n");
    for version in ["v1", "v2"] {
        std::fs::write(
            repo.join("Dockerfile.dev"),
            format!(
                "FROM awman-test/guest:fixture\nRUN printf '{version}\\n' > /etc/builder-marker\n"
            ),
        )
        .unwrap();
        let ready = support::awman(&s)
            .current_dir(&repo)
            .env("AWMAN_TEST_BUILTIN", "1")
            .args(["--build", "ready", "--json"])
            .run();
        ready.assert_success();
        let result: serde_json::Value = serde_json::from_slice(&ready.stdout).unwrap();
        assert_eq!(result["ready"], true, "{result}");
        let out = support::session::exec(&s, support::awman_exe(), &repo, "claude", "builder");
        out.assert_success();
        assert!(out.stdout_text().contains("BUILDER_UID=1234"), "{out:?}");
        assert!(out.stdout_text().lines().any(|l| l == version), "{out:?}");
    }
    // Switch PATH to private traps, preserving source endpoint identity. Cached
    // launch must succeed without invoking any builder or rebuilding a fixture.
    let docker = s.install_fake(fake_exe, "docker");
    docker.on(&[]).exit(91);
    let container = s.install_fake(fake_exe, "container");
    container.on(&[]).exit(91);
    let out = support::session::exec(&s, support::awman_exe(), &repo, "claude", "cached");
    out.assert_success();
    assert!(out.stdout_text().contains("v2"), "{out:?}");
    assert!(
        docker.calls().is_empty() && container.calls().is_empty(),
        "cached execution reached builder"
    );
    // Remove only images owned by this unique project, never stop a shared service.
    let stem = repo.file_name().unwrap().to_string_lossy();
    for tag in [
        format!("awman-{stem}-claude:latest"),
        format!("awman-{stem}:latest"),
    ] {
        s.command(&cli)
            .args(["image", "rm"])
            .arg(tag)
            .run()
            .assert_success();
    }
}
#[test]
fn docker_build_import_rebuild_then_cached_execution() {
    run(
        "builder::docker_build_import_rebuild_then_cached_execution",
        false,
    );
}
#[test]
fn apple_build_import_rebuild_then_cached_execution() {
    run(
        "builder::apple_build_import_rebuild_then_cached_execution",
        true,
    );
}
