//! Real-binary tests: worker dispatch before any runtime init, `--version`,
//! ambient-override refusal at the worker boundary, secrets/argv/side effects,
//! minimal-PATH execution, dependency listings, helper-extraction and
//! file-access scans. Every child runs with a scrubbed environment and a
//! throwaway HOME/TMPDIR; nothing here touches the developer's real state.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub fn awman() -> PathBuf {
    crate::awman_binary::awman_bin()
}

/// Everything a child may write lives under here, so scans see all side effects.
pub struct Scratch {
    pub dir: tempfile::TempDir,
}

impl Scratch {
    pub fn new() -> Self {
        Self {
            dir: tempfile::Builder::new()
                .prefix("awman-bin-")
                .tempdir_in("/tmp")
                .expect("scratch dir"),
        }
    }

    pub fn home(&self) -> PathBuf {
        let home = self.dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    /// A scrubbed command: no inherited environment, no stdin, isolated HOME.
    pub fn command(&self, binary: &Path, path_env: Option<&str>) -> Command {
        let home = self.home();
        let tmp = self.dir.path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let mut command = Command::new(binary);
        command
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("TMPDIR", &tmp)
            .env("AWMAN_TEST_ISOLATION", "1")
            .stdin(Stdio::null());
        if let Some(path) = path_env {
            command.env("PATH", path);
        }
        command
    }

    /// Every file or directory anywhere under the scratch dir, relative.
    pub fn entries(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) {
            if let Ok(read) = std::fs::read_dir(dir) {
                for entry in read.flatten() {
                    let path = entry.path();
                    out.push(path.strip_prefix(base).unwrap().to_path_buf());
                    if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        walk(&path, base, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(self.dir.path(), self.dir.path(), &mut out);
        out
    }
}

/// Run to completion but never for long: a worker that waits on descriptors it
/// was never given must not hang the suite.
pub fn run_limited(mut command: Command, limit: Duration) -> (Output, Duration) {
    let start = Instant::now();
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn awman");
    let pid = child.id();
    let waiter = std::thread::spawn(move || child.wait_with_output());
    loop {
        if waiter.is_finished() {
            return (waiter.join().unwrap().expect("output"), start.elapsed());
        }
        if start.elapsed() > limit {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            panic!("awman did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

const PATHS: &str = "/usr/bin:/bin";

/// The JSON printed by the private `__awman-builtin-info` route, or `None`
/// when this awman was built without the runtime (that route then exits 70).
pub fn builtin_info() -> Option<serde_json::Value> {
    static INFO: OnceLock<Option<serde_json::Value>> = OnceLock::new();
    INFO.get_or_init(|| {
        let scratch = Scratch::new();
        let output = scratch
            .command(&awman(), Some(PATHS))
            .arg("__awman-builtin-info")
            .output()
            .expect("run awman");
        if output.status.success() {
            serde_json::from_slice(&output.stdout).ok()
        } else {
            None
        }
    })
    .clone()
}

pub fn has_builtin_runtime() -> bool {
    builtin_info().is_some()
}

pub fn host_triple() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        _ => "unsupported",
    }
}

fn skip_without_runtime(test: &str) -> bool {
    if has_builtin_runtime() {
        return false;
    }
    eprintln!(
        "SKIP: {test}: this awman was built without the builtin runtime \
         (run `make test-builtin` / cargo test --features builtin-runtime)"
    );
    true
}

// ─── worker dispatch, version, refusals (run in every build) ────────────────

#[test]
fn builtin_version_output_is_unchanged() {
    let scratch = Scratch::new();
    let output = scratch
        .command(&awman(), Some(PATHS))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("awman {}\n", env!("CARGO_PKG_VERSION")),
        "the runtime version must never leak into --version"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn builtin_worker_route_is_hidden_from_humans() {
    for args in [
        vec!["machine"],
        vec!["machine", "--help"],
        vec!["machine", "--name", "x"],
    ] {
        let scratch = Scratch::new();
        let output = scratch
            .command(&awman(), Some(PATHS))
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", text(&output));
        assert!(
            text(&output).contains("unrecognized subcommand"),
            "{args:?}"
        );
    }
    let scratch = Scratch::new();
    let help = scratch
        .command(&awman(), Some(PATHS))
        .arg("--help")
        .output()
        .unwrap();
    let help = text(&help);
    assert!(!help.contains("machine") && !help.contains("__awman-builtin-info"));
}

#[test]
fn builtin_worker_dispatch_precedes_clap_config_and_runtime_startup() {
    // With the private argv shape the worker (or, without the runtime, the
    // "no worker" refusal) answers *before* clap, config loading, tracing or
    // Tokio: a clap error would be exit 2, and any startup would touch HOME.
    let scratch = Scratch::new();
    let (output, elapsed) = run_limited(
        {
            let mut c = scratch.command(&awman(), Some(PATHS));
            c.args(["machine", "--config-fd", "3"]);
            c
        },
        Duration::from_secs(10),
    );
    let code = output.status.code();
    let expected = if has_builtin_runtime() { 64 } else { 70 };
    assert_eq!(code, Some(expected), "{}", text(&output));
    assert!(output.stdout.is_empty());
    assert!(
        !text(&output).contains("Usage:"),
        "clap must not have parsed this"
    );
    assert!(elapsed < Duration::from_secs(5), "fails fast: {elapsed:?}");
    assert!(
        scratch
            .entries()
            .iter()
            .all(|p| p.starts_with("tmp") || p == Path::new("home")),
        "worker dispatch must not create config, state or log files: {:?}",
        scratch.entries()
    );
    assert!(std::fs::read_dir(scratch.home()).unwrap().next().is_none());
}

#[test]
fn builtin_worker_rejects_bad_descriptor_shapes_without_echoing_argv() {
    if skip_without_runtime("worker descriptor shapes") {
        return;
    }
    let shapes: [&[&str]; 4] = [
        // missing lifecycle lock fd
        &["machine", "--config-fd", "3", "--parent-watch-fd", "4"],
        // stdio descriptor
        &[
            "machine",
            "--config-fd",
            "2",
            "--parent-watch-fd",
            "4",
            "--lifecycle-lock-fd",
            "5",
        ],
        // duplicates
        &[
            "machine",
            "--config-fd",
            "3",
            "--parent-watch-fd",
            "3",
            "--lifecycle-lock-fd",
            "5",
        ],
        // file-based config transport is disabled
        &[
            "machine",
            "--config-file",
            "/tmp/should-not-be-read",
            "--config-fd",
            "3",
            "--parent-watch-fd",
            "4",
            "--lifecycle-lock-fd",
            "5",
        ],
    ];
    for args in shapes {
        let scratch = Scratch::new();
        let (output, _) = run_limited(
            {
                let mut c = scratch.command(&awman(), Some(PATHS));
                c.args(args);
                c
            },
            Duration::from_secs(10),
        );
        assert_eq!(
            output.status.code(),
            Some(64),
            "{args:?}: {}",
            text(&output)
        );
        assert!(
            !text(&output).contains("should-not-be-read"),
            "argv values must never be echoed"
        );
    }
}

#[test]
fn worker_unopened_fd_is_sanitized_error_not_abort() {
    if skip_without_runtime("unopened inherited descriptors") {
        return;
    }
    let scratch = Scratch::new();
    let (output, elapsed) = run_limited(
        {
            let mut command = scratch.command(&awman(), Some(PATHS));
            command.args([
                "machine",
                "--config-fd",
                "96",
                "--parent-watch-fd",
                "97",
                "--lifecycle-lock-fd",
                "99",
                "--sandbox-id",
                "1",
                "--name",
                "SECRET-NAME",
            ]);
            command
        },
        Duration::from_secs(10),
    );
    assert_eq!(output.status.code(), Some(64), "{}", text(&output));
    assert!(elapsed < Duration::from_secs(5));
    assert!(output.stdout.is_empty());
    let diagnostic = text(&output);
    assert!(diagnostic.contains("invalid private worker descriptors"));
    assert!(!diagnostic.contains("SECRET-NAME"));
    assert!(!diagnostic.contains("IO safety"));
}

#[test]
fn builtin_worker_never_leaks_secrets_from_its_environment() {
    let scratch = Scratch::new();
    let (output, _) = run_limited(
        {
            let mut c = scratch.command(&awman(), Some(PATHS));
            c.env("ANTHROPIC_API_KEY", "sk-ant-SECRET-VALUE")
                .env("GH_TOKEN", "ghp_SECRET_VALUE")
                .args(["machine", "--config-fd", "3"]);
            c
        },
        Duration::from_secs(10),
    );
    assert!(!text(&output).contains("SECRET"), "{}", text(&output));
}

#[test]
fn builtin_ambient_msb_overrides_are_refused_at_the_worker_boundary() {
    let names = [
        "MSB_PATH",
        "MSB_LIBKRUNFW_PATH",
        "MSB_AGENTD_PATH",
        "MSB_HOME",
        "MSB_BACKEND",
        "MSB_CONFIG_PATH",
        "MSB_PROFILE",
        "MSB_CACHE_DIR",
        "MSB_SANDBOXES_DIR",
        "MSB_VOLUMES_DIR",
        "MSB_SNAPSHOTS_DIR",
        "MSB_LOGS_DIR",
        "MSB_SECRETS_DIR",
    ];
    for route in [
        vec![
            "machine",
            "--config-fd",
            "3",
            "--parent-watch-fd",
            "4",
            "--lifecycle-lock-fd",
            "5",
        ],
        vec!["__awman-builtin-info"],
    ] {
        for name in names {
            let scratch = Scratch::new();
            let (output, _) = run_limited(
                {
                    let mut c = scratch.command(&awman(), Some(PATHS));
                    c.env(name, "/secret/override-value").args(&route);
                    c
                },
                Duration::from_secs(10),
            );
            assert_eq!(
                output.status.code(),
                Some(64),
                "{name} {route:?}: {}",
                text(&output)
            );
            assert!(text(&output).contains("ambient"), "{name}");
            assert!(
                !text(&output).contains("override-value"),
                "the override's value must never be printed"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn builtin_non_utf8_ambient_overrides_are_still_refused() {
    use std::os::unix::ffi::OsStringExt;
    let scratch = Scratch::new();
    let mut command = scratch.command(&awman(), Some(PATHS));
    command
        .env(
            "MSB_AGENTD_PATH",
            OsString::from_vec(vec![0x2f, 0xff, 0xfe, 0x2f]),
        )
        .arg("__awman-builtin-info");
    let (output, _) = run_limited(command, Duration::from_secs(10));
    assert_eq!(output.status.code(), Some(64), "{}", text(&output));
}

#[test]
fn builtin_private_info_route_without_the_runtime_refuses_cleanly() {
    if has_builtin_runtime() {
        return;
    }
    let scratch = Scratch::new();
    let output = scratch
        .command(&awman(), Some(PATHS))
        .arg("__awman-builtin-info")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(70));
    assert!(text(&output).contains("no builtin runtime worker"));
}

// ─── embedded-runtime provenance (feature builds) ────────────────────────────

#[test]
fn builtin_info_reports_the_strict_embedded_contract() {
    let Some(info) = builtin_info() else {
        skip_without_runtime("info contract");
        return;
    };
    assert_eq!(info["msb_version"], "0.7.2");
    assert_eq!(info["worker_protocol"], "0.7.2/18/awman-1");
    assert_eq!(info["embedded_kernel"], true);
    assert_eq!(info["embedded_guest_agent"], true);
    assert_eq!(info["host_helpers"], false, "no helper executables");
    assert_eq!(info["checkpoint_restore"], false);
    assert_eq!(info["reattach_after_owner_exit"], false);
    assert_eq!(info["target_os"], std::env::consts::OS);
    assert_eq!(info["target_arch"], std::env::consts::ARCH);
    assert_eq!(
        info["libkrun_commit"],
        "2bd0f84ad0956f3032e0490d3b8512b6851eca12"
    );

    // The kernel hash is the one the checksum-verified build input pins.
    let manifest: toml::Value = toml::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("third_party/msb-payloads/manifest.toml"),
        )
        .unwrap(),
    )
    .unwrap();
    let target = &manifest["targets"][host_triple()];
    assert_eq!(
        target["status"].as_str(),
        Some("verified"),
        "a builtin build needs a verified payload"
    );
    assert_eq!(
        info["kernel_sha256"].as_str(),
        target["kernel_sha256"].as_str()
    );
}

#[test]
fn builtin_info_works_with_an_empty_path_and_offline() {
    if skip_without_runtime("empty PATH") {
        return;
    }
    let scratch = Scratch::new();
    // No PATH at all: the embedded runtime must not look for any executable.
    let output = scratch
        .command(&awman(), None)
        .arg("__awman-builtin-info")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    let info: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(info["host_helpers"], false);
    // Nothing was written: no helper, firmware or runtime bundle was extracted.
    assert!(
        scratch
            .entries()
            .iter()
            .all(|p| p.starts_with("tmp") || p == Path::new("home")),
        "{:?}",
        scratch.entries()
    );
}

// ─── artifact scans ──────────────────────────────────────────────────────────

/// The binary the scans inspect: `AWMAN_TEST_BUILTIN_ARTIFACT` names a release
/// (optimized, stripped) artifact; by default the freshly built test binary.
fn artifact() -> PathBuf {
    std::env::var_os("AWMAN_TEST_BUILTIN_ARTIFACT")
        .map(PathBuf::from)
        .unwrap_or_else(awman)
}

const FORBIDDEN_LIBS: &[&str] = &[
    "libsqlite3",
    "libcap-ng",
    "libkrun",
    "libkrunfw",
    "libdbus",
    "libmsb",
    "libssl",
    "libcrypto",
];

fn dependency_listing(binary: &Path) -> Option<Vec<String>> {
    let run = |program: &str, args: &[&str]| -> Option<String> {
        let output = Command::new(program).args(args).arg(binary).output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    if cfg!(target_os = "macos") {
        return run("otool", &["-L"]).map(|t| {
            t.lines()
                .skip(1)
                .map(|l| l.split_whitespace().next().unwrap_or("").to_owned())
                .collect()
        });
    }
    if let Some(dynamic) = run("readelf", &["-d"]) {
        return Some(
            dynamic
                .lines()
                .filter(|l| l.contains("(NEEDED)"))
                .filter_map(|l| l.split('[').nth(1)?.split(']').next().map(str::to_owned))
                .collect(),
        );
    }
    run("ldd", &[]).map(|t| {
        t.lines()
            .map(|l| l.split_whitespace().next().unwrap_or("").to_owned())
            .collect()
    })
}

#[test]
fn builtin_dependency_listing_has_no_sqlite_firmware_or_helper_libraries() {
    let binary = artifact();
    let Some(deps) = dependency_listing(&binary) else {
        eprintln!(
            "SKIP: neither readelf/ldd nor otool could list {}",
            binary.display()
        );
        return;
    };
    for dep in &deps {
        for forbidden in FORBIDDEN_LIBS {
            assert!(
                !dep.contains(forbidden),
                "{} must not depend on {forbidden}: {deps:?}",
                binary.display()
            );
        }
    }
    if cfg!(target_os = "linux") && has_builtin_runtime() {
        const ALLOWED: &[&str] = &[
            "libc.so",
            "libm.so",
            "libgcc_s.so",
            "ld-linux",
            "libdl.so",
            "libpthread.so",
            "librt.so",
            "linux-vdso",
            "ld64.so",
        ];
        for dep in deps.iter().filter(|d| !d.is_empty()) {
            assert!(
                ALLOWED.iter().any(|a| dep.contains(a)),
                "unexpected dynamic dependency {dep} in {deps:?}: only the C/math/unwind runtime is allowed"
            );
        }
    }
}

/// Minimal ELF64 section-name lookup: enough to prove `.msbver` survived
/// linking (and stripping/LTO for a release artifact) without external tools.
#[cfg(target_os = "linux")]
fn elf_section(bytes: &[u8], wanted: &str) -> Option<Vec<u8>> {
    let u16_at = |o: usize| -> Option<usize> {
        Some(u16::from_le_bytes(bytes.get(o..o + 2)?.try_into().ok()?) as usize)
    };
    let u32_at = |o: usize| Some(u32::from_le_bytes(bytes.get(o..o + 4)?.try_into().ok()?));
    let u64_at = |o: usize| Some(u64::from_le_bytes(bytes.get(o..o + 8)?.try_into().ok()?));
    if bytes.get(..4)? != b"\x7fELF" || bytes[4] != 2 || bytes[5] != 1 {
        return None;
    }
    let shoff = u64_at(0x28)? as usize;
    let shentsize = u16_at(0x3A)?;
    let shnum = u16_at(0x3C)?;
    let shstrndx = u16_at(0x3E)?;
    let header = |i: usize| shoff + i * shentsize;
    let strtab = u64_at(header(shstrndx) + 0x18)? as usize;
    for i in 0..shnum {
        let name_off = u32_at(header(i))? as usize;
        let name_start = strtab + name_off;
        let name_end = bytes[name_start..].iter().position(|b| *b == 0)? + name_start;
        if &bytes[name_start..name_end] == wanted.as_bytes() {
            let off = u64_at(header(i) + 0x18)? as usize;
            let size = u64_at(header(i) + 0x20)? as usize;
            return Some(bytes.get(off..off + size)?.to_vec());
        }
    }
    None
}

#[test]
fn builtin_binary_carries_the_msb_version_section_the_sdk_reads_without_executing_it() {
    if skip_without_runtime("msbver section") {
        return;
    }
    let binary = artifact();
    let bytes = std::fs::read(&binary).unwrap();
    #[cfg(target_os = "linux")]
    assert_eq!(
        elf_section(&bytes, ".msbver").as_deref(),
        Some(&b"0.7.2"[..]),
        "{} lost its .msbver section (LTO/strip must retain it)",
        binary.display()
    );
    #[cfg(target_os = "macos")]
    assert!(
        bytes.windows(8).any(|w| w == b"__msbver"),
        "{} has no __TEXT,__msbver section",
        binary.display()
    );
    #[cfg(awman_builtin)]
    assert_eq!(
        microsandbox::setup::resolve_runtime_version(&binary)
            .unwrap()
            .map(|version| version.to_string())
            .as_deref(),
        Some("0.7.2"),
        "SDK could not read the version section in {}",
        binary.display()
    );
}

#[test]
fn builtin_final_artifact_retains_worker_provider() {
    if !matches!(
        std::env::var("AWMAN_TEST_DISTRIBUTION").as_deref(),
        Ok("1" | "true" | "yes" | "on")
    ) {
        eprintln!("SKIP: final optimized artifact requires AWMAN_TEST_DISTRIBUTION=1");
        return;
    }
    let path = std::env::var_os("AWMAN_TEST_BUILTIN_ARTIFACT")
        .map(PathBuf::from)
        .expect("AWMAN_TEST_BUILTIN_ARTIFACT must name the final optimized/stripped artifact");
    assert!(path.is_file(), "final artifact must exist");
    let output = Command::new(&path)
        .arg("__awman-builtin-info")
        .env_clear()
        .output()
        .expect("run exact final artifact");
    assert!(
        output.status.success(),
        "final artifact private info failed"
    );
    let info: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(info["embedded_kernel"], true);
    assert_eq!(info["host_helpers"], false);
    let bytes = std::fs::read(&path).unwrap();
    #[cfg(target_os = "linux")]
    assert_eq!(
        elf_section(&bytes, ".msbver").as_deref(),
        Some(&b"0.7.2"[..])
    );
    #[cfg(target_os = "macos")]
    assert!(bytes.windows(8).any(|w| w == b"__msbver"));
    #[cfg(awman_builtin)]
    assert_eq!(
        microsandbox::setup::resolve_runtime_version(&path)
            .unwrap()
            .map(|version| version.to_string())
            .as_deref(),
        Some("0.7.2")
    );
}

/// Files that would mean a helper, firmware or runtime bundle was unpacked.
fn looks_like_extracted_runtime(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let lower = name.to_ascii_lowercase();
    if [
        "msb",
        "agentd",
        "libkrun",
        "libkrunfw",
        "krunfw",
        "kernel",
        "microsandbox",
    ]
    .iter()
    .any(|needle| {
        lower == *needle
            || lower.starts_with(&format!("{needle}."))
            || lower.starts_with(&format!("{needle}-"))
    }) {
        return true;
    }
    if let Ok(mut file) = std::fs::File::open(path) {
        use std::io::Read as _;
        let mut magic = [0u8; 4];
        if file.read_exact(&mut magic).is_ok() {
            // ELF or a 64-bit Mach-O: an executable/library payload.
            return &magic == b"\x7fELF" || magic == [0xcf, 0xfa, 0xed, 0xfe];
        }
    }
    false
}

#[test]
fn builtin_no_helper_is_extracted_by_the_binary_before_any_guest_starts() {
    let scratch = Scratch::new();
    for args in [
        vec!["--version"],
        vec!["__awman-builtin-info"],
        vec!["--help"],
    ] {
        let _ = scratch
            .command(&awman(), Some(PATHS))
            .args(&args)
            .output()
            .unwrap();
    }
    let extracted: Vec<_> = scratch
        .entries()
        .into_iter()
        .filter(|p| {
            scratch.dir.path().join(p).is_file()
                && looks_like_extracted_runtime(&scratch.dir.path().join(p))
        })
        .collect();
    assert!(
        extracted.is_empty(),
        "helpers/firmware appeared on disk: {extracted:?}"
    );
}

/// `strace` (when usable) proves the binary never opens an external `msb`,
/// firmware or helper file while answering the internal routes.
#[cfg(target_os = "linux")]
#[test]
fn builtin_trace_shows_no_external_executable_or_firmware_access() {
    let probe = Command::new("strace").arg("-V").output();
    if probe.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP: strace is not installed");
        return;
    }
    let scratch = Scratch::new();
    let log = scratch.dir.path().join("trace.log");
    let status = scratch
        .command(Path::new("strace"), Some(PATHS))
        .args([
            "-f",
            "-qq",
            "-e",
            "trace=openat,open,execve,access,stat,newfstatat",
            "-o",
        ])
        .arg(&log)
        .arg(awman())
        .arg(if has_builtin_runtime() {
            "__awman-builtin-info"
        } else {
            "--version"
        })
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    let trace = std::fs::read_to_string(&log).unwrap_or_default();
    if !status.success() && !trace.contains("execve") {
        eprintln!("SKIP: ptrace is not permitted in this environment");
        return;
    }
    let exe = awman().to_string_lossy().into_owned();
    for line in trace.lines() {
        if line.contains("ENOENT") {
            continue;
        }
        let names_runtime = [
            "libkrunfw",
            "libkrun.",
            "/msb\"",
            "agentd",
            "microsandbox/bin",
            ".msb/",
        ]
        .iter()
        .any(|needle| line.contains(needle));
        assert!(
            !names_runtime,
            "external runtime component accessed: {line}"
        );
        if line.contains("execve(") {
            assert!(
                line.contains(&exe),
                "only awman itself may be executed: {line}"
            );
        }
    }
}

/// glibc's loader can narrate every object it maps (`LD_DEBUG=files`), which
/// needs no tracer: a `dlopen`ed firmware or helper library would show up here.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
fn builtin_loader_trace_maps_only_system_libraries_and_no_firmware() {
    let scratch = Scratch::new();
    let output = scratch
        .command(&awman(), Some(PATHS))
        .env("LD_DEBUG", "files")
        .arg(if has_builtin_runtime() {
            "__awman-builtin-info"
        } else {
            "--version"
        })
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    let trace = String::from_utf8_lossy(&output.stderr);
    let initialised: Vec<&str> = trace
        .lines()
        .filter_map(|l| l.split("calling init:").nth(1))
        .map(str::trim)
        .collect();
    assert!(
        !initialised.is_empty(),
        "the loader trace was empty: {trace}"
    );
    const ALLOWED: &[&str] = &[
        "ld-linux",
        "ld64.so",
        "libc.so",
        "libm.so",
        "libgcc_s.so",
        "libdl.so",
        "libpthread.so",
        "librt.so",
        "linux-vdso",
    ];
    for object in initialised {
        let name = Path::new(object)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(object);
        assert!(
            ALLOWED.iter().any(|allowed| name.contains(allowed)),
            "the loader mapped {object}: firmware, helpers and SQLite must not be shared objects"
        );
        assert!(
            !["krun", "msb", "sqlite", "cap-ng", "agentd"]
                .iter()
                .any(|bad| name.contains(bad)),
            "{object}"
        );
    }
    // No dlopen of anything after start-up either (nothing beyond the NEEDED set).
    assert!(
        !trace.contains("libkrunfw"),
        "the kernel firmware must never be loaded as a library"
    );
}
