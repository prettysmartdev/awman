//! Exact candidate bytes only. Never resolves CARGO_BIN_EXE_awman or runs Cargo.
#![allow(dead_code)]
pub mod session;
use awman_test_support::capability::{Offline, ReleaseArtifact};
use awman_test_support::{Granted, Leak, OwnedCommand, TestCase, TestSandbox};
use std::path::{Path, PathBuf};
pub fn fake_cli_exe() -> &'static Path {
    static EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    EXE.get_or_init(|| {
        let test = std::env::current_exe().expect("test executable path");
        let parent = test.parent().unwrap();
        let profile = if parent.file_name().is_some_and(|n| n == "deps") {
            parent.parent().unwrap()
        } else {
            parent
        };
        let path = profile.join(format!("awman-fake-cli{}", std::env::consts::EXE_SUFFIX));
        assert!(
            path.is_file(),
            "BLOCKED[test-archive]: missing companion {}",
            path.display()
        );
        path
    })
}
pub struct ArtifactSandbox {
    sandbox: TestSandbox,
    _state: tempfile::TempDir,
}
impl std::ops::Deref for ArtifactSandbox {
    type Target = TestSandbox;
    fn deref(&self) -> &TestSandbox {
        &self.sandbox
    }
}
impl std::ops::DerefMut for ArtifactSandbox {
    fn deref_mut(&mut self) -> &mut TestSandbox {
        &mut self.sandbox
    }
}
pub fn sandbox(id: &'static str) -> ArtifactSandbox {
    let mut sandbox = TestSandbox::for_test(TestCase::artifact(id));
    sandbox.register_exe("fake-cli", fake_cli_exe());
    let mut builder = tempfile::Builder::new();
    builder.prefix("awa-").rand_bytes(8);
    let state = if cfg!(target_os = "macos") {
        builder.tempdir_in(
            awman_test_support::runtime_env::get()
                .tmproot
                .clone()
                .unwrap_or_else(std::env::temp_dir),
        )
    } else {
        builder.tempdir()
    }
    .expect("BLOCKED[tempdir]: private artifact state");
    std::fs::write(
        sandbox.root().join("vm-state-path"),
        state.path().join("s").to_str().unwrap(),
    )
    .unwrap();
    let root = state.path().join("s");
    sandbox.note("vm_state_root", &root.display().to_string());
    sandbox.note(
        "vm_owner",
        &sandbox.root().file_name().unwrap().to_string_lossy(),
    );
    sandbox.on_finish("private VM catalog", Box::new(move || {
        let catalog = root.join("db/msb.db");
        if !catalog.exists() {
            return Vec::new();
        }
        let audit = || -> rusqlite::Result<i64> {
            let connection = rusqlite::Connection::open_with_flags(&catalog,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            connection.query_row(
                "SELECT COUNT(*) FROM sandbox WHERE status IN ('Starting','Running','Draining','Paused')",
                [], |row| row.get(0))
        };
        match audit() {
            Ok(0) => Vec::new(),
            result => vec![Leak::new("vm", format!("private catalog retains active VM state: {result:?}"))],
        }
    }));
    ArtifactSandbox {
        sandbox,
        _state: state,
    }
}
pub fn artifact(s: &mut TestSandbox) -> Granted<ReleaseArtifact> {
    let a = s.require::<ReleaseArtifact>();
    assert!(
        a.exe.is_absolute(),
        "BLOCKED[release-artifact]: runner must supply an absolute executable path"
    );
    s.grant(&a);
    s.note("artifact_sha256", &a.sha256);
    let exe = a.exe.clone();
    let expected = a.sha256.clone();
    s.on_finish(
        "candidate bytes unchanged",
        Box::new(
            move || match awman_test_support::fixtures::sha256_file(&exe) {
                Ok(actual) if actual == expected => Vec::new(),
                other => vec![Leak::new(
                    "artifact",
                    format!("candidate changed during validation: {other:?}"),
                )],
            },
        ),
    );
    a
}
/// The wrapper is enforced per child; the Offline grant also records runner intent.
pub fn run_artifact(s: &TestSandbox, a: &ReleaseArtifact) -> OwnedCommand {
    run_exact(s, &a.exe)
}
pub fn run_exact(s: &TestSandbox, exe: &Path) -> OwnedCommand {
    s.require::<Offline>();
    #[cfg(target_os = "linux")]
    {
        let probe = s
            .command("/usr/bin/unshare")
            .args(["--user", "--map-root-user", "--net", "--", "/bin/true"])
            .run();
        assert!(
            probe.exit.success(),
            "BLOCKED[offline]: network namespace unavailable: {probe:?}"
        );
        s.command("/usr/bin/unshare")
            .args(["--user", "--map-root-user", "--net", "--"])
            .arg(exe)
            .label("offline exact artifact")
    }
    #[cfg(target_os = "macos")]
    {
        // Allow Unix IPC for the worker while denying IP traffic. Restrict
        // execution too: a helper extracted and deleted before our file scan
        // still cannot execute. Synthetic task agents are mounted only in VMs.
        let literal = serde_json::to_string(&exe.to_string_lossy()).unwrap();
        let bin = serde_json::to_string(&s.bin().to_string_lossy()).unwrap();
        let profile = format!(
            "(version 1)(allow default)(deny network*)\
             (allow network* (local unix-socket) (remote unix-socket))\
             (deny process-exec)(allow process-exec (literal {literal}) (subpath {bin})\
             (literal \"/usr/bin/git\") (literal \"/usr/bin/security\")\
             (literal \"/usr/bin/xcrun\")\
             (literal \"/Library/Developer/CommandLineTools/usr/bin/git\"))"
        );
        s.command("/usr/bin/sandbox-exec")
            .arg("-p")
            .arg(profile)
            .arg(exe)
            .label("offline exact artifact")
    }
    #[cfg(windows)]
    {
        // Windows runner provisioning enforces the outbound firewall rule. No
        // process-local substitute claims to enforce it here.
        assert_eq!(
            awman_test_support::runtime_env::get().host_resource("offline"),
            Some("windows-firewall"),
            "BLOCKED[offline]: isolated Windows firewall runner required"
        );
        s.command(exe).label("offline exact artifact")
    }
}
pub fn info(s: &TestSandbox, a: &ReleaseArtifact) -> serde_json::Value {
    let out = run_artifact(s, a)
        .arg("__awman-builtin-info")
        .env("PATH", "")
        .run();
    out.assert_success();
    serde_json::from_slice(&out.stdout).expect("provider info must be JSON")
}
pub fn verify_signature(s: &TestSandbox, a: &ReleaseArtifact) {
    s.command("/usr/bin/codesign")
        .args(["--verify", "--strict", "--verbose=2"])
        .arg(&a.exe)
        .run()
        .assert_success();
    let embedded = s
        .command("/usr/bin/codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(&a.exe)
        .run();
    embedded.assert_success();
    let expected = s.work().join("expected-entitlements.plist");
    let actual = s.work().join("embedded-entitlements.plist");
    std::fs::write(
        &expected,
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/macos/awman.entitlements"
        )),
    )
    .unwrap();
    std::fs::write(&actual, embedded.stdout).unwrap();
    s.note(
        "entitlements_sha256",
        &awman_test_support::fixtures::sha256_file(&expected).unwrap(),
    );
    let parse = |path: &Path| -> serde_json::Value {
        let out = s
            .command("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(path)
            .run();
        out.assert_success();
        serde_json::from_slice(&out.stdout).expect("valid entitlement dictionary")
    };
    assert_eq!(
        parse(&actual),
        parse(&expected),
        "candidate entitlements differ from committed shipping policy"
    );
}
pub fn state(s: &TestSandbox) -> PathBuf {
    PathBuf::from(std::fs::read_to_string(s.root().join("vm-state-path")).unwrap())
}
pub fn record(name: &str, value: &serde_json::Value) {
    let runtime = awman_test_support::runtime_env::get();
    let dir = runtime
        .evidence_dir
        .clone()
        .unwrap_or_else(|| runtime.system_temp.join("awman-artifact-measurements"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
    eprintln!("measurement: {}", dir.join(name).display());
}
pub fn files(root: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            result.extend(files(&path));
        } else {
            result.push(path);
        }
    }
    result
}
pub fn no_helpers(s: &TestSandbox) {
    let mut paths = files(s.root());
    let state_root = state(s);
    if state_root.exists() {
        paths.extend(files(&state_root));
    }
    for p in paths {
        let name = p.file_name().unwrap().to_string_lossy();
        assert!(
            !(name == "msb"
                || name == "agentd"
                || name.starts_with("libkrun")
                || name == "kernel.bin"),
            "extracted host helper: {}",
            p.display()
        );
    }
}
