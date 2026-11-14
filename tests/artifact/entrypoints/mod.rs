use crate::support;
#[test]
fn version_and_provider_info_work_without_path_or_network() {
    let mut s =
        support::sandbox("entrypoints::version_and_provider_info_work_without_path_or_network");
    let a = support::artifact(&mut s);
    let out = support::run_artifact(&s, &a)
        .arg("--version")
        .env("PATH", "")
        .run();
    out.assert_success();
    assert!(out.stdout_text().starts_with("awman "), "{out:?}");
    #[cfg(not(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(
            target_os = "linux",
            any(target_arch = "aarch64", target_arch = "x86_64")
        )
    )))]
    {
        let refused = support::run_artifact(&s, &a)
            .arg("__awman-builtin-info")
            .run();
        assert_eq!(refused.exit, awman_test_support::Exit::Code(70));
    }
    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(
            target_os = "linux",
            any(target_arch = "aarch64", target_arch = "x86_64")
        )
    ))]
    {
        let info = support::info(&s, &a);
        assert_eq!(info["host_helpers"], false);
        assert_eq!(info["embedded_kernel"], true);
        assert_eq!(info["embedded_guest_agent"], true);
        assert_eq!(info["target_os"], std::env::consts::OS);
        assert_eq!(info["target_arch"], std::env::consts::ARCH);
        let manifest: toml::Value = toml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../third_party/msb-payloads/manifest.toml"
        )))
        .unwrap();
        let target = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
            ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
            ("macos", "aarch64") => "aarch64-apple-darwin",
            platform => panic!("unsupported builtin artifact target: {platform:?}"),
        };
        assert_eq!(
            info["kernel_sha256"].as_str(),
            manifest["targets"][target]["kernel_sha256"].as_str(),
            "candidate's embedded kernel differs from the pinned payload"
        );
    }
}
#[test]
fn worker_refuses_missing_descriptors_without_echoing_secrets() {
    let mut s =
        support::sandbox("entrypoints::worker_refuses_missing_descriptors_without_echoing_secrets");
    let a = support::artifact(&mut s);
    let out = support::run_artifact(&s, &a)
        .args([
            "machine",
            "--config-fd",
            "-1",
            "--private-secret=canary-secret",
        ])
        .run();
    assert!(!out.exit.success());
    assert!(!format!(
        "{}{}",
        out.stdout_text(),
        String::from_utf8_lossy(&out.stderr)
    )
    .contains("canary-secret"));
    support::no_helpers(&s);
}
#[test]
fn ambient_overrides_fail_closed_and_redact_values() {
    let mut s = support::sandbox("entrypoints::ambient_overrides_fail_closed_and_redact_values");
    let a = support::artifact(&mut s);
    for name in [
        "MSB_PATH",
        "MSB_HOME",
        "MSB_BACKEND",
        "MSB_AGENTD_PATH",
        "MSB_LIBKRUNFW_PATH",
    ] {
        let out = support::run_artifact(&s, &a)
            .env(name, "secret-runtime-override")
            .arg("__awman-builtin-info")
            .run();
        assert_eq!(out.exit, awman_test_support::Exit::Code(64));
        assert!(!format!("{out:?}").contains("secret-runtime-override"));
    }
    support::no_helpers(&s);
}
