//! Native checks without runners (design 3.4, 7.9, 7.10).
//!
//! The command verifies the attested kit, extracts it safely, and runs
//! `<kit>/bin/xtask test kit-run` either locally (`LocalHost`, the Apple
//! Silicon Mac) or over SSH (`SshHost`, the Linux KVM host). Neither a checkout
//! nor Cargo is used on either host. The SBX check uses only its dedicated sign-in
//! and test provider credential on the Mac.
//!
//! Before any host work an intent (`native/<id>/intent.json`, random
//! allocation ID and root) is written atomically; after the started process
//! confirms its identity it becomes a lease (`lease.json`): allocation, host
//! boot ID, PID and process start time, deadline. Resume adopts a durable
//! completed result first, adopts or cleans the allocation an intent names,
//! waits for authenticated live work, and only kills work whose identity it
//! can prove it owns. Owned processes that outlive a finished run void its
//! result.

use super::error::{Step, Stop};
use super::journal::NativeRecord;
use super::plan::{Download, NativePlan};
use super::util::{is_sha256, private_dir, random_id, read_json, sha256_file, write_json};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod evidence;

/// Longest Unix socket path the kernel accepts on macOS (sun_path 104 incl. NUL).
pub const SUN_PATH_MAX: usize = 103;
/// Bytes the artifact harness appends below `--tmproot` for its deepest
/// guest socket unless the kit declares `socket_suffix_bytes` (incident I1).
pub const DEFAULT_SOCKET_SUFFIX: usize = 64;
/// Largest expanded kit accepted (bytes).
pub const KIT_MAX_BYTES: u64 = 8 << 30;

pub struct NativeJob {
    pub plan: NativePlan,
    pub commit: String,
    pub tree: String,
    pub asset: PathBuf,
    pub asset_sha256: String,
    pub kit_tar: PathBuf,
    /// Hash of extracted kit.json, pinned when the tar attestation was verified.
    pub kit_sha256: String,
    pub manifest_sha256: String,
    pub plan_sha256: String,
    pub policy_sha256: String,
    /// The verified, extracted kit.
    pub kit_dir: PathBuf,
    /// Verified local copies of every download (local host only).
    pub downloads: Vec<(Download, PathBuf)>,
    /// Durable per-check state: lease, result, raw evidence.
    pub out_dir: PathBuf,
    pub deadline_seconds: u64,
}

pub trait NativeHost {
    fn cancel(&self, job: &NativeJob, world: &super::world::World) -> Step;
    fn describe(&self) -> String;
    /// Preflight: every capability this host needs, before long work.
    fn probe(&self, plan: &NativePlan) -> Step;
    /// Start (or adopt) the check. Idempotent: a completed durable result or
    /// authenticated live work is never restarted.
    fn start(&self, job: &NativeJob, world: &super::world::World) -> Step;
    /// `Some((durable result.json, kit-run exit))` once finished.
    fn poll(&self, job: &NativeJob, world: &super::world::World) -> Step<Option<(PathBuf, i32)>>;
}

/// The durable result already copied into the private state, if any.
pub fn durable(job: &NativeJob) -> Option<(PathBuf, i32)> {
    let result = job.out_dir.join("result.json");
    let exit = std::fs::read_to_string(job.out_dir.join("exit"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    result.exists().then_some((result, exit))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lease {
    pub allocation: String,
    pub host: String,
    pub boot_id: String,
    pub root: String,
    pub pid: u32,
    pub start: String,
    pub deadline: chrono::DateTime<chrono::Utc>,
}

// ── kit verification and safe extraction ─────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Kit {
    pub schema: u32,
    pub native_id: String,
    pub commit: String,
    pub tree: String,
    pub triple: String,
    pub manifest_sha256: String,
    pub plan_sha256: String,
    pub policy_sha256: String,
    pub files: BTreeMap<String, String>,
    pub fixtures: serde_json::Value,
    pub previous_release: Option<serde_json::Value>,
    #[serde(alias = "expected_tests")]
    pub tests: Vec<String>,
    #[serde(default)]
    pub socket_suffix_bytes: Option<usize>,
}

/// Extract `tar` into a fresh `dest`: only relative regular files and
/// directories, no links or devices, no duplicates, bounded size;
/// executable bits only under `bin/` and the compiled test payload.
pub fn safe_extract(tar_path: &Path, dest: &Path) -> Step {
    let bad = |m: String| Stop::new("kit-invalid", format!("{}: {m}", tar_path.display()));
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(|e| bad(e.to_string()))?;
    }
    private_dir(dest).map_err(|e| bad(format!("{e:#}")))?;
    let file = std::fs::File::open(tar_path).map_err(|e| bad(e.to_string()))?;
    let mut archive = tar::Archive::new(file);
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for entry in archive.entries().map_err(|e| bad(e.to_string()))? {
        let mut entry = entry.map_err(|e| bad(e.to_string()))?;
        let path = entry.path().map_err(|e| bad(e.to_string()))?.into_owned();
        let normal = path.components().all(|c| {
            matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        });
        if !normal || path.as_os_str().is_empty() {
            return Err(bad(format!("unsafe path {}", path.display())));
        }
        let rel: PathBuf = path
            .components()
            .filter(|c| !matches!(c, std::path::Component::CurDir))
            .collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        if !seen.insert(rel.clone()) {
            return Err(bad(format!("duplicate entry {}", rel.display())));
        }
        let kind = entry.header().entry_type();
        let target = dest.join(&rel);
        if kind.is_dir() {
            private_dir(&target).map_err(|e| bad(format!("{e:#}")))?;
            continue;
        }
        if !kind.is_file() {
            return Err(bad(format!("{} is not a regular file", rel.display())));
        }
        total = total
            .checked_add(entry.header().size().unwrap_or(u64::MAX))
            .ok_or_else(|| bad("expanded kit size overflows".into()))?;
        if total > KIT_MAX_BYTES {
            return Err(bad("expanded kit exceeds the size limit".into()));
        }
        if let Some(parent) = target.parent() {
            private_dir(parent).map_err(|e| bad(format!("{e:#}")))?;
        }
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| bad(format!("{}: {e}", rel.display())))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| bad(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let exec = (rel.starts_with("bin")
                || rel.starts_with("payload/target/debug/deps")
                || rel == Path::new("payload/target/debug/awman-fake-cli"))
                && entry.header().mode().unwrap_or(0) & 0o100 != 0;
            std::fs::set_permissions(
                &target,
                std::fs::Permissions::from_mode(if exec { 0o700 } else { 0o600 }),
            )
            .map_err(|e| bad(e.to_string()))?;
        }
    }
    Ok(())
}

/// Check the extracted kit: every file bound by `digests.json`, `kit.json`
/// for this check, commit and the plan's exact test selection.
pub fn check_kit(dir: &Path, plan: &NativePlan, commit: &str) -> Step<Kit> {
    let bad = |m: String| Stop::new("kit-invalid", format!("kit {}: {m}", plan.id));
    let digests: BTreeMap<String, String> =
        read_json(&dir.join("digests.json")).map_err(|e| bad(format!("{e:#}")))?;
    let mut files = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).map_err(|e| bad(e.to_string()))? {
            let p = e.map_err(|e| bad(e.to_string()))?.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if rel != "digests.json" {
                    files.insert(rel);
                }
            }
        }
    }
    let listed: BTreeSet<String> = digests.keys().cloned().collect();
    if files != listed {
        return Err(bad(format!(
            "digests.json does not bind exactly the kit files (unlisted {:?}, missing {:?})",
            files.difference(&listed).collect::<Vec<_>>(),
            listed.difference(&files).collect::<Vec<_>>()
        )));
    }
    for (rel, want) in &digests {
        if sha256_file(&dir.join(rel)).map_err(|e| bad(format!("{e:#}")))? != *want {
            return Err(bad(format!("{rel} does not match digests.json")));
        }
    }
    let kit: Kit = read_json(&dir.join("kit.json")).map_err(|e| bad(format!("{e:#}")))?;
    if kit.schema != 1 || kit.native_id != plan.id || kit.commit != commit {
        return Err(bad("kit.json names another check, commit or schema".into()));
    }
    let mut runtime = digests.clone();
    runtime.remove("kit.json");
    if kit.files != runtime || kit.triple != plan.triple {
        return Err(bad(
            "kit runtime files or target differ from the plan".into()
        ));
    }
    let fixtures = serde_json::json!(plan
        .fixtures
        .iter()
        .map(|f| serde_json::json!({
            "id": f.id, "file": f.file, "arch": f.arch, "sha256": f.sha256
        }))
        .collect::<Vec<_>>());
    let previous =
        plan.previous_release.as_ref().filter(|p| !p.is_null()).map(
            |p| serde_json::json!({"triple":plan.triple,"asset":p["asset"],"sha256":p["sha256"]}),
        );
    if kit.fixtures != fixtures || kit.previous_release != previous {
        return Err(bad(
            "kit fixture or predecessor pins differ from the plan".into()
        ));
    }
    let want: BTreeSet<_> = plan.tests.iter().collect();
    let have: BTreeSet<_> = kit.tests.iter().collect();
    if want != have || have.len() != kit.tests.len() {
        return Err(bad(
            "kit test selection differs from the release plan".into()
        ));
    }
    if !dir.join("bin/xtask").is_file() {
        return Err(bad("bin/xtask is missing".into()));
    }
    Ok(kit)
}

// ── result verification ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct NativeResult {
    pub schema: u32,
    pub native_id: String,
    pub commit: String,
    pub tree: String,
    pub artifact_sha256: String,
    pub kit_sha256: String,
    pub outcome: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub reason_code: Option<String>,
    pub tests: Vec<TestResult>,
    #[serde(default)]
    pub host: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub finished: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestResult {
    pub id: String,
    pub outcome: String,
}

/// A result proves a pass only when it binds this check's commit, tree,
/// asset and kit, reports exactly the expected tests, each passed, and the
/// exit code agrees. Copying a `passed` field proves nothing (design 7.2).
pub fn verify_result(job: &NativeJob, path: &Path, exit: i32) -> Step<NativeRecord> {
    let id = &job.plan.id;
    let invalid = |m: String| Stop::new("native-result-invalid", format!("{id}: {m}"));
    let r: NativeResult = read_json(path).map_err(|e| invalid(format!("{e:#}")))?;
    if r.schema != 1 {
        return Err(invalid(format!("unsupported result schema {}", r.schema)));
    }
    if &r.native_id != id || r.commit != job.commit || r.tree != job.tree {
        return Err(invalid("result names another check, commit or tree".into()));
    }
    if r.artifact_sha256 != job.asset_sha256 || r.kit_sha256 != job.kit_sha256 {
        return Err(invalid(
            "result is not bound to the exact asset and kit".into(),
        ));
    }
    let record = |outcome: &str| -> Step<NativeRecord> {
        Ok(NativeRecord {
            sbx: read_json::<serde_json::Value>(path)?.get("sbx").cloned(),
            service: read_json::<serde_json::Value>(path)?
                .get("service")
                .cloned(),
            outcome: outcome.into(),
            result_sha256: sha256_file(path)?,
            asset_sha256: r.artifact_sha256.clone(),
            kit_sha256: r.kit_sha256.clone(),
            reason: r.reason.clone(),
            finished: r
                .finished
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&chrono::Utc)),
            host: r
                .host
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string()),
                    )
                })
                .collect(),
        })
    };
    match (r.outcome.as_str(), exit) {
        ("blocked", 2) => {
            return Err(Stop::new(
                "native-blocked",
                format!(
                    "{id} blocked [{}] {}",
                    r.reason_code.as_deref().unwrap_or("unknown"),
                    r.reason.as_deref().unwrap_or("")
                ),
            ))
        }
        ("failed", 1) => {
            let failing: Vec<_> = r
                .tests
                .iter()
                .filter(|t| t.outcome != "passed")
                .map(|t| t.id.as_str())
                .collect();
            return Err(Stop::new(
                "native-failed",
                format!("{id}: {}", failing.join(", ")),
            ));
        }
        ("passed", 0) => {}
        (o, e) => return Err(invalid(format!("outcome `{o}` with exit {e}"))),
    }
    let mut seen = BTreeSet::new();
    for t in &r.tests {
        if !seen.insert(t.id.as_str()) {
            return Err(invalid(format!("test {} reported twice", t.id)));
        }
        if t.outcome != "passed" {
            return Err(invalid(format!(
                "passed result contains {} = {}",
                t.id, t.outcome
            )));
        }
    }
    let want: BTreeSet<&str> = job.plan.tests.iter().map(String::as_str).collect();
    if seen != want {
        return Err(invalid(format!(
            "executed tests differ from the plan (missing {:?}, extra {:?})",
            want.difference(&seen).collect::<Vec<_>>(),
            seen.difference(&want).collect::<Vec<_>>()
        )));
    }
    evidence::verify(job, path)?;
    record("passed")
}

// ── recovery identity ────────────────────────────────────────────────────────

/// Written atomically before anything is allocated or started (design
/// 7.10). A controller killed at any later point leaves this identity, and
/// resume uses it to adopt or clean exactly this allocation instead of
/// creating a second one. It becomes a `Lease` once the started process has
/// confirmed its own PID and start time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Intent {
    pub allocation: String,
    pub host: String,
    pub root: String,
    /// Boot of the local host when the intent was recorded. Empty for the
    /// SSH host, whose supervisor reports its boot in the start handshake.
    #[serde(default)]
    pub boot_id: String,
}

fn lease_path(job: &NativeJob) -> PathBuf {
    job.out_dir.join("lease.json")
}

fn intent_path(job: &NativeJob) -> PathBuf {
    job.out_dir.join("intent.json")
}

/// Drop the recovery records once their allocation is gone.
fn forget(job: &NativeJob) {
    let _ = std::fs::remove_file(lease_path(job));
    let _ = std::fs::remove_file(intent_path(job));
}

fn valid_allocation(allocation: &str) -> bool {
    allocation.len() == 32
        && allocation
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Work recorded on another host is never abandoned by starting a second
/// run here; the operator resumes with the host that owns it.
fn check_host(job: &NativeJob, recorded: &str, host: &str) -> Step {
    if recorded == host {
        return Ok(());
    }
    Err(Stop::new(
        "native-blocked",
        format!(
            "{} has unfinished work recorded on {recorded}, not {host}",
            job.plan.id
        ),
    )
    .with_remedy(format!(
        "run make release with the host that owns the work ({recorded}) so it is adopted or cleaned"
    )))
}

fn read_lease(job: &NativeJob, host: &str) -> Step<Option<Lease>> {
    match read_json::<Lease>(&lease_path(job)) {
        Ok(lease) => {
            check_host(job, &lease.host, host)?;
            Ok(Some(lease))
        }
        Err(e) if std::fs::symlink_metadata(lease_path(job)).is_ok() => Err(Stop::new(
            "state-corrupt",
            format!("{} has an unreadable ownership lease: {e:#}", job.plan.id),
        )),
        Err(_) => Ok(None),
    }
}

/// The recorded intent, or a new one written before any host work.
fn load_intent(
    job: &NativeJob,
    host: &str,
    boot_id: String,
    root_for: impl Fn(&str) -> String,
) -> Step<Intent> {
    let path = intent_path(job);
    if path.exists() {
        let intent: Intent = read_json(&path)?;
        check_host(job, &intent.host, host)?;
        if !valid_allocation(&intent.allocation) || intent.root != root_for(&intent.allocation) {
            return Err(Stop::new(
                "state-corrupt",
                format!("{} names an invalid allocation", path.display()),
            ));
        }
        return Ok(intent);
    }
    let allocation = random_id()?;
    let intent = Intent {
        root: root_for(&allocation),
        allocation,
        host: host.into(),
        boot_id,
    };
    write_json(&path, &intent)?;
    Ok(intent)
}

fn new_lease(
    job: &NativeJob,
    world: &super::world::World,
    intent: &Intent,
    boot_id: String,
    pid: u32,
    start: String,
) -> Lease {
    // Adopted work gets a fresh backstop deadline: the kit enforces its own
    // budget from its actual start, so it cannot overrun by more than that.
    Lease {
        allocation: intent.allocation.clone(),
        host: intent.host.clone(),
        boot_id,
        root: intent.root.clone(),
        pid,
        start,
        deadline: world.clock.now() + chrono::Duration::seconds(job.deadline_seconds as i64),
    }
}

/// Record the lease, then retire the intent it replaces.
fn confirm(job: &NativeJob, lease: &Lease) -> Step {
    write_json(&lease_path(job), lease)?;
    let _ = std::fs::remove_file(intent_path(job));
    Ok(())
}

/// Owned processes that outlived the kit runner. The record is durable, so a
/// resumed controller never accepts the result that came with them.
fn record_leak(job: &NativeJob, detail: &str) -> Step {
    write_json(&job.out_dir.join("leaks.json"), &detail)?;
    Ok(())
}

fn recorded_leak(job: &NativeJob) -> Option<Stop> {
    let detail: String = read_json(&job.out_dir.join("leaks.json")).ok()?;
    Some(Stop::new(
        "native-result-invalid",
        format!(
            "{}: owned processes were still running after the kit runner exited ({detail}); they were stopped and the result is not accepted",
            job.plan.id
        ),
    ))
}

/// `key=value` lines.
fn kv(bytes: &[u8]) -> BTreeMap<String, String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

/// The PID and start time a started process wrote about itself.
fn read_handshake(text: &[u8]) -> Option<(u32, String)> {
    let facts = kv(text);
    let pid = facts.get("pid")?.parse::<u32>().ok().filter(|p| *p > 1)?;
    let start = facts.get("start")?.clone();
    (!start.is_empty()).then_some((pid, start))
}

// ── helpers for owned processes ──────────────────────────────────────────────

fn process_start(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

fn local_boot_id() -> String {
    if let Ok(id) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        return id.trim().to_string();
    }
    Command::new("sysctl")
        .args(["-n", "kern.boottime"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

/// Processes whose process group is `pgid` (`ps`, available on both hosts).
fn group_members(pgid: u32) -> Vec<u32> {
    let Ok(out) = Command::new("ps").args(["-A", "-o", "pid=,pgid="]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let group = fields.next()?.parse::<u32>().ok()?;
            (group == pgid).then_some(pid)
        })
        .collect()
}

/// Copy the completed evidence out of a disposable root before cleanup.
fn keep_evidence(from: &Path, to: &Path) -> Step {
    private_dir(to)?;
    for e in
        std::fs::read_dir(from).map_err(|e| Stop::new("native-result-invalid", e.to_string()))?
    {
        let p = e
            .map_err(|e| Stop::new("native-result-invalid", e.to_string()))?
            .path();
        if p.is_file() {
            std::fs::copy(&p, to.join(p.file_name().unwrap()))
                .map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
        }
    }
    Ok(())
}

// ── LocalHost: the Apple Silicon Mac running the command ─────────────────────

/// The local kit wrapper. Its first action is the start handshake: it writes
/// its own PID and start time into the allocation, so the lease records an
/// identity the process itself confirmed. If the allocation was abandoned
/// (renamed) before that, the write fails and no work starts. It records the
/// kit exit code next to result.json so a later controller can collect a
/// run it did not start.
const LOCAL_WRAPPER: &str = r#"printf 'pid=%s\nstart=%s\n' "$$" "$(ps -o lstart= -p $$)" > "$AWMAN_KIT_ROOT/started.tmp" && mv "$AWMAN_KIT_ROOT/started.tmp" "$AWMAN_KIT_ROOT/started" || exit 125
"$@"; echo $? > "$AWMAN_KIT_OUT/exit""#;

/// How long the controller waits for the start handshake.
const HANDSHAKE_SECONDS: u64 = 10;

pub struct LocalHost {
    /// Parent of private allocations; canonical `/private/tmp` on macOS.
    pub tmp_parent: PathBuf,
    /// Children started by this process, reaped while polling.
    pub children: std::cell::RefCell<Vec<std::process::Child>>,
}

impl LocalHost {
    pub fn new() -> LocalHost {
        let parent = if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        };
        LocalHost {
            tmp_parent: PathBuf::from(parent),
            children: Default::default(),
        }
    }

    fn allocation_root(&self, allocation: &str) -> PathBuf {
        self.tmp_parent.join(format!("awr-{}", &allocation[..12]))
    }

    pub fn socket_budget_ok(&self, suffix: usize) -> bool {
        self.allocation_root("000000000000")
            .join("t")
            .to_string_lossy()
            .len()
            + suffix
            <= SUN_PATH_MAX
    }

    fn owns(&self, root: &Path, allocation: &str) -> bool {
        root.starts_with(&self.tmp_parent)
            && std::fs::symlink_metadata(root).is_ok_and(|m| m.is_dir())
            && std::fs::read_to_string(root.join("owner")).is_ok_and(|o| o.trim() == allocation)
    }

    fn reap(&self) {
        for c in self.children.borrow_mut().iter_mut() {
            let _ = c.try_wait();
        }
    }

    /// The wrapper process named by the lease is running (same boot, same
    /// PID and start time).
    fn alive(&self, lease: &Lease) -> bool {
        self.reap();
        lease.boot_id == local_boot_id()
            && process_start(lease.pid).as_deref() == Some(lease.start.as_str())
    }

    /// Give a wrapper that just wrote its exit code a moment to leave, so it
    /// is not mistaken for a leaked process.
    fn settle(&self, lease: &Lease) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.alive(lease) && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// Stop every process this allocation owns, before its root is removed:
    /// the authenticated wrapper's process group, processes whose private
    /// HOME/TMPDIR lies under the root, and the rest of the wrapper's group
    /// once one of those proves the group is still ours. Unrelated processes
    /// are never signalled. Returns what was still running apart from a live
    /// wrapper.
    fn stop_owned(&self, lease: &Lease) -> Vec<u32> {
        if lease.boot_id != local_boot_id() {
            // A reboot ended every process of that allocation.
            return Vec::new();
        }
        if self.alive(lease) {
            kill_group(lease.pid);
        }
        let members = group_members(lease.pid);
        let mut stopped = crate::test::isolation::cleanup_owned_processes(Path::new(&lease.root));
        if stopped.iter().any(|p| members.contains(p)) {
            kill_group(lease.pid);
            stopped.extend(members);
        }
        stopped.sort_unstable();
        stopped.dedup();
        stopped.retain(|p| *p != lease.pid);
        self.reap();
        stopped
    }

    fn remove_root(&self, root: &str, allocation: &str) {
        let root = Path::new(root);
        if self.owns(root, allocation) {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    fn read_started(root: &Path) -> Option<(u32, String)> {
        read_handshake(&std::fs::read(root.join("started")).ok()?)
    }

    /// Resume after an interruption between recording the intent and the
    /// lease. A run whose handshake authenticates it (alive, or finished with
    /// an exit code) is adopted. Anything else is made unusable first, then
    /// its owned processes are stopped and its root removed. Returns whether
    /// the run was adopted.
    fn adopt_or_clean(
        &self,
        job: &NativeJob,
        world: &super::world::World,
        intent: &Intent,
    ) -> Step<bool> {
        let root = PathBuf::from(&intent.root);
        if std::fs::symlink_metadata(&root).is_err() {
            let parked = PathBuf::from(format!("{}.abandoned", intent.root));
            if self.owns(&parked, &intent.allocation) {
                crate::test::isolation::cleanup_owned_processes(&root);
                std::fs::rename(&parked, &root).map_err(anyhow::Error::from)?;
            } else {
                return Ok(false);
            }
        }
        if !self.owns(&root, &intent.allocation) {
            forget(job);
            return Err(Stop::new(
                "mac-host",
                format!("{} is not ours; it was left untouched", root.display()),
            ));
        }
        let lease_for =
            |(pid, start)| new_lease(job, world, intent, intent.boot_id.clone(), pid, start);
        if let Some(lease) = Self::read_started(&root).map(lease_for) {
            if self.alive(&lease) || root.join("out/exit").exists() {
                confirm(job, &lease)?;
                return Ok(true);
            }
        }
        // Park the allocation: a wrapper that has not yet completed its
        // handshake can no longer start work under the original path.
        let parked = PathBuf::from(format!("{}.abandoned", intent.root));
        let stale = if std::fs::rename(&root, &parked).is_ok() {
            parked
        } else {
            root.clone()
        };
        match Self::read_started(&stale).map(lease_for) {
            Some(lease) => {
                self.stop_owned(&lease);
            }
            None => {
                crate::test::isolation::cleanup_owned_processes(&root);
            }
        }
        if [
            crate::manifest::release_policy::APPLE_NATIVE,
            crate::manifest::release_policy::DOCKER_NATIVE,
        ]
        .contains(&job.plan.id.as_str())
        {
            // Processes are stopped. Restore the reservation's canonical scratch path
            // for reconciliation; retain the intent and directory on any failure.
            if stale != root {
                std::fs::rename(&stale, &root).map_err(anyhow::Error::from)?;
            }
            cleanup_runtime_allocation(job, &root)?;
            keep_evidence(&root.join("out"), &job.out_dir)?;
            self.remove_root(&intent.root, &intent.allocation);
            return Ok(false);
        }
        if job.plan.id == crate::manifest::release_policy::SBX_NATIVE {
            super::sbx::cleanup(&stale.join("out"), &root.join("tmp")).map_err(|e| {
                Stop::new(
                    "native-result-invalid",
                    format!("owned SBX cleanup failed: {e:#}"),
                )
            })?;
        }
        let _ = std::fs::remove_dir_all(&stale);
        Ok(false)
    }

    /// Allocate the root, start the wrapper and complete the handshake.
    fn launch(&self, job: &NativeJob, world: &super::world::World, intent: &Intent) -> Step {
        let root = PathBuf::from(&intent.root);
        std::fs::create_dir(&root).map_err(|e| {
            Stop::new(
                "mac-host",
                format!("cannot allocate {}: {e}", root.display()),
            )
        })?;
        private_dir(&root)?;
        std::fs::write(root.join("owner"), &intent.allocation)
            .map_err(|e| Stop::new("mac-host", e.to_string()))?;
        for d in ["tmp", "out", "home", "fixtures"] {
            private_dir(&root.join(d))?;
        }
        for (d, file) in &job.downloads {
            let to = root.join("fixtures").join(&d.file);
            if let Some(p) = to.parent() {
                private_dir(p)?;
            }
            std::fs::copy(file, &to).map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
            #[cfg(unix)]
            if d.id.starts_with("previous-release:") {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o700))
                    .map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
            }
        }
        let log = std::fs::File::create(job.out_dir.join("kit-run.log"))
            .map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", LOCAL_WRAPPER, "kit-run"])
            .arg(job.kit_dir.join("bin/xtask"))
            .args(["test", "kit-run", "--kit"])
            .arg(&job.kit_dir)
            .arg("--artifact")
            .arg(&job.asset)
            .args(["--artifact-sha256", &job.asset_sha256, "--fixtures"])
            .arg(root.join("fixtures"))
            .arg("--tmproot")
            .arg(root.join("tmp"))
            .arg("--out")
            .arg(root.join("out"))
            .current_dir(root.join("tmp"))
            .env("AWMAN_KIT_ROOT", &root)
            .env("AWMAN_KIT_OUT", root.join("out"))
            .env("HOME", root.join("home"))
            // Keeps every descendant's private roots under the allocation,
            // which is how cleanup recognises them.
            .env("TMPDIR", root.join("tmp"))
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .stdin(Stdio::null())
            .stdout(
                log.try_clone()
                    .map_err(|e| Stop::new("state-corrupt", e.to_string()))?,
            )
            .stderr(log);
        if job.plan.id == crate::manifest::release_policy::SBX_NATIVE {
            cmd.args(["--native", &job.plan.id]);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = cmd
            .spawn()
            .map_err(|e| Stop::new("kit-invalid", format!("cannot start the kit runner: {e}")))?;
        let pid = child.id();
        self.children.borrow_mut().push(child);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(HANDSHAKE_SECONDS);
        let start = loop {
            match Self::read_started(&root) {
                Some((p, start)) if p == pid => break Some(start),
                Some(_) => break None,
                None if std::time::Instant::now() >= until => break None,
                None => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        };
        let Some(start) = start else {
            // Our own unreaped child: its PID cannot have been reused.
            kill_group(pid);
            crate::test::isolation::cleanup_owned_processes(&root);
            cleanup_runtime_allocation(job, &root)?;
            self.remove_root(&intent.root, &intent.allocation);
            forget(job);
            return Err(Stop::new(
                "native-blocked",
                format!(
                    "the {} kit runner did not confirm its start within {HANDSHAKE_SECONDS} s; see native/{}/kit-run.log",
                    job.plan.id, job.plan.id
                ),
            ));
        };
        confirm(
            job,
            &new_lease(job, world, intent, intent.boot_id.clone(), pid, start),
        )
    }

    /// Stop, remove and forget the leased allocation.
    fn discard(&self, job: &NativeJob, lease: &Lease) -> Step {
        self.stop_owned(lease);
        cleanup_runtime_allocation(job, Path::new(&lease.root))?;
        self.remove_root(&lease.root, &lease.allocation);
        forget(job);
        Ok(())
    }
}

impl Default for LocalHost {
    fn default() -> Self {
        Self::new()
    }
}

fn which(tool: &str) -> bool {
    Command::new("sh")
        .args(["-c", "command -v \"$1\" >/dev/null 2>&1", "sh", tool])
        .status()
        .is_ok_and(|s| s.success())
}

impl NativeHost for LocalHost {
    fn cancel(&self, job: &NativeJob, world: &super::world::World) -> Step {
        if let Some(lease) = read_lease(job, "local")? {
            if !valid_allocation(&lease.allocation)
                || Path::new(&lease.root) != self.allocation_root(&lease.allocation)
            {
                return Err(Stop::new("state-corrupt", "invalid local cleanup lease"));
            }
            return self.discard(job, &lease);
        }
        if intent_path(job).exists() {
            let intent = load_intent(job, "local", String::new(), |a| {
                self.allocation_root(a).to_string_lossy().into()
            })?;
            if self.adopt_or_clean(job, world, &intent)? {
                let lease = read_lease(job, "local")?
                    .ok_or_else(|| Stop::new("state-corrupt", "adopted cleanup has no lease"))?;
                self.discard(job, &lease)?;
            }
            forget(job);
        }
        Ok(())
    }
    fn describe(&self) -> String {
        "this Mac".into()
    }

    fn probe(&self, plan: &NativePlan) -> Step {
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return Err(Stop::new(
                "mac-host",
                format!(
                    "{} runs locally and needs an Apple Silicon Mac; run make release there",
                    plan.id
                ),
            ));
        }
        if plan.id == crate::manifest::release_policy::SBX_NATIVE {
            super::sbx::probe().map_err(Stop::from)?;
            return Ok(());
        }
        if plan.id == crate::manifest::release_policy::APPLE_NATIVE {
            let mut command = Command::new("container");
            command.args(["system", "status"]);
            let out =
                super::util::output_bounded(&mut command, None, std::time::Duration::from_secs(20))
                    .map_err(|e| Stop::new("apple-service-unavailable", e.to_string()))?;
            if !out.status.success() {
                return Err(Stop::new(
                    "apple-service-unavailable",
                    "container system status failed",
                ));
            }
            return Ok(());
        }
        let hv = Command::new("sysctl")
            .args(["-n", "kern.hv_support"])
            .output();
        if !hv.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "1") {
            return Err(Stop::new(
                "mac-host",
                "Hypervisor.framework is unavailable (kern.hv_support != 1)",
            ));
        }
        for tool in ["git", "codesign", "sandbox-exec", "ps"] {
            if !which(tool) {
                return Err(Stop::new("mac-host", format!("`{tool}` is not on PATH")));
            }
        }
        if !self.socket_budget_ok(DEFAULT_SOCKET_SUFFIX) {
            return Err(Stop::new(
                "mac-host",
                format!(
                    "socket path budget exceeded under {}",
                    self.tmp_parent.display()
                ),
            ));
        }
        let need_mb = 4096u64;
        let df = Command::new("df")
            .args(["-Pk"])
            .arg(&self.tmp_parent)
            .output();
        let free_kb = df
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .nth(1)
                    .map(str::to_string)
            })
            .and_then(|l| {
                l.split_whitespace()
                    .nth(3)
                    .and_then(|v| v.parse::<u64>().ok())
            });
        if free_kb.is_some_and(|kb| kb / 1024 < need_mb) {
            return Err(Stop::new(
                "mac-host",
                format!(
                    "{} has less than {need_mb} MB free for the kit, fixtures and VM state",
                    self.tmp_parent.display()
                ),
            ));
        }
        Ok(())
    }

    fn start(&self, job: &NativeJob, world: &super::world::World) -> Step {
        if durable(job).is_some() {
            return Ok(());
        }
        if let Some(lease) = read_lease(job, "local")? {
            if self.alive(&lease) || Path::new(&lease.root).join("out/exit").exists() {
                // Authenticated live work or a finished run: poll adopts it.
                return Ok(());
            }
            // Dead and incomplete: stop and clean only our allocation.
            self.discard(job, &lease)?;
        }
        let intent = load_intent(job, "local", local_boot_id(), |a| {
            self.allocation_root(a).to_string_lossy().into()
        })?;
        if self.adopt_or_clean(job, world, &intent)? {
            return Ok(());
        }
        self.launch(job, world, &intent)
    }

    fn poll(&self, job: &NativeJob, world: &super::world::World) -> Step<Option<(PathBuf, i32)>> {
        self.reap();
        if let Some(leak) = recorded_leak(job) {
            if let Some(lease) = read_lease(job, "local")? {
                self.discard(job, &lease)?;
            }
            return Err(leak);
        }
        if let Some(done) = durable(job) {
            return Ok(Some(done));
        }
        let lease = read_lease(job, "local")?.ok_or_else(|| {
            Stop::new(
                "native-blocked",
                format!("{} has no running check; run make release", job.plan.id),
            )
        })?;
        let root = PathBuf::from(&lease.root);
        let out = root.join("out");
        if out.join("exit").exists() {
            self.settle(&lease);
            let leaked = self.stop_owned(&lease);
            if !out.join("result.json").exists() {
                self.discard(job, &lease)?;
                return Err(Stop::new(
                    "native-result-invalid",
                    format!(
                        "{} exited without result.json; see native/{}/kit-run.log",
                        job.plan.id, job.plan.id
                    ),
                ));
            }
            if !leaked.is_empty() {
                record_leak(job, &format!("pids {leaked:?}"))?;
            }
            keep_evidence(&out, &job.out_dir)?;
            if cleanup_runtime_allocation(job, &root)? {
                record_leak(job, "owned SBX sandbox survived the kit runner")?;
            }
            // Original raw evidence was retained before reconciliation.
            self.remove_root(&lease.root, &lease.allocation);
            forget(job);
            if let Some(leak) = recorded_leak(job) {
                return Err(leak);
            }
            return Ok(durable(job));
        }
        if !self.alive(&lease) {
            self.discard(job, &lease)?;
            return Err(Stop::new(
                "native-blocked",
                format!("the {} kit runner stopped without a result; make release reruns only this check", job.plan.id),
            ));
        }
        if world.clock.now() > lease.deadline {
            self.discard(job, &lease)?;
            return Err(Stop::new(
                runtime_deadline_code(job),
                format!(
                    "{} exceeded its {} s budget",
                    job.plan.id, job.deadline_seconds
                ),
            ));
        }
        Ok(None)
    }
}

// ── SshHost: the Linux KVM host ──────────────────────────────────────────────

/// One constant remote supervisor. Arguments arrive as positional
/// parameters, already validated; nothing user-controlled is evaluated.
/// Allocation and start are idempotent and queryable by the allocation ID
/// recorded in the intent before the first remote call, so a resumed
/// controller adopts or cleans the same allocation.
const SUPERVISOR: &str = r#"set -eu
cmd=$1; shift
owned() { [ -d "$1" ] && [ ! -L "$1" ] && [ -O "$1" ] && [ -f "$1/owner" ] && [ "$(cat "$1/owner")" = "$2" ] && case $1 in /tmp/awr-*) true;; *) false;; esac; }
boot() { cat /proc/sys/kernel/random/boot_id; }
leader() { sed -n "s/^$2=//p" "$1/started" 2>/dev/null || true; }
# Give a wrapper that just wrote its exit code a moment to leave.
settle() {
  pid=$(leader "$1" pid); i=0
  while [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
}
# Kill this allocation's processes: any whose environment holds a path
# under the root, and members of the kit session started on this boot.
sweep() {
  root=$1; n=0; sid=
  if [ "$(leader "$root" boot)" = "$(boot)" ]; then sid=$(leader "$root" pid); fi
  for d in /proc/[0-9]*; do
    p=${d#/proc/}
    [ "$p" = "$$" ] && continue
    hit=
    if tr '\0' '\n' < "$d/environ" 2>/dev/null | grep -q "^[^=]*=$root/"; then hit=1; fi
    if [ -z "$hit" ] && [ -n "$sid" ] && [ "$(sed 's/.*) //' "$d/stat" 2>/dev/null | cut -d' ' -f4)" = "$sid" ]; then hit=1; fi
    if [ -n "$hit" ] && kill -KILL "$p" 2>/dev/null; then n=$((n+1)); fi
  done
  echo "swept=$n"
}
case $cmd in
docker-probe)
  command -v docker >/dev/null 2>&1 || { echo docker.server=no; exit 0; }
  if timeout --kill-after=5 20 docker version --format '{{.Server.Version}}' > /dev/null 2>&1; then echo docker.server=yes; else echo docker.server=no; fi
  ;;
probe)
  echo "arch=$(uname -m)"
  if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then echo kvm=yes; else echo kvm=no; fi
  for t in git setsid tar curl sha256sum ps timeout; do
    if command -v "$t" >/dev/null 2>&1; then echo "tool.$t=yes"; else echo "tool.$t=no"; fi
  done
  echo "seccomp=$(grep -c '^Seccomp:' /proc/self/status 2>/dev/null || echo 0)"
  echo "free_kb=$(df -Pk /tmp | awk 'NR==2 {print $4}')"
  echo "glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')"
  ;;
query)
  root=$1; owner=$2
  if [ ! -e "$root" ] && [ ! -L "$root" ]; then echo state=absent; exit 0; fi
  owned "$root" "$owner" || { echo state=foreign; exit 0; }
  if [ -f "$root/started" ]; then echo state=started; cat "$root/started"
  elif [ -d "$root/launch" ]; then echo state=launching
  else echo state=allocated; fi
  ;;
alloc)
  root=$1; owner=$2
  case $root in /tmp/awr-*) ;; *) echo "invalid allocation $root" >&2; exit 4;; esac
  if [ -e "$root" ] || [ -L "$root" ]; then
    owned "$root" "$owner" || { echo "allocation $root is not ours" >&2; exit 3; }
  else
    mkdir -m 700 "$root"; printf %s "$owner" > "$root/owner"
  fi
  for d in tmp out home fixtures kit; do [ -d "$root/$d" ] || mkdir -m 700 "$root/$d"; done
  echo "$root"
  ;;
fixture)
  root=$1; id=$2; sha=$3; url=$4; file=$5
  cache="$HOME/.cache/awman-release/fixtures"; mkdir -p -m 700 "$cache"
  if [ ! -f "$cache/$sha" ] || [ "$(sha256sum "$cache/$sha" | cut -d' ' -f1)" != "$sha" ]; then
    curl -fsSL --max-time 900 -o "$cache/$sha.part" "$url"
    [ "$(sha256sum "$cache/$sha.part" | cut -d' ' -f1)" = "$sha" ] || { echo "fixture $id digest mismatch" >&2; exit 3; }
    mv "$cache/$sha.part" "$cache/$sha"
  fi
  mkdir -p "$root/fixtures/$(dirname "$file")"; cp "$cache/$sha" "$root/fixtures/$file"
  case $file in previous-release/*) chmod 700 "$root/fixtures/$file";; esac
  ;;
receive)
  owned "$1" "$3"
  cat > "$1/$2"
  ;;
start)
  root=$1; sha=$2; owner=$3
  owned "$root" "$owner"
  # Started at most once per allocation; a repeated start reports the
  # identity recorded by the first.
  if mkdir "$root/launch" 2>/dev/null; then
    tar -xf "$root/kit.tar" -C "$root/kit" --no-same-owner --no-overwrite-dir
    chmod 700 "$root/asset"
    cd "$root/tmp"
    HOME="$root/home" TMPDIR="$root/tmp" AWMAN_KIT_OUT="$root/out" setsid nohup sh -c '"$@"; echo $? > "$AWMAN_KIT_OUT/exit"' kit-run \
      "$root/kit/bin/xtask" test kit-run --kit "$root/kit" \
      --artifact "$root/asset" --artifact-sha256 "$sha" --fixtures "$root/fixtures" \
      --tmproot "$root/tmp" --out "$root/out" > "$root/out/kit-run.log" 2>&1 < /dev/null &
    pid=$!
    printf 'pid=%s\nboot=%s\nstart=%s\n' "$pid" "$(boot)" "$(ps -o lstart= -p "$pid")" > "$root/started.tmp"
    mv "$root/started.tmp" "$root/started"
  fi
  [ -f "$root/started" ] || { echo "the start of $root was interrupted" >&2; exit 5; }
  cat "$root/started"
  ;;
status)
  root=$1; owner=$2; pid=$3; start=$4; bootid=$5
  owned "$root" "$owner" || { echo state=foreign; exit 0; }
  if [ -f "$root/out/exit" ] && [ -f "$root/out/result.json" ]; then echo state=done; exit 0; fi
  if [ "$(boot)" = "$bootid" ] && [ "$(ps -o lstart= -p "$pid" 2>/dev/null)" = "$start" ]; then echo state=running; else echo state=dead; fi
  ;;
sweep)
  root=$1; owner=$2
  owned "$root" "$owner"
  settle "$root"; sweep "$root"
  ;;
collect)
  root=$1; owner=$2
  owned "$root" "$owner"
  tar -cf - -C "$root/out" .
  ;;
kill)
  root=$1; owner=$2; pid=$3; start=$4; bootid=$5
  owned "$root" "$owner"
  if [ "$(boot)" = "$bootid" ] && [ "$(ps -o lstart= -p "$pid" 2>/dev/null)" = "$start" ]; then kill -KILL -- "-$pid" 2>/dev/null || true; fi
  ;;
cleanup)
  root=$1; owner=$2
  owned "$root" "$owner" || { echo swept=0; exit 0; }
  sweep "$root"
  if [ -f "$root/out/runtime-owned.json" ]; then
    timeout --kill-after=5 120 "$root/kit/bin/xtask" test kit-reconcile --kit "$root/kit" --out "$root/out" --tmproot "$root/tmp"
  fi
  rm -rf -- "$root"
  ;;
esac
"#;

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Wall-clock deadline for each supervisor command (design 7.9). The SSH
/// connect and keepalive options only detect a dead connection; a live host
/// whose command stalls is stopped at this deadline.
pub fn ssh_deadline(op: &str) -> std::time::Duration {
    std::time::Duration::from_secs(match op {
        // curl on the host is bounded at 900 s.
        "fixture" => 960,
        // Kit and asset uploads.
        "receive" => 1800,
        // Kit extraction; result bundle download.
        "start" | "collect" => 300,
        "cleanup" => 180,
        _ => 60,
    })
}

pub struct SshHost {
    pub destination: String,
    /// Private local directory for the SSH control socket.
    pub control_dir: PathBuf,
    /// The SSH client (`ssh`); tests substitute a local stand-in.
    pub program: PathBuf,
}

impl SshHost {
    fn ssh(&self, args: &[&str], stdin: Option<&Path>) -> Step<Vec<u8>> {
        self.ssh_within(args, stdin, ssh_deadline(args[0]))
    }

    fn ssh_within(
        &self,
        args: &[&str],
        stdin: Option<&Path>,
        deadline: std::time::Duration,
    ) -> Step<Vec<u8>> {
        let op = args[0];
        let control = self.control_dir.join("cm");
        if control.to_string_lossy().len() > SUN_PATH_MAX {
            return Err(Stop::new(
                "linux-host-prerequisite",
                "SSH control socket path is too long",
            ));
        }
        let remote = std::iter::once("sh".to_string())
            .chain(["-c".to_string(), SUPERVISOR.to_string(), "sup".to_string()])
            .chain(args.iter().map(|a| a.to_string()))
            .map(|a| shell_quote(&a))
            .collect::<Vec<_>>()
            .join(" ");
        let mut c = Command::new(&self.program);
        c.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=8",
            "-o",
            "ServerAliveInterval=15",
        ])
        .args([
            "-o",
            "ServerAliveCountMax=4",
            "-o",
            "ForwardAgent=no",
            "-o",
            "ClearAllForwardings=yes",
        ])
        .args(["-o", "ControlMaster=auto", "-o", "ControlPersist=120", "-o"])
        .arg(format!("ControlPath={}", control.display()))
        .arg(&self.destination)
        .arg("--")
        .arg(remote)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        match stdin {
            Some(p) => {
                c.stdin(
                    std::fs::File::open(p)
                        .map_err(|e| Stop::new("state-corrupt", e.to_string()))?,
                );
            }
            None => {
                c.stdin(Stdio::null());
            }
        }
        let out = match super::util::output_bounded(&mut c, None, deadline) {
            Ok(out) => out,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return Err(Stop::new(
                    "linux-host-unreachable",
                    format!(
                    "ssh {} {op} gave no result within its {} s deadline; the command was stopped",
                    self.destination,
                    deadline.as_secs()
                ),
                ))
            }
            Err(e) => return Err(Stop::new("tool-missing", format!("ssh: {e}"))),
        };
        if out.status.code() == Some(255) {
            return Err(Stop::new(
                "linux-host-unreachable",
                format!(
                    "ssh {} {op} failed (8 s connect timeout): {}",
                    self.destination,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        if !out.status.success() {
            return Err(Stop::new(
                "linux-host-prerequisite",
                format!(
                    "{op} on {}: {}",
                    self.destination,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        Ok(out.stdout)
    }

    fn remote_root(allocation: &str) -> String {
        format!("/tmp/awr-{}", &allocation[..12])
    }

    fn cleanup(&self, root: &str, allocation: &str) -> Step {
        let out = self
            .ssh(&["cleanup", root, allocation], None)
            .map_err(|e| {
                if e.code == "linux-host-prerequisite" {
                    Stop::new(
                        "native-result-invalid",
                        format!("owned runtime cleanup failed: {}", e.message),
                    )
                } else {
                    e
                }
            })?;
        if kv(&out).get("leaked").is_some_and(|v| v != "0") {
            return Err(Stop::new(
                "native-result-invalid",
                "owned runtime object survived the kit runner",
            ));
        }
        Ok(())
    }

    /// Turn the supervisor's start handshake into the lease. An identity that
    /// does not authenticate a process is never trusted: the allocation is
    /// cleaned instead.
    fn adopt(
        &self,
        job: &NativeJob,
        world: &super::world::World,
        intent: &Intent,
        facts: &[u8],
    ) -> Step {
        let boot = kv(facts).get("boot").cloned().unwrap_or_default();
        match read_handshake(facts) {
            Some((pid, start)) if !boot.is_empty() => {
                confirm(job, &new_lease(job, world, intent, boot, pid, start))
            }
            _ => {
                self.cleanup(&intent.root, &intent.allocation)?;
                forget(job);
                Err(Stop::new(
                    "linux-host-prerequisite",
                    format!(
                        "the {} run on {} reported no process identity; its allocation was cleaned",
                        job.plan.id, self.destination
                    ),
                ))
            }
        }
    }

    /// Stop, remove and forget the leased allocation.
    fn discard(&self, job: &NativeJob, lease: &Lease) -> Step {
        self.cleanup(&lease.root, &lease.allocation)?;
        forget(job);
        Ok(())
    }
}

impl NativeHost for SshHost {
    fn cancel(&self, job: &NativeJob, _world: &super::world::World) -> Step {
        if let Some(lease) = read_lease(job, &self.destination)? {
            if !valid_allocation(&lease.allocation)
                || lease.root != Self::remote_root(&lease.allocation)
            {
                return Err(Stop::new("state-corrupt", "invalid remote cleanup lease"));
            }
            return self.discard(job, &lease);
        }
        if intent_path(job).exists() {
            let intent = load_intent(job, &self.destination, String::new(), Self::remote_root)?;
            self.cleanup(&intent.root, &intent.allocation)?;
            forget(job);
        }
        Ok(())
    }
    fn describe(&self) -> String {
        self.destination.clone()
    }

    fn probe(&self, plan: &NativePlan) -> Step {
        let facts = kv(&self.ssh(&["probe"], None)?);
        let get = |k: &str| facts.get(k).map(String::as_str).unwrap_or("");
        let want_arch = if plan.triple.starts_with("x86_64") {
            "x86_64"
        } else {
            "aarch64"
        };
        if get("arch") != want_arch {
            return Err(Stop::new(
                "linux-host-prerequisite",
                format!(
                    "{} is {}, {} needs {want_arch}",
                    self.destination,
                    get("arch"),
                    plan.id
                ),
            ));
        }
        let docker = plan.id == crate::manifest::release_policy::DOCKER_NATIVE;
        if docker {
            let facts = kv(&self.ssh(&["docker-probe"], None)?);
            if facts.get("docker.server").map(String::as_str) != Some("yes") {
                return Err(Stop::new(
                    "docker-daemon-unavailable",
                    format!("Docker server is unavailable on {}", self.destination),
                ));
            }
        }
        if !docker && get("kvm") != "yes" {
            return Err(Stop::new(
                "linux-host-prerequisite",
                format!(
                    "/dev/kvm is not usable by this login on {}",
                    self.destination
                ),
            ));
        }
        for t in ["git", "setsid", "tar", "curl", "sha256sum", "ps", "timeout"] {
            if get(&format!("tool.{t}")) != "yes" {
                return Err(Stop::new(
                    "linux-host-prerequisite",
                    format!("`{t}` is missing on {}", self.destination),
                ));
            }
        }
        if !docker && get("seccomp") == "0" {
            return Err(Stop::new(
                "linux-host-prerequisite",
                format!(
                    "seccomp is unavailable on {}; offline denial cannot be enforced",
                    self.destination
                ),
            ));
        }
        let free_mb = get("free_kb").parse::<u64>().unwrap_or(0) / 1024;
        if free_mb < 4096 {
            return Err(Stop::new(
                "linux-host-prerequisite",
                format!(
                    "/tmp on {} has {free_mb} MB free; the kit, fixtures and VM state need 4096 MB",
                    self.destination
                ),
            ));
        }
        Ok(())
    }

    fn start(&self, job: &NativeJob, world: &super::world::World) -> Step {
        if durable(job).is_some() || read_lease(job, &self.destination)?.is_some() {
            // A lease means live or finished remote work; poll adopts it.
            return Ok(());
        }
        // The intent is durable before the first remote call, so every
        // allocation this controller makes can be found again by its ID.
        let intent = load_intent(job, &self.destination, String::new(), Self::remote_root)?;
        let (root, allocation) = (intent.root.as_str(), intent.allocation.as_str());
        let facts = self.ssh(&["query", root, allocation], None)?;
        match kv(&facts).get("state").map(String::as_str).unwrap_or("") {
            // Started before an interruption (or a lost response): adopt it.
            "started" => return self.adopt(job, world, &intent, &facts),
            "foreign" => {
                forget(job);
                return Err(Stop::new(
                    "linux-host-prerequisite",
                    format!(
                        "allocation {root} on {} is not ours; it was left untouched",
                        self.destination
                    ),
                ));
            }
            // The launch was interrupted before its identity was recorded:
            // stop whatever it owns, then prepare the same allocation again.
            "launching" => self.cleanup(root, allocation)?,
            // Absent or allocated: preparation below is idempotent.
            _ => {}
        }
        let allocated = String::from_utf8_lossy(&self.ssh(&["alloc", root, allocation], None)?)
            .trim()
            .to_string();
        if allocated != root {
            return Err(Stop::new(
                "linux-host-prerequisite",
                format!("unexpected allocation {allocated}"),
            ));
        }
        for d in job.plan.downloads() {
            self.ssh(&["fixture", root, &d.id, &d.sha256, &d.url, &d.file], None)?;
        }
        self.ssh(
            &["receive", root, "kit.tar", allocation],
            Some(&job.kit_tar),
        )?;
        self.ssh(&["receive", root, "asset", allocation], Some(&job.asset))?;
        let started = self.ssh(&["start", root, &job.asset_sha256, allocation], None)?;
        self.adopt(job, world, &intent, &started)
    }

    fn poll(&self, job: &NativeJob, world: &super::world::World) -> Step<Option<(PathBuf, i32)>> {
        if let Some(leak) = recorded_leak(job) {
            if let Some(lease) = read_lease(job, &self.destination)? {
                self.discard(job, &lease)?;
            }
            return Err(leak);
        }
        if let Some(done) = durable(job) {
            return Ok(Some(done));
        }
        let lease = read_lease(job, &self.destination)?.ok_or_else(|| {
            Stop::new(
                "native-blocked",
                format!("{} has no running check; run make release", job.plan.id),
            )
        })?;
        let pid = lease.pid.to_string();
        let ident = [
            lease.root.as_str(),
            lease.allocation.as_str(),
            pid.as_str(),
            lease.start.as_str(),
            lease.boot_id.as_str(),
        ];
        let with = |cmd: &'static str| std::iter::once(cmd).chain(ident).collect::<Vec<_>>();
        let state = kv(&self.ssh(&with("status"), None)?)
            .remove("state")
            .unwrap_or_default();
        match state.as_str() {
            "done" => {}
            "running" if world.clock.now() <= lease.deadline => return Ok(None),
            "running" => {
                self.ssh(&with("kill"), None)?;
                self.discard(job, &lease)?;
                return Err(Stop::new(
                    runtime_deadline_code(job),
                    format!(
                        "{} exceeded its {} s budget on {}",
                        job.plan.id, job.deadline_seconds, self.destination
                    ),
                ));
            }
            "foreign" => {
                forget(job);
                return Err(Stop::new(
                    "linux-host-prerequisite",
                    format!(
                        "allocation {} on {} is not ours; it was left untouched",
                        lease.root, self.destination
                    ),
                ));
            }
            _ => {
                // Dead and incomplete (host reboot, lost process): clean only
                // our allocation; the next make release reruns just this check.
                self.discard(job, &lease)?;
                return Err(Stop::new(
                    "linux-host-unreachable",
                    format!(
                        "the {} run on {} stopped before finishing",
                        job.plan.id, self.destination
                    ),
                ));
            }
        }
        let swept = kv(&self.ssh(&["sweep", &lease.root, &lease.allocation], None)?)
            .get("swept")
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(0);
        if swept > 0 {
            record_leak(job, &format!("{swept} on {}", self.destination))?;
        }
        let bundle = job.out_dir.join("remote-out.tar");
        let bytes = self.ssh(&["collect", &lease.root, &lease.allocation], None)?;
        std::fs::write(&bundle, bytes).map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
        let unpacked = job.out_dir.join("remote-out");
        safe_extract(&bundle, &unpacked)
            .map_err(|e| Stop::new("native-result-invalid", e.message))?;
        keep_evidence(&unpacked, &job.out_dir)?;
        self.discard(job, &lease)?;
        if let Some(leak) = recorded_leak(job) {
            return Err(leak);
        }
        durable(job).map(Some).ok_or_else(|| {
            Stop::new(
                "native-result-invalid",
                format!("{} returned no result.json", job.plan.id),
            )
        })
    }
}

/// Download pinned inputs into the private content-addressed cache and
/// return verified local copies (local host only; the SSH host fetches its
/// own by public URL and digest, so no file or credential is relayed).
pub fn fetch_downloads(
    forge: &dyn super::forge::Forge,
    cache: &Path,
    plan: &NativePlan,
) -> Step<Vec<(Download, PathBuf)>> {
    // The cache owns its directory: a fresh state root (release-doctor's
    // scratch, a first attempt) has none yet, and it must be private.
    private_dir(cache).map_err(|e| {
        Stop::new(
            "state-corrupt",
            format!("download cache {}: {e:#}", cache.display()),
        )
    })?;
    let mut out = Vec::new();
    for d in plan.downloads() {
        if !is_sha256(&d.sha256) || !d.url.starts_with("https://") {
            return Err(Stop::new(
                "pin-pending",
                format!("{} has no published pin", d.id),
            ));
        }
        let file = cache.join(&d.sha256);
        // A verified marker (size recorded at verification) avoids rehashing
        // large fixtures on every poll; any size change forces a rehash.
        let marker = cache.join(format!("{}.verified", d.sha256));
        let size = std::fs::metadata(&file).map(|m| m.len()).ok();
        let marked = size.is_some_and(|s| {
            std::fs::read_to_string(&marker).is_ok_and(|m| m.trim() == s.to_string())
        });
        let cached = file.exists() && sha256_file(&file).ok().as_deref() == Some(d.sha256.as_str());
        if !(marked || cached) {
            let part = cache.join(format!("{}.part", d.sha256));
            forge
                .download_public(&d.url, &part)
                .map_err(|e| Stop::new("pin-unreachable", format!("{}: {}", d.id, e.message)))?;
            if sha256_file(&part)? != d.sha256 {
                let _ = std::fs::remove_file(&part);
                return Err(Stop::new(
                    "pin-unreachable",
                    format!("{} downloaded bytes differ from the pin", d.id),
                ));
            }
            std::fs::rename(&part, &file).map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
        }
        if !marked {
            let len = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            std::fs::write(&marker, len.to_string())
                .map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
        }
        out.push((d, file));
    }
    Ok(out)
}

/// Production host factory. The SSH control socket lives in a private,
/// short temporary directory owned by this process (removed on exit).
pub struct RealHosts {
    pub control: tempfile::TempDir,
}

impl RealHosts {
    pub fn new() -> anyhow::Result<RealHosts> {
        Ok(RealHosts {
            control: tempfile::Builder::new().prefix("awr-").tempdir_in("/tmp")?,
        })
    }
}

impl super::world::Hosts for RealHosts {
    fn sbx_qualification(&self) -> Step<super::sbx::Qualification> {
        Ok(super::sbx::probe()?)
    }
    fn host(&self, plan: &NativePlan, linux_host: Option<&str>) -> Step<Box<dyn NativeHost>> {
        match plan.host.as_str() {
            "local" => Ok(Box::new(LocalHost::new())),
            _ => {
                let destination = linux_host.ok_or_else(|| {
                    Stop::new(
                        "linux-host-missing",
                        format!(
                            "{} needs a Linux KVM host and no LINUX_HOST is set",
                            plan.id
                        ),
                    )
                })?;
                Ok(Box::new(SshHost {
                    destination: destination.into(),
                    control_dir: self.control.path().to_path_buf(),
                    program: PathBuf::from("ssh"),
                }))
            }
        }
    }
}

fn cleanup_runtime_allocation(job: &NativeJob, root: &Path) -> Step<bool> {
    if [
        crate::manifest::release_policy::APPLE_NATIVE,
        crate::manifest::release_policy::DOCKER_NATIVE,
    ]
    .contains(&job.plan.id.as_str())
        && root.join("out/runtime-owned.json").exists()
    {
        // Execute only the kit whose complete digest inventory was attested.
        check_kit(&job.kit_dir, &job.plan, &job.commit)?;
        let mut c = Command::new(job.kit_dir.join("bin/xtask"));
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
        c.args(["test", "kit-reconcile", "--kit"])
            .arg(&job.kit_dir)
            .arg("--out")
            .arg(root.join("out"))
            .arg("--tmproot")
            .arg(root.join("tmp"));
        let output = super::util::output_bounded(&mut c, None, std::time::Duration::from_secs(120))
            .map_err(|e| {
                Stop::new(
                    "native-result-invalid",
                    format!("owned runtime cleanup: {e}"),
                )
            })?;
        if !output.status.success() {
            return Err(Stop::new(
                "native-result-invalid",
                "owned runtime object could not be removed",
            ));
        }
        let count = kv(&output.stdout)
            .get("leaked")
            .and_then(|n| n.parse::<u64>().ok())
            .ok_or_else(|| Stop::new("native-result-invalid", "missing owned cleanup receipt"))?;
        return Ok(count > 0);
    }
    if job.plan.id != crate::manifest::release_policy::SBX_NATIVE {
        return Ok(false);
    }
    super::sbx::cleanup(&root.join("out"), &root.join("tmp")).map_err(|e| {
        Stop::new(
            "native-result-invalid",
            format!("owned SBX cleanup failed: {e:#}"),
        )
    })
}

fn runtime_deadline_code(job: &NativeJob) -> &'static str {
    if [
        crate::manifest::release_policy::APPLE_NATIVE,
        crate::manifest::release_policy::DOCKER_NATIVE,
    ]
    .contains(&job.plan.id.as_str())
    {
        "native-failed"
    } else {
        "timeout"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_with(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, kind, body) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(*kind);
            h.set_size(body.len() as u64);
            h.set_mode(0o755);
            if *kind == tar::EntryType::Symlink {
                h.set_link_name("/etc/passwd").unwrap();
            }
            // Bypass the builder's own path checks to model a hostile archive.
            let bytes = name.as_bytes();
            h.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
            h.set_cksum();
            b.append(&h, *body).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn kit_extraction_refuses_traversal_links_and_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let bad_cases: Vec<Vec<u8>> = vec![
            tar_with(&[("../escape", tar::EntryType::Regular, b"x")]),
            tar_with(&[("/abs", tar::EntryType::Regular, b"x")]),
            tar_with(&[("link", tar::EntryType::Symlink, b"")]),
            tar_with(&[("dev", tar::EntryType::Char, b"")]),
            tar_with(&[
                ("a", tar::EntryType::Regular, b"x"),
                ("a", tar::EntryType::Regular, b"y"),
            ]),
        ];
        for (i, bytes) in bad_cases.iter().enumerate() {
            let p = tmp.path().join(format!("k{i}.tar"));
            std::fs::write(&p, bytes).unwrap();
            let err = safe_extract(&p, &tmp.path().join(format!("out{i}"))).unwrap_err();
            assert_eq!(err.code, "kit-invalid", "{i}: {err}");
        }
        assert!(!tmp.path().join("escape").exists());
        let good = tmp.path().join("good.tar");
        std::fs::write(
            &good,
            tar_with(&[
                ("bin/xtask", tar::EntryType::Regular, b"#!"),
                (
                    "payload/target/debug/deps/artifact-123",
                    tar::EntryType::Regular,
                    b"test",
                ),
                (
                    "payload/target/debug/awman-fake-cli",
                    tar::EntryType::Regular,
                    b"fake",
                ),
                ("workspace/untrusted", tar::EntryType::Regular, b"no exec"),
                ("kit.json", tar::EntryType::Regular, b"{}"),
            ]),
        )
        .unwrap();
        let out = tmp.path().join("good");
        safe_extract(&good, &out).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(out.join("bin/xtask"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(out.join("kit.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            for (name, mode) in [
                ("payload/target/debug/deps/artifact-123", 0o700),
                ("payload/target/debug/awman-fake-cli", 0o700),
                ("workspace/untrusted", 0o600),
            ] {
                assert_eq!(
                    std::fs::metadata(out.join(name))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    mode
                );
            }
        }
    }

    #[test]
    fn remote_arguments_are_single_quoted_and_the_socket_root_fits() {
        assert_eq!(shell_quote("a'b; rm -rf /"), "'a'\\''b; rm -rf /'");
        let mac = LocalHost {
            tmp_parent: PathBuf::from("/private/tmp"),
            children: Default::default(),
        };
        assert!(mac.socket_budget_ok(DEFAULT_SOCKET_SUFFIX));
        let long = LocalHost {
            tmp_parent: PathBuf::from(format!("/private/var/folders/{}", "x".repeat(60))),
            children: Default::default(),
        };
        assert!(!long.socket_budget_ok(DEFAULT_SOCKET_SUFFIX));
    }

    // ── fixtures for the host adapters ───────────────────────────────────────

    use crate::release::forge_dir;
    use crate::release::world::World;
    use std::time::Duration;

    fn contract_plan() -> serde_json::Value {
        serde_json::from_slice(include_bytes!(
            "../../tests/release_contract/fixtures/release-plan.json"
        ))
        .unwrap()
    }

    /// A seeded fault world (virtual clock, public pinned inputs).
    fn seeded(dir: &Path) -> (World, super::super::plan::Plan) {
        let store = forge_dir::seed(dir, "v0.15.0", contract_plan());
        let plan = serde_json::from_value(store.plans["default"].clone()).unwrap();
        (forge_dir::world(dir, Default::default()), plan)
    }

    /// A job whose kit `bin/xtask` is a shell stand-in: `pass` writes a
    /// result, `hang` runs until killed, `leak` passes but leaves an owned
    /// child behind. Each start appends to `<root>/starts`.
    fn job(tmp: &Path, plan: &super::super::plan::Plan, host: &str, mode: &str) -> NativeJob {
        let mut native = plan.native.iter().find(|n| n.host == host).unwrap().clone();
        native.fixtures.clear();
        native.previous_release = None;
        let kit = tmp.join("kit");
        std::fs::create_dir_all(kit.join("bin")).unwrap();
        std::fs::write(
            kit.join("bin/xtask"),
            r#"#!/bin/sh
echo started >> "$AWMAN_KIT_OUT/../starts"
case "$(cat "$0.mode")" in
hang) exec sleep 300 ;;
leak) sleep 300 & echo $! > "$AWMAN_KIT_OUT/leaked" ;;
esac
printf '{}' > "$AWMAN_KIT_OUT/result.json"
"#,
        )
        .unwrap();
        std::fs::write(kit.join("bin/xtask.mode"), mode).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                kit.join("bin/xtask"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let kit_tar = tmp.join("kit.tar");
        let mut b = tar::Builder::new(std::fs::File::create(&kit_tar).unwrap());
        b.append_dir_all(".", &kit).unwrap();
        b.finish().unwrap();
        std::fs::write(tmp.join("asset"), "asset").unwrap();
        let out_dir = tmp.join("state");
        private_dir(&out_dir).unwrap();
        NativeJob {
            plan: native,
            commit: "R".into(),
            tree: "T".into(),
            asset: tmp.join("asset"),
            asset_sha256: "a".repeat(64),
            kit_tar,
            kit_sha256: "b".repeat(64),
            manifest_sha256: "c".repeat(64),
            plan_sha256: "d".repeat(64),
            policy_sha256: "e".repeat(64),
            kit_dir: kit,
            downloads: vec![],
            out_dir,
            deadline_seconds: 600,
        }
    }

    /// A live, non-zombie process.
    fn running(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| s.rsplit_once(") ").map(|(_, rest)| !rest.starts_with('Z')))
            .unwrap_or(false)
    }

    fn poll_until_finished(
        host: &dyn NativeHost,
        job: &NativeJob,
        w: &World,
    ) -> Step<Option<(PathBuf, i32)>> {
        awman_test_support::wait::wait_for(
            "the native run to finish",
            Duration::from_secs(30),
            || match host.poll(job, w) {
                Ok(None) => None,
                other => Some(other),
            },
        )
    }

    #[test]
    fn a_fresh_download_cache_is_created_private_before_downloading() {
        let tmp = tempfile::tempdir().unwrap();
        let (w, plan) = seeded(&tmp.path().join("world"));
        let native = plan.native.iter().find(|n| n.host == "local").unwrap();
        // release-doctor passes `<fresh scratch>/fixtures`, which does not exist.
        let cache = tmp.path().join("scratch/fixtures");
        let got = fetch_downloads(w.forge.as_ref(), &cache, native).unwrap();
        assert_eq!(got.len(), native.downloads().len());
        for (d, file) in &got {
            assert_eq!(sha256_file(file).unwrap(), d.sha256);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cache).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_stalled_remote_command_is_stopped_at_its_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let ssh = tmp.path().join("ssh");
        // A connected host whose command never finishes.
        std::fs::write(&ssh, "#!/bin/sh\nexec sleep 300\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let host = SshHost {
            destination: "build@kvm1".into(),
            control_dir: tmp.path().to_path_buf(),
            program: ssh,
        };
        let began = std::time::Instant::now();
        let err = host
            .ssh_within(&["status"], None, Duration::from_millis(300))
            .unwrap_err();
        assert_eq!(err.code, "linux-host-unreachable");
        assert!(err.message.contains("deadline"), "{err}");
        assert!(began.elapsed() < Duration::from_secs(20));
        for op in [
            "probe", "query", "alloc", "fixture", "receive", "start", "status", "sweep", "collect",
            "kill", "cleanup",
        ] {
            assert!(ssh_deadline(op) <= Duration::from_secs(1800), "{op}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_lost_start_response_is_adopted_on_resume_not_started_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let (w, plan) = seeded(&tmp.path().join("world"));
        let job = job(tmp.path(), &plan, "ssh", "hang");
        // A local stand-in for ssh that runs the supervisor with a private
        // HOME and, while `lose-start` exists, drops the start response.
        let ssh = tmp.path().join("ssh");
        let fake = tmp.path().display().to_string();
        std::fs::write(
            &ssh,
            format!(
                r#"#!/bin/sh
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done; shift
if [ -e '{fake}/lose-start' ]; then
  case "$1" in *"'sup' 'start'"*) HOME='{fake}' sh -c "$1" >/dev/null; exit 255;; esac
fi
HOME='{fake}' exec sh -c "$1"
"#
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(tmp.path().join("lose-start"), "").unwrap();
        let host = SshHost {
            destination: "build@kvm1".into(),
            control_dir: tmp.path().to_path_buf(),
            program: ssh,
        };
        let err = host.start(&job, &w).unwrap_err();
        assert_eq!(err.code, "linux-host-unreachable", "{err}");
        assert!(!lease_path(&job).exists());
        let intent: Intent = read_json(&intent_path(&job)).unwrap();
        let root = PathBuf::from(&intent.root);
        let first = read_handshake(&std::fs::read(root.join("started")).unwrap()).unwrap();

        std::fs::remove_file(tmp.path().join("lose-start")).unwrap();
        host.start(&job, &w).unwrap();
        let lease: Lease = read_json(&lease_path(&job)).unwrap();
        assert_eq!(lease.allocation, intent.allocation);
        assert_eq!((lease.pid, lease.start.clone()), first);
        assert!(!intent_path(&job).exists());
        assert_eq!(
            std::fs::read_to_string(root.join("starts")).unwrap(),
            "started\n"
        );
        assert!(host.poll(&job, &w).unwrap().is_none());

        w.clock.sleep(job.deadline_seconds + 1);
        let err = host.poll(&job, &w).unwrap_err();
        assert_eq!(err.code, "timeout", "{err}");
        assert!(!root.exists());
        assert!(!lease_path(&job).exists());
        awman_test_support::wait::wait_for(
            "the timed-out run to stop",
            Duration::from_secs(10),
            || (!running(lease.pid)).then_some(()),
        );
    }

    fn local_host(tmp: &Path) -> LocalHost {
        let parent = tmp.join("alloc");
        private_dir(&parent).unwrap();
        LocalHost {
            tmp_parent: parent,
            children: Default::default(),
        }
    }

    fn allocations(host: &LocalHost) -> Vec<String> {
        std::fs::read_dir(&host.tmp_parent)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_controller_killed_before_its_lease_is_adopted_not_restarted() {
        let tmp = tempfile::tempdir().unwrap();
        let (w, plan) = seeded(&tmp.path().join("world"));
        let job = job(tmp.path(), &plan, "local", "hang");
        let first = local_host(tmp.path());
        first.start(&job, &w).unwrap();
        let lease: Lease = read_json(&lease_path(&job)).unwrap();
        // The state a controller killed between spawn and lease leaves.
        std::fs::remove_file(lease_path(&job)).unwrap();
        let intent = Intent {
            allocation: lease.allocation.clone(),
            host: "local".into(),
            root: lease.root.clone(),
            boot_id: lease.boot_id.clone(),
        };
        write_json(&intent_path(&job), &intent).unwrap();

        let resumed = LocalHost {
            tmp_parent: first.tmp_parent.clone(),
            children: Default::default(),
        };
        resumed.start(&job, &w).unwrap();
        let adopted: Lease = read_json(&lease_path(&job)).unwrap();
        assert_eq!(
            (&adopted.allocation, adopted.pid, &adopted.start),
            (&lease.allocation, lease.pid, &lease.start)
        );
        assert_eq!(allocations(&resumed).len(), 1);
        let root = PathBuf::from(&lease.root);
        assert_eq!(
            std::fs::read_to_string(root.join("starts")).unwrap(),
            "started\n"
        );
        assert!(resumed.poll(&job, &w).unwrap().is_none());

        w.clock.sleep(job.deadline_seconds + 1);
        assert_eq!(resumed.poll(&job, &w).unwrap_err().code, "timeout");
        assert!(allocations(&resumed).is_empty());
        awman_test_support::wait::wait_for(
            "the timed-out run to stop",
            Duration::from_secs(10),
            || (!running(lease.pid)).then_some(()),
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_allocation_without_a_handshake_is_cleaned_and_started_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (w, plan) = seeded(&tmp.path().join("world"));
        let job = job(tmp.path(), &plan, "local", "pass");
        let host = local_host(tmp.path());
        // Killed after allocating, before the wrapper's handshake.
        let intent = load_intent(&job, "local", local_boot_id(), |a| {
            host.allocation_root(a).to_string_lossy().into()
        })
        .unwrap();
        std::fs::create_dir(&intent.root).unwrap();
        std::fs::write(Path::new(&intent.root).join("owner"), &intent.allocation).unwrap();

        host.start(&job, &w).unwrap();
        let lease: Lease = read_json(&lease_path(&job)).unwrap();
        assert_eq!(lease.allocation, intent.allocation);
        assert_eq!(allocations(&host).len(), 1, "{:?}", allocations(&host));
        let (result, exit) = poll_until_finished(&host, &job, &w).unwrap().unwrap();
        assert_eq!(exit, 0);
        assert!(result.starts_with(&job.out_dir));
        assert!(allocations(&host).is_empty());
        assert!(!lease_path(&job).exists() && !intent_path(&job).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_owned_process_outliving_a_passing_run_voids_the_result() {
        let tmp = tempfile::tempdir().unwrap();
        let (w, plan) = seeded(&tmp.path().join("world"));
        let job = job(tmp.path(), &plan, "local", "leak");
        let host = local_host(tmp.path());
        host.start(&job, &w).unwrap();
        let err = poll_until_finished(&host, &job, &w).unwrap_err();
        assert_eq!(err.code, "native-result-invalid", "{err}");
        let leaked: u32 = std::fs::read_to_string(job.out_dir.join("leaked"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        awman_test_support::wait::wait_for(
            "the leaked child to be stopped",
            Duration::from_secs(10),
            || (!running(leaked)).then_some(()),
        );
        assert!(allocations(&host).is_empty());
        // The verdict is durable: a resumed controller does not accept it.
        let resumed = local_host(tmp.path());
        resumed.start(&job, &w).unwrap();
        assert_eq!(
            resumed.poll(&job, &w).unwrap_err().code,
            "native-result-invalid"
        );
    }
}
