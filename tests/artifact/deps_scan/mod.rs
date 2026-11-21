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
        // GNU releases may use the operating system's C ABI and compiler
        // unwinder. Runtime providers, databases, TLS and compression must not
        // become machine-local shared-library prerequisites.
        let allowed = [
            "libc.so.6",
            "libgcc_s.so.1",
            "libm.so.6",
            "libpthread.so.0",
            "libdl.so.2",
            "librt.so.1",
            "libutil.so.1",
            "ld-linux-x86-64.so.2",
            "ld-linux-aarch64.so.1",
        ];
        for line in text.lines().filter(|line| line.contains("(needed)")) {
            let library = line
                .split_once('[')
                .and_then(|(_, rest)| rest.split_once(']'))
                .map(|(name, _)| name)
                .expect("readelf NEEDED entry contains a library name");
            assert!(
                allowed.contains(&library),
                "unexpected shared dependency: {library}"
            );
        }
        let versions = s
            .command("/usr/bin/readelf")
            .args(["--version-info", "--wide"])
            .arg(&a.exe)
            .run();
        versions.assert_success();
        support::record(
            &format!("abi-{}.json", a.sha256),
            &serde_json::json!({"artifact_sha256":a.sha256,"versions":versions.stdout_text()}),
        );
        let sections = s
            .command("/usr/bin/readelf")
            .args(["--sections", "--wide"])
            .arg(&a.exe)
            .run();
        sections.assert_success();
        assert_eq!(
            sections.stdout_text().contains(".msbver"),
            cfg!(target_arch = "x86_64"),
            "embedded provider section differs from this platform's release policy"
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
    if cfg!(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64")
    )) {
        out.assert_success();
    } else {
        assert_eq!(out.exit, awman_test_support::Exit::Code(70));
    }
    support::no_helpers(&s);
}
