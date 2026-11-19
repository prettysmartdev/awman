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
    let load_archive = if apple {
        archive.clone()
    } else {
        s.fixture("guest-docker-base")
    };
    s.command(&cli)
        .args(["image", "load", "--input"])
        .arg(&load_archive)
        .run()
        .assert_success();
    support::watch(&s);
    let repo = support::session::prepare(
        &s,
        &archive,
        "claude",
        "#!/bin/sh\nIFS= read -r p\necho BUILDER_UID=$(id -u)\ncat /etc/builder-marker\n",
    );
    // The shared archive fixture never builds its placeholder agent image.
    // This builder journey needs the rendered project tag, as provisioning
    // writes it after substituting the bundled template's base-image marker.
    std::fs::write(
        repo.join(".awman/Dockerfile.claude"),
        format!(
            "FROM {}\n",
            awman::data::image_tags::project_image_tag(&repo)
        ),
    )
    .unwrap();
    std::fs::write(
        repo.join("Dockerfile.dev"),
        "FROM awman-test/guest:fixture\nUSER root\nRUN printf 'v1\\n' > /etc/builder-marker\nUSER probe\n",
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
    // session::prepare already installed the private descriptor readiness ping.
    for version in ["v1", "v2"] {
        std::fs::write(
            repo.join("Dockerfile.dev"),
            format!(
                "FROM awman-test/guest:fixture\nUSER root\nRUN printf '{version}\\n' > /etc/builder-marker\nUSER probe\n"
            ),
        )
        .unwrap();
        let ready = support::awman(&s)
            .current_dir(&repo)
            .env("AWMAN_TEST_BUILTIN", "1");
        // The first absent image exercises the enabled builder's automatic
        // preparation path. The second revision explicitly forces a rebuild.
        let ready = if version == "v1" {
            ready.args(["ready", "--json"])
        } else {
            ready.args(["--build", "ready", "--json"])
        }
        .run();
        ready.assert_success();
        let result: serde_json::Value = serde_json::from_slice(&ready.stdout).unwrap();
        assert_eq!(result["ready"], true, "{result}");
        let out = support::session::exec(&s, support::awman_exe(), &repo, "claude", "builder");
        out.assert_success();
        assert!(out.stdout_text().contains("BUILDER_UID=1234"), "{out:?}");
        assert!(out.stdout_text().lines().any(|l| l == version), "{out:?}");
    }
    // Remove this project’s source images before cached execution. Any new
    // source acquisition now fails; the imported guest must remain usable.
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
    // Switch PATH to private traps, preserving source endpoint identity. Cached
    // launch retains the source freshness check, but must survive its failure
    // without building, exporting or acquiring another image.
    let fake_exe = support::fake_cli_exe();
    let docker = s.install_fake(fake_exe, "docker");
    docker.on(&[]).exit(91);
    let container = s.install_fake(fake_exe, "container");
    container.on(&[]).exit(91);
    let out = support::session::exec(&s, support::awman_exe(), &repo, "claude", "cached");
    out.assert_success();
    assert!(out.stdout_text().contains("v2"), "{out:?}");
    // Enabled builders check the source's input labels even for an installed
    // image (the image freshness contract). Only the first read-only project
    // inspection may reach the unavailable CLI; no build or export is allowed.
    let expected = if apple {
        vec![
            "image".into(),
            "inspect".into(),
            awman::data::image_tags::project_image_tag(&repo),
        ]
    } else {
        vec![
            "--host".into(),
            global["runtime"]["builtin"]["imageSource"]["host"]
                .as_str()
                .unwrap()
                .into(),
            "image".into(),
            "inspect".into(),
            awman::data::image_tags::project_image_tag(&repo),
        ]
    };
    let selected_calls = if apple {
        container.calls()
    } else {
        docker.calls()
    };
    let other_calls = if apple {
        docker.calls()
    } else {
        container.calls()
    };
    assert!(
        !selected_calls.is_empty(),
        "enabled builder omitted its source freshness check"
    );
    assert!(
        selected_calls
            .iter()
            .all(|call| call.argv == expected && call.exit == Some(91)),
        "cached launch exceeded its failed metadata inspection: {selected_calls:#?}"
    );
    assert!(
        other_calls.is_empty(),
        "cached launch reached another source: {other_calls:#?}"
    );

    // Disabling the builder gives a truly source-independent cached launch:
    // even read-only CLI probes are forbidden, and both traps stay installed.
    global["runtime"]["builtin"]["builder"]["enabled"] = false.into();
    std::fs::write(
        s.awman_home().join("config.json"),
        serde_json::to_vec(&global).unwrap(),
    )
    .unwrap();
    let docker_before = docker.calls();
    let container_before = container.calls();
    let out = support::session::exec(
        &s,
        support::awman_exe(),
        &repo,
        "claude",
        "cached-builder-disabled",
    );
    out.assert_success();
    assert!(out.stdout_text().contains("v2"), "{out:?}");
    assert_eq!(
        docker.calls(),
        docker_before,
        "disabled builder invoked Docker"
    );
    assert_eq!(
        container.calls(),
        container_before,
        "disabled builder invoked Apple Containers"
    );
}
#[test]
fn docker_build_import_rebuild_then_cached_execution() {
    run(
        "builder::docker_build_import_rebuild_then_cached_execution",
        false,
    );
}
#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn apple_build_import_rebuild_then_cached_execution() {
    run(
        "builder::apple_build_import_rebuild_then_cached_execution",
        true,
    );
}
