//! [`TestSandbox`]: a private root for one test.

use crate::capability::{self, Capability, Granted, ProbeContext};
use crate::env::EnvMap;
use crate::evidence::{self, FailureRecord, ProcessRecord};
use crate::fakes::FakeCli;
use crate::net::{self, Ports};
use crate::process::{ChildCore, OwnedCommand};
use crate::pty::PtyCommand;
use crate::tier::{TestCase, Tier};
use crate::Leak;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Agent / code-assistant executables that must never run on the host
/// (CLAUDE.md security constraint, hygiene rule H10). A fake installed under
/// one of these names inside the sandbox is allowed; nothing else is.
pub const AGENT_NAMES: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "opencode",
    "aider",
    "copilot",
    "cursor-agent",
    "goose",
    "amp",
    "crush",
    "qwen",
    "kiro",
    "kiro-cli",
    "droid",
    "cline",
    "q",
];

/// Programs that run other programs from their arguments; their arguments
/// are checked for agent names too.
const INTERPRETERS: &[&str] = &[
    "sh",
    "bash",
    "dash",
    "zsh",
    "ksh",
    "fish",
    "env",
    "xargs",
    "nohup",
    "timeout",
    "nice",
    "python",
    "python3",
    "perl",
    "ruby",
    "node",
    "cmd",
    "powershell",
    "pwsh",
    "script",
    "busybox",
    "stdbuf",
    "time",
];

/// Never allowed: privilege escalation, and the host keychain and clipboard
/// tools (a private HOME does not isolate them).
const FORBIDDEN: &[&str] = &[
    "sudo",
    "doas",
    "su",
    "pkexec",
    "runas",
    "security",
    "secret-tool",
    "pbcopy",
    "pbpaste",
    "xclip",
    "xsel",
    "wl-copy",
    "wl-paste",
    "cmdkey",
];

/// An end-of-test leak probe ([`TestSandbox::on_finish`]).
pub type LeakProbe = Box<dyn Fn() -> Vec<Leak> + Send>;

/// State shared between the sandbox, its commands, children and ports.
pub(crate) struct Shared {
    pub tier: Tier,
    pub test_path: String,
    pub root: PathBuf,
    next_process: AtomicUsize,
    children: Mutex<Vec<Arc<ChildCore>>>,
    /// Canonical paths of executables registered or installed by the test.
    allowed: Mutex<BTreeSet<PathBuf>>,
    fakes: Mutex<BTreeSet<PathBuf>>,
    pub exes: Mutex<BTreeMap<String, PathBuf>>,
    pub tcp: Mutex<Vec<SocketAddr>>,
    pub sockets: Mutex<Vec<PathBuf>>,
    pub(crate) probes: Mutex<Vec<(String, LeakProbe)>>,
    granted: Mutex<Vec<(&'static str, capability::Reprobe)>>,
    notes: Mutex<BTreeMap<String, String>>,
    pub fixtures: Mutex<BTreeMap<String, String>>,
    redactions: Mutex<Vec<String>>,
}

impl Shared {
    pub fn check_may_spawn(&self) {
        if !self.tier.may_spawn() {
            panic!(
                "MISPLACED[subprocess]: tier unit may not start processes; \
                 use TestSandbox::for_test with a component-or-higher TestCase"
            );
        }
    }

    pub fn check_may_use_network(&self) {
        if !self.tier.may_spawn() {
            panic!("MISPLACED[loopback]: tier unit may not open sockets");
        }
    }

    pub fn next_process_index(&self) -> usize {
        self.next_process.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn register_child(&self, core: Arc<ChildCore>) {
        self.children.lock().unwrap().push(core);
    }

    pub fn allow(&self, path: &Path) {
        let canonical = std::fs::canonicalize(path)
            .unwrap_or_else(|e| panic!("cannot allow {}: {e}", path.display()));
        self.allowed.lock().unwrap().insert(canonical);
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for secret in self.redactions.lock().unwrap().iter() {
            if !secret.is_empty() {
                out = out.replace(secret.as_str(), "<redacted>");
            }
        }
        out
    }

    /// Refuse anything that is not a registered/installed executable or an
    /// ordinary system tool, and anything that would run an agent on the host.
    pub fn check_program(&self, program: &Path, args: &[OsString]) {
        let canonical = std::fs::canonicalize(program)
            .unwrap_or_else(|e| panic!("cannot start {}: {e}", program.display()));
        if self.fakes.lock().unwrap().contains(&canonical) {
            return;
        }
        let stem = canonical
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let invoked = program
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if FORBIDDEN.contains(&stem.as_str()) || FORBIDDEN.contains(&invoked.as_str()) {
            panic!("REFUSED: tests never escalate privileges or touch the host keychain/clipboard ({})", program.display());
        }
        if AGENT_NAMES.contains(&stem.as_str()) || AGENT_NAMES.contains(&invoked.as_str()) {
            panic!(
                "REFUSED: `{}` is a code agent; agents never run on the host. \
                 Install a fake with TestSandbox::install_fake or run it inside a runtime.",
                program.display()
            );
        }
        if INTERPRETERS.contains(&stem.as_str()) || INTERPRETERS.contains(&invoked.as_str()) {
            for arg in args {
                let text = arg.to_string_lossy();
                for token in text.split(|c: char| !(c.is_alphanumeric() || "._-/\\".contains(c))) {
                    let base = token
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(token)
                        .to_lowercase();
                    let base = base.strip_suffix(".exe").unwrap_or(&base);
                    if (AGENT_NAMES.contains(&base) && base.len() > 1) || FORBIDDEN.contains(&base)
                    {
                        panic!(
                            "REFUSED: `{}` would run `{base}` (an agent or host keychain/clipboard/privilege tool) through an interpreter",
                            program.display()
                        );
                    }
                }
            }
        }
        if self.allowed.lock().unwrap().contains(&canonical) {
            return;
        }
        if canonical.starts_with(canonical_root(&self.root)) {
            panic!(
                "REFUSED: {} was not installed with exe::install / install_fake; \
                 tests never write an executable and run it",
                program.display()
            );
        }
        let in_system_dir = canonical
            .parent()
            .is_some_and(|dir| system_tool_dirs().iter().any(|d| d == dir));
        if !in_system_dir {
            panic!(
                "REFUSED: {} is not on the sandbox tool allowlist (system directories {:?}); \
                 register it with TestSandbox::register_exe",
                program.display(),
                system_tool_dirs()
            );
        }
    }
}

fn canonical_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

/// The system directories a child may run tools from (canonicalized).
pub(crate) fn system_tool_dirs() -> &'static [PathBuf] {
    static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs: Vec<PathBuf> = system_path_dirs().to_vec();
        if cfg!(unix) {
            for extra in [
                "/usr/sbin",
                "/sbin",
                "/usr/local/bin",
                "/opt/homebrew/bin",
                "/usr/lib/git-core",
                "/usr/libexec/git-core",
            ] {
                dirs.push(PathBuf::from(extra));
            }
        }
        let mut canonical: Vec<PathBuf> = dirs
            .iter()
            .filter_map(|d| std::fs::canonicalize(d).ok())
            .collect();
        canonical.sort();
        canonical.dedup();
        canonical
    })
}

/// The fixed `PATH` of every child after `<sandbox>/bin`.
pub(crate) fn system_path_dirs() -> &'static [PathBuf] {
    static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs = Vec::new();
        if cfg!(windows) {
            if let Some(root) = &crate::runtime_env::get().system_root {
                dirs.push(root.join("System32"));
            }
        } else {
            dirs.push(PathBuf::from("/usr/bin"));
            dirs.push(PathBuf::from("/bin"));
            // BSD installs administrative filesystem tools such as chown here.
            // These are fixed system locations, never the operator's PATH.
            dirs.push(PathBuf::from("/usr/sbin"));
            dirs.push(PathBuf::from("/sbin"));
        }
        if let Some(git) = capability::find_git() {
            if let Some(dir) = git.parent() {
                if !dirs.iter().any(|d| d == dir) {
                    dirs.push(dir.to_path_buf());
                }
            }
        }
        dirs
    })
}

/// A private root for one test: HOME, XDG directories, awman data root, temp
/// dir, a `bin` directory first on `PATH`, a work directory, a short socket
/// directory, and the complete child environment.
///
/// Dropping the sandbox (or calling [`TestSandbox::finish`]) kills and reaps
/// every owned process, runs the leak check, writes failure evidence when the
/// test failed, and removes the root when it passed.
pub struct TestSandbox {
    shared: Arc<Shared>,
    env: EnvMap,
    root_dir: Option<tempfile::TempDir>,
    sock_short: Option<tempfile::TempDir>,
    sock_dir: PathBuf,
    ports: Ports,
    finished: bool,
}

impl std::fmt::Debug for TestSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestSandbox")
            .field("test", &self.shared.test_path)
            .field("tier", &self.shared.tier)
            .field("root", &self.shared.root)
            .finish()
    }
}

impl TestSandbox {
    /// A unit-tier sandbox (files only: no processes, sockets or real
    /// resources). Under `AWMAN_TEST_TMPROOT` when set, else the system temp
    /// dir. Panics `BLOCKED[tempdir]: …` if it cannot be made.
    pub fn new() -> TestSandbox {
        let path = std::thread::current().name().unwrap_or("unit").to_string();
        TestSandbox::create(Tier::Unit, &path, None)
    }

    /// Like [`TestSandbox::new`]; `label` names the test in evidence.
    pub fn named(label: &str) -> TestSandbox {
        TestSandbox::create(Tier::Unit, label, None)
    }

    /// A sandbox for an explicitly identified test of any tier.
    ///
    /// When the runner profile states a tier (`AWMAN_TEST_TIER`) that differs
    /// from `case.tier`, this panics `MISPLACED[tier]`: a selected run may
    /// validate a test's tier, never change it.
    pub fn for_test(case: TestCase) -> TestSandbox {
        TestSandbox::for_test_in(case, None)
    }

    /// Like [`TestSandbox::for_test`], under an explicit parent directory
    /// instead of `AWMAN_TEST_TMPROOT` / the system temp dir.
    pub fn for_test_in(case: TestCase, parent: Option<&Path>) -> TestSandbox {
        if let Some(selected) = &crate::runtime_env::get().tier {
            if selected != case.tier.name() {
                panic!(
                    "MISPLACED[tier]: {} is a {} test but this run selected tier {selected}",
                    case.id, case.tier
                );
            }
        }
        TestSandbox::create(case.tier, case.id, parent)
    }

    fn create(tier: Tier, test_path: &str, parent: Option<&Path>) -> TestSandbox {
        evidence::install_panic_hook();
        crate::config::run_config().ensure_usable();
        let runtime = crate::runtime_env::get();
        let parent = parent
            .map(Path::to_path_buf)
            .or_else(|| runtime.tmproot.clone())
            .unwrap_or_else(|| runtime.system_temp.clone());
        if let Err(e) = std::fs::create_dir_all(&parent) {
            panic!("BLOCKED[tempdir]: cannot create {}: {e}", parent.display());
        }
        let root_dir = tempfile::Builder::new()
            .prefix("awman-test-")
            .tempdir_in(&parent)
            .unwrap_or_else(|e| {
                panic!(
                    "BLOCKED[tempdir]: cannot create a sandbox in {}: {e}",
                    parent.display()
                )
            });
        let root = root_dir.path().to_path_buf();
        let home = root.join("home");
        let dirs = [
            home.clone(),
            home.join(".config"),
            home.join(".local/share"),
            home.join(".local/state"),
            home.join(".cache"),
            home.join(".awman"),
            root.join("tmp"),
            root.join("bin"),
            root.join("work"),
            root.join("proc"),
            root.join("no-hooks"),
        ];
        for dir in &dirs {
            std::fs::create_dir_all(dir)
                .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: {}: {e}", dir.display()));
        }
        let (sock_dir, sock_short) = make_sock_dir(&root);
        std::fs::write(root.join("gitconfig"), git_config(&root))
            .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: gitconfig: {e}"));

        let shared = Arc::new(Shared {
            tier,
            test_path: test_path.to_string(),
            root: root.clone(),
            next_process: AtomicUsize::new(0),
            children: Mutex::new(Vec::new()),
            allowed: Mutex::new(BTreeSet::new()),
            fakes: Mutex::new(BTreeSet::new()),
            exes: Mutex::new(BTreeMap::new()),
            tcp: Mutex::new(Vec::new()),
            sockets: Mutex::new(Vec::new()),
            probes: Mutex::new(Vec::new()),
            granted: Mutex::new(Vec::new()),
            notes: Mutex::new(BTreeMap::new()),
            fixtures: Mutex::new(BTreeMap::new()),
            redactions: Mutex::new(Vec::new()),
        });
        let env = default_env(&root);
        let ports = Ports::new(shared.clone(), sock_dir.clone());
        TestSandbox {
            shared,
            env,
            root_dir: Some(root_dir),
            sock_short,
            sock_dir,
            ports,
            finished: false,
        }
    }

    /// The test's tier.
    pub fn tier(&self) -> Tier {
        self.shared.tier
    }

    /// `"<package>::<binary> <test path>"`.
    pub fn test_id(&self) -> String {
        evidence::format_test_id(&self.shared.test_path)
    }

    /// The sandbox root.
    pub fn root(&self) -> &Path {
        &self.shared.root
    }

    /// `HOME` (and `USERPROFILE`).
    pub fn home(&self) -> PathBuf {
        self.shared.root.join("home")
    }

    /// `XDG_CONFIG_HOME`.
    pub fn xdg_config(&self) -> PathBuf {
        self.home().join(".config")
    }

    /// `XDG_DATA_HOME`.
    pub fn xdg_data(&self) -> PathBuf {
        self.home().join(".local/share")
    }

    /// `AWMAN_HOME` (`$HOME/.awman`).
    pub fn awman_home(&self) -> PathBuf {
        self.home().join(".awman")
    }

    /// `TMPDIR` / `TMP` / `TEMP`.
    pub fn tmp(&self) -> PathBuf {
        self.shared.root.join("tmp")
    }

    /// First `PATH` entry; fakes and installed executables go here.
    pub fn bin(&self) -> PathBuf {
        self.shared.root.join("bin")
    }

    /// Default working directory of children.
    pub fn work(&self) -> PathBuf {
        self.shared.root.join("work")
    }

    /// A directory whose absolute path is short enough for `sockaddr_un`:
    /// `/tmp/aw-<random>` (0700, owner-checked) when the root is too long,
    /// else `<root>/s`. Windows: `<root>\s`.
    pub fn sock_dir(&self) -> &Path {
        &self.sock_dir
    }

    /// The complete child environment.
    pub fn env(&self) -> &EnvMap {
        &self.env
    }

    /// Change the child environment for every later command.
    pub fn env_mut(&mut self) -> &mut EnvMap {
        &mut self.env
    }

    /// Start building a supervised process (the only way to start one in
    /// tests). Panics `MISPLACED` in a unit-tier sandbox.
    pub fn command(&self, program: impl AsRef<OsStr>) -> OwnedCommand {
        self.shared.check_may_spawn();
        OwnedCommand::new(
            self.shared.clone(),
            program.as_ref(),
            self.env.clone(),
            self.work(),
        )
    }

    /// Like [`TestSandbox::command`] under a pseudo-terminal.
    pub fn pty_command(&self, program: impl AsRef<OsStr>) -> PtyCommand {
        self.shared.check_may_spawn();
        PtyCommand::new(
            self.shared.clone(),
            program.as_ref(),
            self.env.clone(),
            self.work(),
        )
    }

    /// Owned ports and Unix sockets. Panics `MISPLACED` in a unit-tier sandbox.
    pub fn ports(&self) -> &Ports {
        self.shared.check_may_use_network();
        &self.ports
    }

    /// Register an executable built with the tests (for example
    /// `("awman", cargo_bin!("awman"))`): it is allowed to run and
    /// capabilities such as `AwmanBinary` find it by `name`.
    pub fn register_exe(&mut self, name: &str, path: &Path) {
        if path.exists() {
            self.shared.allow(path);
        }
        self.shared
            .exes
            .lock()
            .unwrap()
            .insert(name.to_string(), path.to_path_buf());
    }

    /// Allow one extra tool outside the system directories (for example the
    /// Docker CLI a capability resolved).
    pub fn allow_tool(&mut self, path: &Path) {
        self.shared.allow(path);
    }

    pub(crate) fn allow_exe_path(&self, path: &Path) {
        self.shared.allow(path);
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Install `fake_cli_exe` (the `*-fake-cli` binary) as `<bin>/<name>`
    /// and return its scenario handle.
    pub fn install_fake(&self, fake_cli_exe: &Path, name: &str) -> FakeCli {
        let fake = FakeCli::install(self, fake_cli_exe, name);
        self.shared
            .fakes
            .lock()
            .unwrap()
            .insert(std::fs::canonicalize(fake.exe()).expect("installed fake"));
        self.shared.allow(fake.exe());
        fake
    }

    /// Install an executable atomically under `<bin>/<name>` (see
    /// [`crate::exe::install`]) and allow it to run.
    pub fn install_exe(&self, src: &Path, name: &str) -> PathBuf {
        let installed = crate::exe::install_as(src, &self.bin(), name);
        self.shared.allow(&installed);
        installed
    }

    /// Probe a capability for this test (see [`crate::capability`]).
    ///
    /// - guaranteed or allowed by the tier and present → `Granted`;
    /// - guaranteed/allowed but absent → panic `BLOCKED[<id>]: <reason>`;
    /// - not available to the tier → panic `MISPLACED[<id>]: …`.
    pub fn require<C: Capability>(&self) -> Granted<C> {
        let granted = capability::require::<C>(self.shared.tier, &self.probe_context());
        self.shared
            .granted
            .lock()
            .unwrap()
            .push((C::ID, capability::reprobe::<C>));
        granted
    }

    pub(crate) fn probe_context(&self) -> ProbeContext {
        ProbeContext::new(
            self.shared.root.clone(),
            self.shared.exes.lock().unwrap().clone(),
        )
    }

    /// Add what a granted capability needs to the child environment (for
    /// example `AWMAN_TEST_DOCKER=1`, `DOCKER_HOST`, the CLI directory on
    /// `PATH`), and allow its tools to run.
    pub fn grant<C: Capability>(&mut self, cap: &Granted<C>) {
        cap.grant_env(&mut self.env);
        for tool in cap.tools() {
            if tool.exists() {
                self.shared.allow(&tool);
            }
        }
    }

    /// An extra leak probe run at the end (VMs, images, containers).
    pub fn on_finish(&self, name: &str, probe: LeakProbe) {
        self.shared
            .probes
            .lock()
            .unwrap()
            .push((name.to_string(), probe));
    }

    /// Add a field to this test's failure record (any thread).
    pub fn note(&self, key: &str, value: &str) {
        self.shared
            .notes
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
    }

    /// Replace `secret` with `<redacted>` in diagnostics and evidence.
    pub fn redact(&self, secret: &str) {
        self.shared
            .redactions
            .lock()
            .unwrap()
            .push(secret.to_string());
    }

    pub(crate) fn record_fixture(&self, id: &str, sha256: &str) {
        self.shared
            .fixtures
            .lock()
            .unwrap()
            .insert(id.to_string(), sha256.to_string());
    }

    /// Kill and reap everything owned, run the leak check, write evidence,
    /// remove the root. A leak panics unless the thread is already panicking
    /// (then it is added to the failure record).
    pub fn finish(mut self) {
        self.close();
    }

    fn close(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let panicking = std::thread::panicking();
        let mut leaks = Vec::new();
        let children: Vec<Arc<ChildCore>> = self.shared.children.lock().unwrap().clone();
        for child in &children {
            leaks.extend(child.leak_check());
        }
        leaks.extend(crate::sys::escaped_processes(&self.shared.root));
        leaks.extend(net::leak_check(&self.shared, &self.sock_dir));
        let probes = std::mem::take(&mut *self.shared.probes.lock().unwrap());
        for (name, probe) in probes {
            for mut leak in probe() {
                leak.detail = format!("{} (probe {name})", leak.detail);
                leaks.push(leak);
            }
        }
        if let Some(short) = self.sock_short.take() {
            if let Err(e) = short.close() {
                leaks.push(Leak::new("tempdir", format!("socket dir: {e}")));
            }
        }
        let mut thread_notes = evidence::take_thread_notes();
        if !panicking && leaks.is_empty() {
            if let Some(dir) = self.root_dir.take() {
                let path = dir.path().to_path_buf();
                if let Err(e) = dir.close() {
                    panic!(
                        "LEAK: tempdir: {} could not be removed: {e}",
                        path.display()
                    );
                }
            }
            return;
        }
        // Failure: keep the root and write the record.
        let root = self
            .root_dir
            .take()
            .map(|d| d.keep())
            .unwrap_or_else(|| self.shared.root.clone());
        thread_notes.extend(self.shared.notes.lock().unwrap().clone());
        let lost = if panicking {
            self.lost_capabilities()
        } else {
            Vec::new()
        };
        let mut record = FailureRecord {
            schema: 1,
            test: self.test_id(),
            tier: self.shared.tier.name().to_string(),
            seed: thread_notes.remove("seed"),
            target: evidence::target().to_string(),
            toolchain: evidence::toolchain().to_string(),
            fixtures: self.shared.fixtures.lock().unwrap().clone(),
            sandbox: root.display().to_string(),
            processes: children
                .iter()
                .map(|c| ProcessRecord {
                    label: c.label.clone(),
                    pid: c.pid,
                    pgid: c.group_id(),
                    exit: c
                        .exit()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "running".into()),
                    stdout: c.stdout_path.display().to_string(),
                    stderr: c.stderr_path.display().to_string(),
                })
                .collect(),
            vms: thread_notes.remove("vm").into_iter().collect(),
            capabilities: self
                .shared
                .granted
                .lock()
                .unwrap()
                .iter()
                .map(|(id, _)| id.to_string())
                .collect(),
            capabilities_lost: lost,
            leaks: leaks.clone(),
            panic: if panicking {
                evidence::last_panic().map(|p| self.shared.redact(&p))
            } else {
                None
            },
            reproduce: evidence::reproduce(&self.shared.test_path),
            notes: thread_notes,
        };
        let evidence_dir = crate::runtime_env::get().evidence_dir.clone();
        if evidence_dir.is_some() {
            let _ = evidence::write_record(&mut record, &root, None);
        }
        match evidence::write_record(&mut record, &root, evidence_dir.as_deref()) {
            Ok(path) => eprintln!(
                "awman-test-support: failure record {} (sandbox kept at {})",
                path.display(),
                root.display()
            ),
            Err(e) => eprintln!("awman-test-support: could not write the failure record: {e}"),
        }
        if !panicking {
            let list: Vec<String> = leaks.iter().map(|l| l.to_string()).collect();
            panic!(
                "LEAK: the test left resources behind:\n  {}",
                list.join("\n  ")
            );
        }
    }

    fn lost_capabilities(&self) -> Vec<String> {
        let ctx = self.probe_context();
        self.shared
            .granted
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, reprobe)| reprobe(&ctx).is_err())
            .map(|(id, _)| id.to_string())
            .collect()
    }
}

impl Default for TestSandbox {
    fn default() -> Self {
        TestSandbox::new()
    }
}

impl Drop for TestSandbox {
    fn drop(&mut self) {
        self.close();
    }
}

fn make_sock_dir(root: &Path) -> (PathBuf, Option<tempfile::TempDir>) {
    let local = root.join("s");
    // Room for "/<name>" with names up to 24 bytes.
    if cfg!(windows) || local.as_os_str().len() + 25 <= crate::net::MAX_SOCKET_PATH {
        std::fs::create_dir_all(&local)
            .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: {}: {e}", local.display()));
        return (local, None);
    }
    let parent = crate::runtime_env::get()
        .tmproot
        .clone()
        .unwrap_or_else(|| crate::runtime_env::get().system_temp.clone());
    let short = tempfile::Builder::new()
        .prefix("aw-")
        .rand_bytes(8)
        .tempdir_in(&parent)
        .unwrap_or_else(|e| {
            panic!(
                "BLOCKED[tempdir]: cannot create a short socket dir in {}: {e}",
                parent.display()
            )
        });
    assert!(short.path().as_os_str().len() + 25 <= crate::net::MAX_SOCKET_PATH,
        "BLOCKED[tempdir]: configured socket root {} is too long; grant a shorter private AWMAN_TEST_TMPROOT", parent.display());
    make_private(short.path());
    check_private_dir(short.path());
    (short.path().to_path_buf(), Some(short))
}

#[cfg(unix)]
fn make_private(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: chmod {}: {e}", dir.display()));
}

#[cfg(not(unix))]
fn make_private(_dir: &Path) {}

#[cfg(unix)]
fn check_private_dir(dir: &Path) {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(dir)
        .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: {}: {e}", dir.display()));
    let me = nix::unistd::geteuid().as_raw();
    if !meta.is_dir() || meta.uid() != me || meta.mode() & 0o077 != 0 {
        panic!(
            "BLOCKED[tempdir]: socket dir {} is not a private directory owned by uid {me}",
            dir.display()
        );
    }
}

#[cfg(not(unix))]
fn check_private_dir(_dir: &Path) {}

/// The global git configuration of every child: fixed identity, no hooks,
/// no credential helpers, no signing, no fsmonitor.
fn git_config(root: &Path) -> String {
    let hooks = root.join("no-hooks");
    format!(
        "[user]\n\tname = awman test\n\temail = test@awman.invalid\n\
         [init]\n\tdefaultBranch = main\n\
         [core]\n\thooksPath = {}\n\tfsmonitor = false\n\tautocrlf = false\n\
         [commit]\n\tgpgsign = false\n[tag]\n\tgpgsign = false\n\
         [credential]\n\thelper =\n\
         [protocol \"ext\"]\n\tallow = never\n\
         [advice]\n\tdetachedHead = false\n",
        hooks.display().to_string().replace('\\', "/")
    )
}

/// The default child environment, built from nothing.
fn default_env(root: &Path) -> EnvMap {
    let home = root.join("home");
    let tmp = root.join("tmp");
    let mut env = EnvMap::new();
    env.set("HOME", &home)
        .set("XDG_CONFIG_HOME", home.join(".config"))
        .set("XDG_DATA_HOME", home.join(".local/share"))
        .set("XDG_STATE_HOME", home.join(".local/state"))
        .set("XDG_CACHE_HOME", home.join(".cache"))
        .set("AWMAN_HOME", home.join(".awman"))
        .set("TMPDIR", &tmp)
        .set("TMP", &tmp)
        .set("TEMP", &tmp)
        .set("AWMAN_TEST_ISOLATION", "1")
        .set("GIT_CONFIG_GLOBAL", root.join("gitconfig"))
        .set("GIT_CONFIG_NOSYSTEM", "1")
        .set("GIT_TERMINAL_PROMPT", "0")
        .set("TZ", "UTC")
        .set("LC_ALL", "C.UTF-8");
    let path = std::env::join_paths(
        std::iter::once(root.join("bin")).chain(system_path_dirs().iter().cloned()),
    )
    .expect("PATH entries");
    env.set("PATH", path);
    if cfg!(windows) {
        env.set("USERPROFILE", &home)
            .set("APPDATA", home.join("AppData/Roaming"))
            .set("LOCALAPPDATA", home.join("AppData/Local"))
            .set("PATHEXT", ".COM;.EXE;.BAT;.CMD");
        if let Some(system_root) = &crate::runtime_env::get().system_root {
            env.set("SystemRoot", system_root);
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_sandbox_has_the_layout_and_cleans_up() {
        let sandbox = TestSandbox::new();
        let root = sandbox.root().to_path_buf();
        for dir in [
            sandbox.home(),
            sandbox.xdg_config(),
            sandbox.xdg_data(),
            sandbox.awman_home(),
            sandbox.tmp(),
            sandbox.bin(),
            sandbox.work(),
        ] {
            assert!(dir.is_dir(), "{}", dir.display());
            assert!(dir.starts_with(&root));
        }
        assert_eq!(sandbox.tier(), Tier::Unit);
        sandbox.finish();
        assert!(!root.exists());
    }

    #[test]
    fn default_env_is_complete_and_private() {
        let sandbox = TestSandbox::named("env");
        let env = sandbox.env();
        assert_eq!(env.get("HOME"), Some(sandbox.home().as_os_str()));
        assert_eq!(
            env.get("AWMAN_HOME"),
            Some(sandbox.awman_home().as_os_str())
        );
        assert_eq!(env.get_str("AWMAN_TEST_ISOLATION"), Some("1"));
        assert_eq!(env.get_str("GIT_CONFIG_NOSYSTEM"), Some("1"));
        assert_eq!(env.path_dirs()[0], sandbox.bin());
        for forbidden in [
            "GITHUB_TOKEN",
            "HTTPS_PROXY",
            "http_proxy",
            "SSH_AUTH_SOCK",
            "DOCKER_HOST",
            "AWMAN_TEST_DOCKER",
        ] {
            assert!(env.get(forbidden).is_none(), "{forbidden}");
        }
    }

    #[test]
    fn unit_sandbox_refuses_processes_and_sockets() {
        let sandbox = TestSandbox::new();
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(sandbox.command("git"))
        }))
        .unwrap_err();
        assert!(crate::panic_text(&err).starts_with("MISPLACED[subprocess]"));
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sandbox.ports();
        }))
        .unwrap_err();
        assert!(crate::panic_text(&err).starts_with("MISPLACED[loopback]"));
    }

    #[test]
    fn git_config_disables_hooks_helpers_and_signing() {
        let text = git_config(Path::new("/r"));
        assert!(text.contains("hooksPath = /r/no-hooks"));
        assert!(text.contains("helper =\n"));
        assert!(text.contains("gpgsign = false"));
        assert!(text.contains("fsmonitor = false"));
    }

    #[test]
    fn long_roots_get_a_short_private_socket_dir() {
        let parent = tempfile::tempdir().unwrap();
        let long = parent.path().join("x".repeat(90));
        std::fs::create_dir_all(&long).unwrap();
        let sandbox = TestSandbox::create(Tier::Component, "long_root", Some(&long));
        let sock = sandbox.sock_dir().to_path_buf();
        assert!(!sock.starts_with(sandbox.root()));
        assert!(
            sock.as_os_str().len() + 27 <= crate::net::MAX_SOCKET_PATH,
            "{}",
            sock.display()
        );
        let configured = crate::runtime_env::get()
            .tmproot
            .clone()
            .unwrap_or_else(|| crate::runtime_env::get().system_temp.clone());
        assert!(
            sock.starts_with(configured),
            "socket fallback must honor the configured private temp root"
        );
        sandbox.finish();
        assert!(!sock.exists());
    }

    #[test]
    fn short_roots_keep_sockets_inside() {
        let sandbox = TestSandbox::new();
        if sandbox.root().as_os_str().len() + 27 <= crate::net::MAX_SOCKET_PATH {
            assert_eq!(sandbox.sock_dir(), sandbox.root().join("s"));
        } else {
            assert!(!sandbox.sock_dir().starts_with(sandbox.root()));
        }
    }

    #[test]
    fn agent_programs_are_refused() {
        let sandbox = TestSandbox::create(Tier::Component, "agents", None);
        let fake_agent = sandbox.tmp().join("claude");
        std::fs::write(&fake_agent, "").unwrap();
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sandbox.shared.check_program(&fake_agent, &[]);
        }))
        .unwrap_err();
        assert!(crate::panic_text(&err).contains("is a code agent"));
    }
}
