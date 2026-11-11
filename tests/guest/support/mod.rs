//! Guest ownership, fixture verification and supervised children. No host opt-in skips.
#![allow(dead_code)]
pub mod session;
use awman_test_support::capability::{GuestFixtures, Hypervisor};
use awman_test_support::{Exit, Leak, OwnedCommand, TestCase, TestSandbox};
use std::path::{Path, PathBuf};

pub fn awman_exe() -> &'static Path {
    static EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    EXE.get_or_init(|| companion("awman"))
}
pub fn guest_driver_exe() -> &'static Path {
    static EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    EXE.get_or_init(|| companion("awman-guest-driver"))
}
pub fn guest_net_driver_exe() -> &'static Path {
    static EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    EXE.get_or_init(|| companion("awman-guest-net-driver"))
}
pub fn fake_cli_exe() -> &'static Path {
    static EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    EXE.get_or_init(|| companion("awman-fake-cli"))
}
// Archives relocate as a unit: never fall back to a build machine's baked-in
// Cargo path, where a later rebuild could silently replace the archived input.
fn companion(name: &str) -> PathBuf {
    let test = std::env::current_exe().expect("test executable path");
    let parent = test.parent().unwrap();
    let profile = if parent.file_name().is_some_and(|n| n == "deps") {
        parent.parent().unwrap()
    } else {
        parent
    };
    let path = profile.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "BLOCKED[test-archive]: missing companion {}",
        path.display()
    );
    path
}
pub struct GuestSandbox {
    sandbox: TestSandbox,
    _state: tempfile::TempDir,
}
impl std::ops::Deref for GuestSandbox {
    type Target = TestSandbox;
    fn deref(&self) -> &TestSandbox {
        &self.sandbox
    }
}
impl std::ops::DerefMut for GuestSandbox {
    fn deref_mut(&mut self) -> &mut TestSandbox {
        &mut self.sandbox
    }
}
pub fn sandbox(id: &'static str) -> GuestSandbox {
    sandbox_case(TestCase::guest(id))
}
pub fn soak_sandbox(id: &'static str) -> GuestSandbox {
    sandbox_case(TestCase::soak(id))
}
fn sandbox_case(case: TestCase) -> GuestSandbox {
    let mut s = TestSandbox::for_test(case);
    s.register_exe("fake-cli", fake_cli_exe());
    for (name, exe) in [
        ("awman", awman_exe()),
        ("guest-driver", guest_driver_exe()),
        ("guest-net-driver", guest_net_driver_exe()),
    ] {
        s.register_exe(name, exe);
    }
    let vm_state = tempfile::Builder::new()
        .prefix("awg-")
        .rand_bytes(8)
        .tempdir_in(
            awman_test_support::runtime_env::get()
                .tmproot
                .clone()
                .unwrap_or_else(|| awman_test_support::runtime_env::get().system_temp.clone()),
        )
        .expect("BLOCKED[tempdir]: private VM root");
    std::fs::write(
        s.root().join("vm-state-path"),
        vm_state.path().join("s").to_str().unwrap(),
    )
    .unwrap();
    let owner = s.root().file_name().unwrap().to_string_lossy().into_owned();
    s.note("vm_owner", &owner);
    s.note("vm_state_root", &state(&s).display().to_string());
    GuestSandbox {
        sandbox: s,
        _state: vm_state,
    }
}
pub fn state(s: &TestSandbox) -> PathBuf {
    PathBuf::from(std::fs::read_to_string(s.root().join("vm-state-path")).unwrap())
}
pub fn awman(s: &TestSandbox) -> OwnedCommand {
    s.command(awman_exe()).label("actual awman")
}
pub fn fixture(s: &TestSandbox) -> PathBuf {
    s.require::<Hypervisor>();
    let fixtures = s.require::<GuestFixtures>();
    let path = fixtures
        .paths
        .get("guest-oci-base")
        .expect("manifest must pin guest-oci-base")
        .clone();
    s.note(
        "fixture_guest-oci-base_sha256",
        &awman_test_support::fixtures::sha256_file(&path).unwrap(),
    );
    path
}
pub fn checked(output: awman_test_support::process::Output) -> String {
    assert_ne!(
        output.exit,
        Exit::Code(77),
        "BLOCKED[guest-driver]: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    output.assert_success();
    output.stdout_text()
}
pub fn scenario(s: &TestSandbox, scenario: &str) -> String {
    let archive = fixture(s);
    watch(s);
    let scripts = s.root().join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    for script in ["guest-checks.sh", "image-checks.sh"] {
        std::fs::write(scripts.join(script), script_source(script)).unwrap();
    }
    let out = checked(
        s.command(guest_driver_exe())
            .args(["--scenario", scenario, "--state-dir"])
            .arg(state(s))
            .arg("--archive")
            .arg(archive)
            .args(["--tag", "awman-test/guest:fixture", "--work"])
            .arg(s.work())
            .arg("--scripts")
            .arg(scripts)
            .run(),
    );
    assert!(
        out.contains("LEFTOVER\t0"),
        "driver must report its owner-scoped cleanup: {out}"
    );
    out
}
/// Attach an owner-state leak probe to every real CLI session. The library creates a
/// fresh runtime owner UUID per process; the private state root bounds enumeration.
#[cfg(awman_builtin)]
pub fn watch(s: &TestSandbox) {
    let root = state(s);
    s.on_finish(
        "guest VMs and images",
        Box::new(move || {
            if !root.exists() {
                return Vec::new();
            }
            let rt = match awman::engine::container::ContainerRuntime::builtin(
                awman::engine::container::runtime::BuiltinRuntimeSettings {
                    state_dir: root.clone(),
                    ambient_overrides: Vec::new(),
                    test_isolation: false,
                },
            ) {
                Ok(rt) => rt,
                Err(e) => return vec![Leak::new("vm", format!("cannot audit owner state: {e}"))],
            };
            let mut leaks = Vec::new();
            match rt.list_running_all() {
                Ok(vms) => {
                    for vm in vms {
                        leaks.push(Leak::new("vm", &vm.id));
                        let _ = rt.stop(&vm);
                    }
                }
                Err(e) => leaks.push(Leak::new("vm", format!("cannot enumerate: {e}"))),
            }
            if let Ok(vms) = rt.list_stopped() {
                for vm in vms {
                    if let Err(e) = rt.remove_container(&vm.id) {
                        leaks.push(Leak::new("vm", e.to_string()));
                    }
                }
            }
            match rt.list_dangling_images() {
                Ok(images) => {
                    for image in images {
                        if let Err(e) = rt.remove_image(&image.id) {
                            leaks.push(Leak::new("image", e.to_string()));
                        }
                    }
                }
                Err(e) => leaks.push(Leak::new("image", format!("cannot enumerate: {e}"))),
            }
            leaks
        }),
    );
}
#[cfg(not(awman_builtin))]
pub fn watch(_: &TestSandbox) {
    panic!("BLOCKED[builtin-payload]: this build has no VM payload");
}
/// Require every fixture contract assertion exactly once; an empty/partial
/// transcript, UNSUPPORTED marker or missing completion is a failure.
pub fn transcript(stdout: &str, script: &str, complete: &str) {
    use std::collections::BTreeSet;
    let source = script_source(script);
    let expected: BTreeSet<_> = source
        .lines()
        .filter_map(|line| {
            line.strip_prefix("check ")
                .and_then(|rest| rest.split_whitespace().next())
        })
        .collect();
    assert!(!expected.is_empty());
    let mut seen = BTreeSet::new();
    for line in stdout.lines() {
        assert!(
            !line.starts_with("FAIL\t") && !line.starts_with("UNSUPPORTED\t"),
            "{stdout}"
        );
        if let Some(name) = line.strip_prefix("PASS\t") {
            assert!(seen.insert(name), "duplicate {name}");
        }
    }
    assert_eq!(seen, expected, "incomplete guest contract: {stdout}");
    assert!(
        stdout.lines().any(|line| line == complete),
        "missing {complete}: {stdout}"
    );
}

fn script_source(script: &str) -> &'static str {
    match script {
        "guest-checks.sh" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/guest/support/mount-contract.sh"
        )),
        "image-checks.sh" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/guest/support/image-contract.sh"
        )),
        other => panic!("unknown guest contract {other}"),
    }
}
