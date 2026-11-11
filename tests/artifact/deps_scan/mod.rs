use crate::support;
#[test]
fn runtime_dependencies_are_embedded_and_helpers_are_not_extracted() {
    let mut s = support::sandbox(
        "deps_scan::runtime_dependencies_are_embedded_and_helpers_are_not_extracted",
    );
    let a = support::artifact(&mut s);
    #[cfg(target_os = "linux")]
    let deps = s
        .command("/usr/bin/readelf")
        .args(["--dynamic", "--wide"])
        .arg(&a.exe)
        .run();
    #[cfg(target_os = "macos")]
    let deps = s.command("/usr/bin/otool").arg("-L").arg(&a.exe).run();
    #[cfg(windows)]
    let deps = {
        let scanner = std::path::PathBuf::from(
            awman_test_support::runtime_env::get()
                .host_resource("dependency-scanner")
                .expect("BLOCKED[dependency-scanner]: supply the absolute native dumpbin.exe path"),
        );
        assert!(
            scanner.is_absolute() && scanner.is_file(),
            "BLOCKED[dependency-scanner]: native dumpbin.exe missing"
        );
        s.allow_tool(&scanner);
        s.command(scanner).arg("/DEPENDENTS").arg(&a.exe).run()
    };
    deps.assert_success();
    let text = deps.stdout_text().to_lowercase();
    for forbidden in ["libsqlite3", "libcap-ng", "libkrun", "microsandbox.dll"] {
        assert!(
            !text.contains(forbidden),
            "forbidden dependency {forbidden}: {text}"
        );
    }
    #[cfg(target_os = "linux")]
    {
        let sections = s
            .command("/usr/bin/readelf")
            .args(["--sections", "--wide"])
            .arg(&a.exe)
            .run();
        sections.assert_success();
        assert!(
            sections.stdout_text().contains(".msbver"),
            "embedded provider section missing"
        );
    }
    #[cfg(target_os = "macos")]
    {
        let sections = s.command("/usr/bin/otool").arg("-l").arg(&a.exe).run();
        sections.assert_success();
        assert!(sections.stdout_text().contains("__msbver"));
    }
    support::record(
        &format!("dependencies-{}.json", a.sha256),
        &serde_json::json!({"artifact_sha256":a.sha256,"dependencies":text}),
    );
    let out = support::run_artifact(&s, &a)
        .env("PATH", "")
        .arg("__awman-builtin-kernel-check")
        .run();
    if cfg!(windows) {
        assert_eq!(out.exit, awman_test_support::Exit::Code(70));
    } else {
        out.assert_success();
    }
    support::no_helpers(&s);
}
