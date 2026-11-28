//! Release SBX smoke: fake `sbx`, `ssh` and `awman` executables owned by the
//! test sandbox. No real SBX CLI, SSH host, provider agent or login is used.
use super::*;
use awman_test_support::{TestCase, TestSandbox};
use std::sync::atomic::Ordering;

const FAKE_SBX: &str = r#"#!/bin/sh
D='@STATE@'
printf '%s\n' "$*" >> "$D/calls"
if test "$1" = --version; then
    test -f "$D/fail-version" && exit 1
    printf 'sbx version %s\n' "$(cat "$D/version" 2>/dev/null || echo 0.54.1)"
    exit 0
fi
test "$1" = --no-input && shift
cmd=$1
shift
test -f "$D/hang-$cmd" && sleep 30
test -f "$D/fail-$cmd" && exit 1
case "$cmd" in
ls)
    test -f "$D/signed-out" && exit 1
    count=$(cat "$D/ls-count" 2>/dev/null || echo 0); count=$((count + 1)); printf '%s' "$count" > "$D/ls-count"
    test "$(cat "$D/fail-ls-at" 2>/dev/null || echo 0)" -ne "$count" || exit 1
    printf '['
    sep=
    for f in "$D"/sandboxes/*; do
        test -e "$f" || continue
        printf '%s{"name":"%s"}' "$sep" "${f##*/}"
        sep=,
    done
    printf ']\n'
    ;;
create)
    test -e "$D/sandboxes/$2" && exit 1
    printf '%s\n' "$4" > "$D/sandboxes/$2"
    ;;
inspect) printf '{"name":"%s"}\n' "$1" ;;
exec) echo exec-output-witness; echo exec-error-witness >&2; cp "$2/.host-probe" "$2/.guest-probe" ;;
run)
    ws=$(cat "$D/sandboxes/$2")
    cp "$ws/.host-probe" "$ws/.guest-probe"
    ;;
stop) ;;
rm) test -f "$D/rm-noop" || rm -f "$D/sandboxes/$2" ;;
*) exit 64 ;;
esac
"#;

const FAKE_AWMAN: &str = r#"#!/bin/sh
D='@STATE@'
if test "$1" = --version; then echo 'awman 9.9.9-test'; exit 0; fi
printf '%s\n' "$*" > "$D/ready-argv"
env | sed -n 's/^\(AWMAN_[A-Za-z0-9_]*\)=.*/\1/p' | sort > "$D/ready-env"
cat "$AWMAN_HOME/config.json" > "$D/ready-config"
command -v sbx > "$D/ready-sbx"
pwd -P > "$D/ready-cwd"
if test -f "$D/ready-creates"; then
    n=$(sed -n 's/.*"ready_sandbox": *"\([^"]*\)".*/\1/p' ../../intent.json)
    sbx --no-input create --name "$n" shell "$PWD"
fi
test -f "$D/ready-hang" && sleep 30
if test -f "$D/ready-output"; then cat "$D/ready-output"; else printf '{"ready":true,"runtime":"docker-sbx-experimental"}\n'; fi
exit "$(cat "$D/ready-exit" 2>/dev/null || echo 0)"
"#;

/// Joins the remote words like ssh does and runs them in a local `sh`, with
/// the fake `sbx` first on PATH. Destinations are recorded, never resolved.
const FAKE_SSH: &str = r#"#!/bin/sh
D='@STATE@'
while test $# -gt 0; do
    case "$1" in
    -T) shift ;;
    -o) shift 2 ;;
    *) break ;;
    esac
done
printf '%s\n' "$1" >> "$D/ssh-dest"
shift
test -f "$D/ssh-unreachable" && exit 255
PATH='@BIN@':$PATH
TMPDIR='@TMP@'
export PATH TMPDIR
if test -f "$D/git-routing"; then
    GIT_DIR=$(sed -n '1p' "$D/git-routing")
    GIT_WORK_TREE=$(sed -n '2p' "$D/git-routing")
    export GIT_DIR GIT_WORK_TREE
fi
remote=$*
if test -f "$D/force-setsid-fork"; then
    case "$remote" in
    setsid\ *) remote="setsid --fork ${remote#setsid }" ;;
    esac
fi
exec sh -c "$remote"
"#;

/// A fake agent: if anything ran it on the host, the marker would appear.
const FAKE_CLAUDE: &str = "#!/bin/sh\ntouch '@STATE@/claude-ran'\n";

struct Fixture {
    s: TestSandbox,
    state: PathBuf,
    sbx: PathBuf,
    ssh: PathBuf,
    asset: PathBuf,
}

fn host_triple() -> String {
    match std::env::consts::OS {
        "macos" => format!("{}-apple-darwin", std::env::consts::ARCH),
        _ => format!("{}-unknown-linux-gnu", std::env::consts::ARCH),
    }
}

fn install(s: &mut TestSandbox, name: &str, dir: &Path, text: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    fs::write(&path, text).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    s.register_exe(name, &path);
    path
}

fn fixture(id: &'static str) -> Fixture {
    let mut s = TestSandbox::for_test(TestCase::component(id));
    let state = s.work().join("fake-state");
    fs::create_dir_all(state.join("sandboxes")).unwrap();
    fs::write(state.join("calls"), b"").unwrap();
    let fill = |t: &str| {
        t.replace("@STATE@", state.to_str().unwrap())
            .replace("@BIN@", s.bin().to_str().unwrap())
            .replace("@TMP@", s.tmp().to_str().unwrap())
    };
    let (bin, sbx_text, ssh_text, claude_text, awman_text) = (
        s.bin(),
        fill(FAKE_SBX),
        fill(FAKE_SSH),
        fill(FAKE_CLAUDE),
        fill(FAKE_AWMAN),
    );
    let sbx = install(&mut s, "sbx", &bin, &sbx_text);
    let ssh = install(&mut s, "ssh", &bin, &ssh_text);
    install(&mut s, "claude", &bin, &claude_text);
    let asset_dir = s.work().join("assets");
    fs::create_dir_all(&asset_dir).unwrap();
    let asset = install(&mut s, "awman", &asset_dir, &awman_text);
    Fixture {
        s,
        state,
        sbx,
        ssh,
        asset,
    }
}

impl Fixture {
    fn binding(&self, id: &str, destination: Option<&str>) -> SbxSmokeBinding {
        let selection = select("sbx.macos-arm64", "sbx.linux-x64", destination).unwrap();
        SbxSmokeBinding {
            schema: SCHEMA,
            obligation: id.into(),
            triple: host_triple(),
            commit: "c".repeat(40),
            tree: "t".repeat(40),
            manifest_sha256: "1".repeat(64),
            plan_sha256: "2".repeat(64),
            policy_sha256: "3".repeat(64),
            asset: "awman-test-asset".into(),
            asset_sha256: digest::sha256_file(&self.asset).unwrap(),
            selection_sha256: selection.sha256,
            destination: destination.map(str::to_owned),
            steps: STEPS.iter().map(|s| (*s).to_owned()).collect(),
            ready_agent: "claude".into(),
            min_cli_version: "0.54.0".into(),
            lifecycle_budget_seconds: 600,
            ready_budget_seconds: 420,
            budget_seconds: 1080,
        }
    }
    fn job_with(&self, dir: &str, target: Target, binding: SbxSmokeBinding) -> SbxSmokeJob {
        let out = self.s.work().join("sbx-smoke").join(dir);
        private_dir(&out).unwrap();
        SbxSmokeJob {
            binding,
            target,
            asset_path: self.asset.clone(),
            out_dir: out,
            interrupted: Arc::new(AtomicBool::new(false)),
            sbx_program: self.sbx.clone(),
            ssh_program: self.ssh.clone(),
            remote_tmp: self.s.tmp().to_string_lossy().into_owned(),
        }
    }
    fn mac(&self, dir: &str) -> SbxSmokeJob {
        self.job_with(dir, Target::Local, self.binding("sbx.macos-arm64", None))
    }
    fn linux(&self, dir: &str) -> SbxSmokeJob {
        self.job_with(
            dir,
            Target::Ssh {
                destination: "user@build-host".into(),
            },
            self.binding("sbx.linux-x64", Some("user@build-host")),
        )
    }
    fn flag(&self, name: &str) {
        fs::write(self.state.join(name), b"").unwrap();
    }
    fn clear(&self, name: &str) {
        let _ = fs::remove_file(self.state.join(name));
    }
    fn write(&self, name: &str, text: &str) {
        fs::write(self.state.join(name), text).unwrap();
    }
    fn calls(&self) -> String {
        fs::read_to_string(self.state.join("calls")).unwrap()
    }
    fn sandboxes(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.state.join("sandboxes"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
    fn finish(self) {
        assert!(
            !self.state.join("claude-ran").exists(),
            "an agent executable ran on the host"
        );
        self.s.finish();
    }
}

fn reason(report: &SbxSmokeReport) -> (Outcome, &str) {
    (report.outcome, report.reason_code.as_deref().unwrap_or(""))
}

#[cfg(target_os = "linux")]
fn linux_failure_details(job: &SbxSmokeJob, report: &SbxSmokeReport) -> String {
    let lifecycle = job.out_dir.join("lifecycle");
    let read = |name: &str| {
        fs::read_to_string(lifecycle.join(name))
            .unwrap_or_else(|error| format!("<unavailable: {error}>"))
    };
    format!(
        "reason={:?}; ssh-exit={}; ssh-log={}; receipt-bytes={:?}; reconcile={}",
        report.reason,
        read("ssh-exit.txt"),
        read("ssh.log"),
        fs::metadata(lifecycle.join("receipt.json")).map(|metadata| metadata.len()),
        fs::read_to_string(job.out_dir.join("reconcile.json"))
            .unwrap_or_else(|error| format!("<unavailable: {error}>")),
    )
}

/// Rewrite a report after tampering, refreshing its file list so only the
/// semantic check under test can reject it.
fn rewrite(job: &SbxSmokeJob, edit: impl FnOnce(&mut SbxSmokeReport)) {
    let path = job.out_dir.join("report.json");
    let mut report: SbxSmokeReport = read_json(&path).unwrap();
    edit(&mut report);
    report.files = files_except(&job.out_dir, "report.json").unwrap();
    write_json(&path, &report).unwrap();
}

// ───────────────────────────── pure ─────────────────────────────

#[test]
fn selection_is_mac_only_unless_a_linux_host_is_explicit() {
    let mac = select("sbx.macos-arm64", "sbx.linux-x64", None).unwrap();
    assert_eq!(mac.hosts.len(), 1);
    assert_eq!(mac.hosts[0].host, "local");
    assert_eq!(mac.hosts[0].destination, None);
    let both = select("sbx.macos-arm64", "sbx.linux-x64", Some("user@kvm1")).unwrap();
    assert_eq!(
        both.hosts[1],
        SbxHostSelection {
            id: "sbx.linux-x64".into(),
            host: "ssh".into(),
            destination: Some("user@kvm1".into())
        }
    );
    assert_eq!(both.sha256, SbxSelection::digest(&both.hosts).unwrap());
    assert_eq!(
        both,
        select("sbx.macos-arm64", "sbx.linux-x64", Some("user@kvm1")).unwrap(),
        "the digest is stable"
    );
    assert_ne!(mac.sha256, both.sha256);
    assert_ne!(
        both.sha256,
        select("sbx.macos-arm64", "sbx.linux-x64", Some("user@kvm2"))
            .unwrap()
            .sha256
    );
    for bad in ["-oProxyCommand=x", "a@b;c", "user@$(x)", ""] {
        assert!(select("sbx.macos-arm64", "sbx.linux-x64", Some(bad)).is_err());
    }
}

#[test]
fn a_frozen_selection_never_silently_adds_drops_or_substitutes_a_host() {
    let sel = |host: Option<&str>| select("sbx.macos-arm64", "sbx.linux-x64", host).unwrap();
    let none = BTreeSet::new();
    let linux_passed: BTreeSet<String> = ["sbx.linux-x64".to_owned()].into();
    // Mac only, unchanged.
    assert!(sel(None).admits(&sel(None), &none).is_ok());
    // Mac only, LINUX_HOST now set: blocked with the host named.
    let added = sel(None).admits(&sel(Some("h")), &none).unwrap_err();
    assert!(
        added.contains("Mac only") && added.contains("LINUX_HOST=h"),
        "{added}"
    );
    // Mac + h, same host.
    assert!(sel(Some("h")).admits(&sel(Some("h")), &none).is_ok());
    // Mac + h, LINUX_HOST unset: only a verified Linux pass may continue.
    let dropped = sel(Some("h")).admits(&sel(None), &none).unwrap_err();
    assert!(dropped.contains("LINUX_HOST=h"), "{dropped}");
    assert!(sel(Some("h")).admits(&sel(None), &linux_passed).is_ok());
    // Mac + h, another host: blocked even when h already passed.
    for passed in [&none, &linux_passed] {
        let other = sel(Some("h")).admits(&sel(Some("g")), passed).unwrap_err();
        assert!(
            other.contains("LINUX_HOST=g") && other.contains("LINUX_HOST=h"),
            "{other}"
        );
    }
}

#[test]
fn receipt_fields_decode_and_reject_malformed_base64() {
    assert_eq!(b64_decode("").unwrap(), b"");
    assert_eq!(b64_decode("Zg==").unwrap(), b"f");
    assert_eq!(b64_decode("Zm8=").unwrap(), b"fo");
    assert_eq!(b64_decode("Zm9vYmFy").unwrap(), b"foobar");
    assert_eq!(b64_decode("W10K").unwrap(), b"[]\n");
    for bad in ["Zg=", "Z===", "Zg==Zg==", "Zm9*"] {
        assert!(b64_decode(bad).is_err(), "{bad}");
    }
    assert_eq!(b64_field(&Value::Null).unwrap(), None);
    assert!(b64_field(&json!(1)).is_err());
}

#[test]
fn readiness_output_must_report_the_sbx_runtime() {
    let ok = ready_result(
        "claude",
        Some(0),
        br#"{"ready":true,"runtime":"docker-sbx-experimental"}"#,
    );
    assert!(ok.ready && ok.runtime_kind.as_deref() == Some("sbx"));
    assert_eq!(
        ok.argv,
        ["ready", "--agent", "claude", "--prepare-only", "--json"]
    );
    let docker = ready_result("claude", Some(0), br#"{"ready":true,"runtime":"docker"}"#);
    assert_eq!(docker.runtime_kind.as_deref(), Some("docker"));
    let noisy = ready_result(
        "claude",
        Some(0),
        b"progress\n{\"ready\":false,\"runtime\":\"docker-sbx-experimental\"}\n",
    );
    assert!(!noisy.ready);
    assert!(!ready_result("claude", Some(0), b"not json").ready);
    assert_eq!(
        sandbox_name(Path::new("/tmp/x/repo"), "claude"),
        sandbox_name(Path::new("/tmp/x/repo"), "claude")
    );
    assert!(sandbox_name(Path::new("/r"), "claude").starts_with("awman-"));
    assert!(sandbox_name(Path::new("/r"), "claude").ends_with("-claude"));
}

// ───────────────────────────── Mac (local target) ─────────────────────────────

#[test]
fn local_smoke_passes_binds_its_inputs_and_is_adopted_on_rerun() {
    let f = fixture("sbx_validation::release::tests::local_smoke_passes_binds_its_inputs_and_is_adopted_on_rerun");
    f.write("sandboxes/user-sandbox", "/elsewhere\n");
    let job = f.mac("pass");
    let report = run(&job).unwrap();
    assert_eq!(
        reason(&report),
        (Outcome::Passed, ""),
        "{:?}",
        report.reason
    );
    assert!(report.sandbox.starts_with("awman-release-"));
    assert_eq!(report.steps.len(), STEPS.len());
    assert_eq!(report.cli.as_ref().unwrap().version, "0.54.1");
    assert_eq!(
        report.cli.as_ref().unwrap().sha256,
        digest::sha256_file(&f.sbx).unwrap()
    );
    assert_eq!(report.asset_version.as_deref(), Some("awman 9.9.9-test"));
    assert_eq!(report.limits, LIMITS);
    let record = verify_report(&job).unwrap();
    assert_eq!(record.outcome, "passed");
    assert_eq!(record.binding_sha256, job.binding.sha256().unwrap());
    assert_eq!(record.selection_sha256, job.binding.selection_sha256);
    assert_eq!(
        record.report_sha256,
        digest::sha256_file(&job.out_dir.join("report.json")).unwrap()
    );
    // Production readiness path: fixed argv, private state only, the
    // fingerprinted sbx, no AWMAN_TEST_* or other awman variable.
    assert_eq!(
        fs::read_to_string(f.state.join("ready-argv")).unwrap(),
        "ready --agent claude --prepare-only --json\n"
    );
    assert_eq!(
        fs::read_to_string(f.state.join("ready-env")).unwrap(),
        "AWMAN_HOME\n"
    );
    assert_eq!(
        fs::read_to_string(f.state.join("ready-config")).unwrap(),
        READY_CONFIG
    );
    assert_eq!(
        Path::new(
            fs::read_to_string(f.state.join("ready-sbx"))
                .unwrap()
                .trim()
        ),
        f.sbx
    );
    let cwd = fs::read_to_string(f.state.join("ready-cwd")).unwrap();
    assert_eq!(
        sandbox_name(Path::new(cwd.trim()), "claude"),
        report.ready_sandbox
    );
    assert!(
        !job.out_dir.join("ready/repo").exists(),
        "scratch repository removed"
    );
    assert!(
        !job.out_dir.join("ready/awman-home").exists(),
        "private state removed"
    );
    // Only this check's sandbox was created; the user's survives untouched.
    assert_eq!(f.sandboxes(), ["user-sandbox"]);
    let calls = f.calls();
    assert!(calls.contains(&format!(
        "--no-input create --name {} shell ",
        report.sandbox
    )));
    for forbidden in ["login", "logout", "secret", "--cloud", "user-sandbox"] {
        assert!(!calls.contains(forbidden), "{forbidden} in {calls}");
    }
    // Idempotent: a verifying report is adopted without touching the CLI.
    let again = run(&job).unwrap();
    assert_eq!(again, report);
    assert_eq!(f.calls(), calls);
    f.finish();
}

#[test]
fn local_prerequisites_block_before_any_sandbox_is_created() {
    let f = fixture(
        "sbx_validation::release::tests::local_prerequisites_block_before_any_sandbox_is_created",
    );
    let mut missing = f.mac("missing");
    missing.sbx_program = f.s.work().join("no-such-sbx");
    let report = run(&missing).unwrap();
    assert_eq!(reason(&report), (Outcome::Blocked, "sbx-cli-missing"));
    assert_eq!(verify_report(&missing).unwrap().outcome, "blocked");
    for (i, (flag, code)) in [
        ("fail-version", "sbx-cli-missing"),
        ("version", "sbx-cli-too-old"),
        ("signed-out", "sbx-not-signed-in"),
    ]
    .into_iter()
    .enumerate()
    {
        if flag == "version" {
            f.write("version", "0.53.9");
        } else {
            f.flag(flag);
        }
        let job = f.mac(&format!("blocked-{i}"));
        let report = run(&job).unwrap();
        assert_eq!(reason(&report), (Outcome::Blocked, code));
        assert!(
            !job.out_dir.join("intent.json").exists(),
            "no mutation, no intent"
        );
        f.clear(flag);
    }
    assert!(!f.calls().contains(" create "), "{}", f.calls());
    f.finish();
}

#[test]
fn a_blocked_report_is_retried_and_its_evidence_retained() {
    let f = fixture(
        "sbx_validation::release::tests::a_blocked_report_is_retried_and_its_evidence_retained",
    );
    f.flag("signed-out");
    let job = f.mac("retry");
    assert_eq!(run(&job).unwrap().outcome, Outcome::Blocked);
    f.clear("signed-out");
    assert_eq!(run(&job).unwrap().outcome, Outcome::Passed);
    let retained: Vec<_> = fs::read_dir(job.out_dir.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("retry-blocked-"))
        .collect();
    assert_eq!(retained.len(), 1, "the blocked attempt is kept aside");
    f.finish();
}

#[test]
fn lifecycle_round_trip_and_cleanup_failures_fail_and_only_the_owned_sandbox_is_removed() {
    let f = fixture("sbx_validation::release::tests::lifecycle_round_trip_and_cleanup_failures_fail_and_only_the_owned_sandbox_is_removed");
    f.write("sandboxes/user-sandbox", "/elsewhere\n");
    for (i, (flag, code)) in [
        ("fail-exec", "lifecycle"),
        ("fail-run", "lifecycle"),
        ("fail-stop", "lifecycle"),
        ("fail-rm", "cleanup"),
        ("rm-noop", "cleanup"),
    ]
    .into_iter()
    .enumerate()
    {
        f.flag(flag);
        let job = f.mac(&format!("fault-{i}"));
        let report = run(&job).unwrap();
        assert_eq!(
            reason(&report),
            (Outcome::Failed, code),
            "{flag}: {:?}",
            report.reason
        );
        assert_eq!(verify_report(&job).unwrap().outcome, "failed");
        assert!(
            report.ready.is_none(),
            "readiness never follows a failed lifecycle"
        );
        f.clear(flag);
        if flag == "rm-noop" || flag == "fail-rm" {
            // The fake really kept it; remove it for the next case.
            let _ = fs::remove_file(f.state.join("sandboxes").join(&report.sandbox));
        }
        assert_eq!(f.sandboxes(), ["user-sandbox"], "{flag}");
        // A recorded failure is final: rerunning returns it unchanged.
        assert_eq!(run(&job).unwrap(), report);
    }
    // A passing exit with no guest witness is a round-trip failure.
    fs::write(
        &f.sbx,
        fs::read_to_string(&f.sbx).unwrap().replace(
            "exec) echo exec-output-witness; echo exec-error-witness >&2; cp \"$2/.host-probe\" \"$2/.guest-probe\" ;;",
            "exec) echo exec-output-witness; echo exec-error-witness >&2; ;;",
        ),
    )
    .unwrap();
    let job = f.mac("no-witness");
    let report = run(&job).unwrap();
    assert_eq!(reason(&report), (Outcome::Failed, "lifecycle"));
    assert_eq!(
        report.steps.get("exec"),
        Some(&0),
        "exec exited 0 but left no witness"
    );
    assert_eq!(f.sandboxes(), ["user-sandbox"]);
    f.finish();
}

#[test]
fn unreadable_post_creation_cleanup_is_failed_not_retryable_blocked() {
    let f = fixture("sbx_validation::release::tests::unreadable_post_creation_cleanup_is_failed_not_retryable_blocked");
    f.write("fail-ls-at", "5");
    let job = f.mac("cleanup-listing-fails");
    let report = run(&job).unwrap();
    assert_eq!(reason(&report), (Outcome::Failed, "cleanup"));
    assert!(
        job.out_dir.join("intent.json").exists(),
        "ownership intent remains for reconciliation"
    );
    assert_eq!(
        run(&job).unwrap(),
        report,
        "a failed post-create cleanup remains final"
    );
    f.finish();
}

#[test]
fn readiness_failures_fail_and_its_sandbox_is_removed_and_verified() {
    let f = fixture("sbx_validation::release::tests::readiness_failures_fail_and_its_sandbox_is_removed_and_verified");
    for (i, (file, text)) in [
        ("ready-exit", "1"),
        (
            "ready-output",
            r#"{"ready":false,"runtime":"docker-sbx-experimental"}"#,
        ),
        ("ready-output", r#"{"ready":true,"runtime":"docker"}"#),
        ("ready-output", "garbage"),
    ]
    .into_iter()
    .enumerate()
    {
        f.write(file, text);
        let job = f.mac(&format!("ready-{i}"));
        let report = run(&job).unwrap();
        assert_eq!(
            reason(&report),
            (Outcome::Failed, "readiness"),
            "{file}={text}"
        );
        f.clear(file);
    }
    // The attested digest differs from the bytes on disk: never executed.
    let _ = fs::remove_file(f.state.join("ready-argv"));
    let mut binding = f.binding("sbx.macos-arm64", None);
    binding.asset_sha256 = "f".repeat(64);
    let job = f.job_with("ready-digest", Target::Local, binding);
    assert_eq!(reason(&run(&job).unwrap()), (Outcome::Failed, "readiness"));
    assert!(
        !f.state.join("ready-argv").exists(),
        "a mismatched asset never runs"
    );
    assert!(f.sandboxes().is_empty());
    f.finish();
}

#[test]
fn readiness_created_sandbox_is_removed_and_verified() {
    let f = fixture(
        "sbx_validation::release::tests::readiness_created_sandbox_is_removed_and_verified",
    );
    // awman ready creating its production sandbox is fine: it is removed
    // and verified absent, and the check still passes.
    f.flag("ready-creates");
    let job = f.mac("ready-creates");
    let report = run(&job).unwrap();
    assert_eq!(
        reason(&report),
        (Outcome::Passed, ""),
        "{:?}",
        report.reason
    );
    assert!(f
        .calls()
        .contains(&format!("--no-input rm --force {}", report.ready_sandbox)));
    assert!(f.sandboxes().is_empty());
    f.clear("ready-creates");
    f.finish();
}

#[test]
fn an_unclaimed_readiness_name_collision_is_never_reconciled_as_owned() {
    let f = fixture("sbx_validation::release::tests::an_unclaimed_readiness_name_collision_is_never_reconciled_as_owned");
    let job = f.mac("ready-collision");
    let repo = job.out_dir.join("ready/repo");
    let name = sandbox_name(&repo, "claude");
    f.write(&format!("sandboxes/{name}"), "/unrelated\n");
    let report = run(&job).unwrap();
    assert_eq!(reason(&report), (Outcome::Blocked, "sbx-name-occupied"));
    assert!(
        f.state.join("sandboxes").join(name).exists(),
        "an unclaimed pre-existing sandbox is preserved"
    );
    assert_eq!(f.sandboxes().len(), 1);
    f.finish();
}

#[test]
fn a_hung_step_is_killed_at_its_deadline_and_never_passes() {
    let f = fixture(
        "sbx_validation::release::tests::a_hung_step_is_killed_at_its_deadline_and_never_passes",
    );
    f.flag("hang-exec");
    let mut binding = f.binding("sbx.macos-arm64", None);
    binding.lifecycle_budget_seconds = 2;
    let job = f.job_with("hang-lifecycle", Target::Local, binding);
    let started = Instant::now();
    let report = run(&job).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "deadline not enforced"
    );
    assert_eq!(reason(&report), (Outcome::Failed, "timeout"));
    assert!(
        f.sandboxes().is_empty(),
        "cleanup still ran after the deadline"
    );
    f.clear("hang-exec");
    f.flag("ready-hang");
    let mut binding = f.binding("sbx.macos-arm64", None);
    binding.ready_budget_seconds = 2;
    let job = f.job_with("hang-ready", Target::Local, binding);
    let report = run(&job).unwrap();
    assert_eq!(reason(&report), (Outcome::Failed, "timeout"));
    assert!(report.outcome != Outcome::Passed);
    f.clear("ready-hang");
    f.finish();
}

#[test]
fn interruption_and_resume_reconcile_only_this_checks_resources() {
    let f = fixture("sbx_validation::release::tests::interruption_and_resume_reconcile_only_this_checks_resources");
    // An operator interrupt during a hung step: killed, own sandbox removed,
    // intent kept, no report.
    f.flag("hang-exec");
    let job = f.mac("interrupt");
    let flag = job.interrupted.clone();
    let calls = f.state.join("calls");
    let stopper = std::thread::spawn(move || {
        // Interrupt once the hanging `exec` step has started.
        awman_test_support::wait_for("the hung exec step", Duration::from_secs(30), || {
            fs::read_to_string(&calls)
                .ok()
                .filter(|c| c.contains("--no-input exec "))
                .map(|_| ())
        });
        flag.store(true, Ordering::SeqCst);
    });
    let error = run(&job).unwrap_err();
    stopper.join().unwrap();
    assert!(error.to_string().starts_with("interrupted"), "{error:#}");
    assert!(job.out_dir.join("intent.json").exists());
    assert!(!job.out_dir.join("report.json").exists());
    assert!(f.sandboxes().is_empty());
    f.clear("hang-exec");
    // A killed controller left its sandboxes behind next to a user's.
    let intent: Intent = read_json(&job.out_dir.join("intent.json")).unwrap();
    for name in [&intent.sandbox, &"user-sandbox".to_owned()] {
        f.write(&format!("sandboxes/{name}"), "/x\n");
    }
    job.interrupted.store(false, Ordering::SeqCst);
    let report = run(&job).unwrap();
    assert_eq!(
        reason(&report),
        (Outcome::Passed, ""),
        "{:?}",
        report.reason
    );
    assert_ne!(report.sandbox, intent.sandbox, "a fresh identity is used");
    assert_eq!(
        f.sandboxes(),
        ["user-sandbox"],
        "only owned names were removed"
    );
    let aside: Vec<_> = fs::read_dir(job.out_dir.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("interrupt-interrupted-")
        })
        .collect();
    assert_eq!(aside.len(), 1);
    let resume: Value = read_json(&aside[0].join("resume-reconcile.json")).unwrap();
    assert_eq!(resume["removed"], json!([intent.sandbox]));
    // A resume whose listing is unreadable keeps the intent and blocks.
    let job2 = f.mac("unverifiable");
    write_json(
        &job2.out_dir.join("intent.json"),
        &Intent {
            obligation: job2.binding.obligation.clone(),
            ..intent.clone()
        },
    )
    .unwrap();
    f.flag("signed-out");
    let report = run(&job2).unwrap();
    assert_eq!(reason(&report), (Outcome::Blocked, "sbx-not-signed-in"));
    assert!(job2.out_dir.join("intent.json").exists());
    f.clear("signed-out");
    // Removal that does not take effect is a cleanup failure, not a pass.
    let job3 = f.mac("leak");
    write_json(&job3.out_dir.join("intent.json"), &intent).unwrap();
    f.write(&format!("sandboxes/{}", intent.sandbox), "/x\n");
    f.flag("rm-noop");
    assert!(reconcile(&job3).is_err());
    assert_eq!(reason(&run(&job3).unwrap()), (Outcome::Failed, "cleanup"));
    f.clear("rm-noop");
    // A foreign intent is never acted on.
    let job4 = f.mac("foreign");
    write_json(
        &job4.out_dir.join("intent.json"),
        &Intent {
            sandbox: "user-sandbox".into(),
            ..intent.clone()
        },
    )
    .unwrap();
    assert!(reconcile(&job4).is_err());
    assert!(f.sandboxes().contains(&"user-sandbox".to_owned()));
    for name in f.sandboxes() {
        fs::remove_file(f.state.join("sandboxes").join(name)).unwrap();
    }
    f.finish();
}

#[test]
fn verification_rejects_unbound_tampered_short_or_leaking_evidence() {
    let f = fixture("sbx_validation::release::tests::verification_rejects_unbound_tampered_short_or_leaking_evidence");
    let job = f.mac("verify");
    let original = run(&job).unwrap();
    assert_eq!(original.outcome, Outcome::Passed);
    let report_bytes = fs::read(job.out_dir.join("report.json")).unwrap();
    let restore = || fs::write(job.out_dir.join("report.json"), &report_bytes).unwrap();
    // Unbound: any binding input differs.
    for edit in [
        (|b: &mut SbxSmokeBinding| b.commit = "d".repeat(40)) as fn(&mut SbxSmokeBinding),
        |b| b.asset_sha256 = "e".repeat(64),
        |b| b.selection_sha256 = "9".repeat(64),
        |b| b.plan_sha256 = "8".repeat(64),
    ] {
        let mut other = f.mac("verify");
        other.out_dir = job.out_dir.clone();
        edit(&mut other.binding);
        assert!(verify_report(&other).is_err());
        assert!(
            run(&other).is_err(),
            "a foreign binding never reuses the directory"
        );
    }
    // Modified raw evidence.
    let probe = job.out_dir.join("lifecycle/workspace/.guest-probe");
    let saved = fs::read(&probe).unwrap();
    fs::write(&probe, b"forged\n").unwrap();
    assert!(verify_report(&job).is_err());
    rewrite(&job, |_| {});
    assert!(
        verify_report(&job).is_err(),
        "a forged witness fails even when re-hashed"
    );
    fs::write(&probe, &saved).unwrap();
    restore();
    verify_report(&job).unwrap();
    // Short, duplicated or failed steps.
    for edit in [
        (|r: &mut SbxSmokeReport| {
            r.steps.remove("run");
        }) as fn(&mut SbxSmokeReport),
        |r| {
            r.steps.insert("run".into(), 1);
        },
        |r| {
            r.steps.insert("extra".into(), 0);
        },
        |r| r.finished = r.started + chrono::Duration::seconds(5000),
        |r| r.ready.as_mut().unwrap().runtime_kind = Some("docker".into()),
        |r| r.cli_after.as_mut().unwrap().sha256 = "0".repeat(64),
        |r| r.limits = "Everything is proven.".into(),
        |r| r.reason_code = Some("x".into()),
    ] {
        rewrite(&job, edit);
        assert!(verify_report(&job).is_err());
        restore();
    }
    // Missing step log.
    let log = job.out_dir.join("lifecycle/run.stderr");
    fs::remove_file(&log).unwrap();
    rewrite(&job, |_| {});
    assert!(verify_report(&job).is_err());
    fs::write(&log, b"").unwrap();
    restore();
    // A leaked sandbox in the audit listing.
    let audit_path = job.out_dir.join("reconcile.json");
    let mut audit: Value = read_json(&audit_path).unwrap();
    audit["listing"] = json!([]);
    audit["leaked"] = json!(1);
    write_json(&audit_path, &audit).unwrap();
    rewrite(&job, |_| {});
    assert!(
        verify_report(&job).is_err(),
        "a nonzero leak count cannot pass with an empty listing"
    );
    restore();
    audit = read_json(&audit_path).unwrap();
    audit["listing"] = json!([{"name": original.sandbox}]);
    write_json(&audit_path, &audit).unwrap();
    rewrite(&job, |_| {});
    assert!(verify_report(&job).is_err());
    f.finish();
}

#[test]
fn local_probe_classifies_missing_old_and_signed_out_cli() {
    let f = fixture(
        "sbx_validation::release::tests::local_probe_classifies_missing_old_and_signed_out_cli",
    );
    let probe = |sbx: &Path| probe_with(&Target::Local, "0.54.0", sbx, &f.ssh, LINUX_ARCH, true);
    let identity = probe(&f.sbx).unwrap();
    assert_eq!(identity.version, "0.54.1");
    assert_eq!(identity.sha256, digest::sha256_file(&f.sbx).unwrap());
    assert_eq!(
        probe(&f.s.work().join("absent")).unwrap_err().0,
        "sbx-cli-missing"
    );
    f.write("version", "0.53.0");
    assert_eq!(probe(&f.sbx).unwrap_err().0, "sbx-cli-too-old");
    f.clear("version");
    f.flag("signed-out");
    assert_eq!(probe(&f.sbx).unwrap_err().0, "sbx-not-signed-in");
    // Identity alone does not need a session.
    probe_with(&Target::Local, "0.0.0", &f.sbx, &f.ssh, LINUX_ARCH, false).unwrap();
    f.clear("signed-out");
    assert!(!f.calls().contains("create"));
    f.finish();
}

// ───────────────────────────── Linux over SSH ─────────────────────────────

#[cfg(target_os = "linux")]
#[test]
fn linux_smoke_over_ssh_passes_and_leaves_nothing_on_the_host() {
    let f = fixture("sbx_validation::release::tests::linux_smoke_over_ssh_passes_and_leaves_nothing_on_the_host");
    f.flag("force-setsid-fork");
    f.write("sandboxes/user-sandbox", "/elsewhere\n");
    let job = f.linux("linux");
    let report = run(&job).unwrap();
    assert_eq!(
        reason(&report),
        (Outcome::Passed, ""),
        "{}",
        linux_failure_details(&job, &report)
    );
    assert!(
        fs::read_to_string(job.out_dir.join("remote-logs/exec.stdout"))
            .unwrap()
            .contains("exec-output-witness")
    );
    assert!(
        fs::read_to_string(job.out_dir.join("remote-logs/exec.stderr"))
            .unwrap()
            .contains("exec-error-witness")
    );
    let record = verify_report(&job).unwrap();
    assert_eq!(record.destination.as_deref(), Some("user@build-host"));
    assert_eq!(report.host["os"], "linux");
    assert_eq!(report.host["arch"], std::env::consts::ARCH);
    assert_eq!(
        report.asset_sha256.as_deref(),
        Some(job.binding.asset_sha256.as_str())
    );
    let root = remote_root(&job, &report.sandbox);
    assert!(
        !Path::new(&root).exists(),
        "the owned remote root is removed"
    );
    assert_eq!(f.sandboxes(), ["user-sandbox"]);
    assert_eq!(
        fs::read_to_string(f.state.join("ready-env")).unwrap(),
        "AWMAN_HOME\n"
    );
    assert_eq!(
        fs::read_to_string(f.state.join("ready-cwd"))
            .unwrap()
            .trim(),
        format!("{root}/repo")
    );
    let dests = fs::read_to_string(f.state.join("ssh-dest")).unwrap();
    assert!(dests.lines().all(|d| d == "user@build-host"), "{dests}");
    // Tampering: an aarch64 receipt or another destination never verifies.
    let receipt_path = job.out_dir.join("lifecycle/receipt.json");
    let saved = fs::read(&receipt_path).unwrap();
    let mut receipt: Value = serde_json::from_slice(&saved).unwrap();
    receipt["target"] = json!(if std::env::consts::ARCH == "x86_64" {
        "linux/aarch64"
    } else {
        "linux/x86_64"
    });
    fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    rewrite(&job, |_| {});
    assert!(verify_report(&job).is_err());
    fs::write(&receipt_path, &saved).unwrap();
    rewrite(&job, |_| {});
    verify_report(&job).unwrap();
    let mut other = f.linux("linux");
    other.target = Target::Ssh {
        destination: "user@other".into(),
    };
    other.binding.destination = Some("user@other".into());
    assert!(verify_report(&other).is_err());
    f.finish();
}

#[cfg(target_os = "linux")]
#[test]
fn linux_smoke_ignores_ambient_remote_git_routing() {
    let f =
        fixture("sbx_validation::release::tests::linux_smoke_ignores_ambient_remote_git_routing");
    let sentinel = f.s.work().join("sentinel-repo");
    let init =
        f.s.command("git")
            .args(["init", "--quiet", sentinel.to_str().unwrap()])
            .run();
    init.assert_success();
    let git_dir = sentinel.join(".git");
    fs::write(
        f.state.join("git-routing"),
        format!("{}\n{}\n", git_dir.display(), sentinel.display()),
    )
    .unwrap();

    let job = f.linux("ambient-git-routing");
    let report = run(&job).unwrap();
    assert_eq!(
        reason(&report),
        (Outcome::Passed, ""),
        "{}",
        linux_failure_details(&job, &report)
    );
    assert!(
        !sentinel.join(".git/HEAD.lock").exists(),
        "the external repository was not selected for initialization"
    );
    f.finish();
}

#[test]
#[cfg(target_os = "linux")]
fn remote_reconciliation_stops_a_term_resistant_worker_before_removing_scratch() {
    use awman_test_support::{Ready, Stdin};
    let f = fixture("sbx_validation::release::tests::remote_reconciliation_stops_a_term_resistant_worker_before_removing_scratch");
    let name = "awman-release-0123456789abcdef01234567";
    let job = f.linux("worker-fence");
    let root = PathBuf::from(remote_root(&job, name));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("owner"), name).unwrap();
    let mut worker =
        f.s.command("sh")
            .args(["-s", "--", "run", name])
            .stdin(Stdin::Bytes(
                b"echo worker-ready\ntrap '' TERM\nwhile :; do sleep 1; done\n".to_vec(),
            ))
            .ready(Ready::StdoutLine("worker-ready".into()))
            .deadline(Duration::from_secs(30))
            .spawn();
    let stat = fs::read_to_string(format!("/proc/{}/stat", worker.pid())).unwrap();
    let start = stat.split_whitespace().nth(21).unwrap();
    fs::write(
        root.join("worker"),
        format!("{} {} {}\n", worker.pid(), start, name),
    )
    .unwrap();
    let result =
        f.s.command("sh")
            .args(["-s", "--", "remove", name, root.to_str().unwrap()])
            .stdin(Stdin::Bytes(SCRIPT.as_bytes().to_vec()))
            .deadline(Duration::from_secs(20))
            .run();
    result.assert_success();
    assert!(!root.exists(), "owned scratch root was not removed");
    assert_ne!(
        worker.wait(Duration::from_secs(2)),
        awman_test_support::Exit::Code(0),
        "TERM-resistant worker survived reconciliation"
    );
    f.finish();
}

#[cfg(target_os = "linux")]
#[test]
fn linux_unreachable_signed_out_and_occupied_hosts_block_without_mutation() {
    let f = fixture("sbx_validation::release::tests::linux_unreachable_signed_out_and_occupied_hosts_block_without_mutation");
    f.flag("ssh-unreachable");
    let job = f.linux("unreachable");
    assert_eq!(
        reason(&run(&job).unwrap()),
        (Outcome::Blocked, "linux-host-unreachable")
    );
    assert!(
        job.out_dir.join("intent.json").exists(),
        "intent kept for the next resume"
    );
    let error = probe_with(
        &job.target,
        "0.54.0",
        &f.sbx,
        &f.ssh,
        std::env::consts::ARCH,
        true,
    )
    .unwrap_err();
    assert_eq!(error.0, "linux-host-unreachable");
    f.clear("ssh-unreachable");
    // Reachable again: the earlier intent reconciles (its root never
    // existed) and the retry passes.
    let report = run(&job).unwrap();
    assert_eq!(
        report.outcome,
        Outcome::Passed,
        "{}",
        linux_failure_details(&job, &report)
    );
    fs::write(f.state.join("calls"), b"").unwrap();
    f.flag("signed-out");
    let job = f.linux("signed-out");
    let report = run(&job).unwrap();
    assert_eq!(
        reason(&report),
        (Outcome::Blocked, "sbx-not-signed-in"),
        "{:?}",
        report.reason
    );
    assert_eq!(
        probe_with(
            &job.target,
            "0.54.0",
            &f.sbx,
            &f.ssh,
            std::env::consts::ARCH,
            true
        )
        .unwrap_err()
        .0,
        "sbx-not-signed-in"
    );
    f.clear("signed-out");
    f.write("version", "0.53.0");
    let job = f.linux("too-old");
    assert_eq!(
        reason(&run(&job).unwrap()),
        (Outcome::Blocked, "sbx-cli-too-old")
    );
    f.clear("version");
    assert!(!f.calls().contains(" create "), "{}", f.calls());
    let identity = probe_with(
        &job.target,
        "0.54.0",
        &f.sbx,
        &f.ssh,
        std::env::consts::ARCH,
        true,
    )
    .unwrap();
    assert_eq!(identity.sha256, digest::sha256_file(&f.sbx).unwrap());
    // A remote failure after create fails and still removes the sandbox.
    f.flag("fail-run");
    let job = f.linux("remote-fail");
    let report = run(&job).unwrap();
    assert_eq!(
        reason(&report),
        (Outcome::Failed, "lifecycle"),
        "{:?}",
        report.reason
    );
    assert!(f.sandboxes().is_empty());
    f.clear("fail-run");
    // A foreign directory at the owned root path is never removed.
    let job = f.linux("foreign-root");
    let intent = Intent {
        schema: 1,
        obligation: job.binding.obligation.clone(),
        sandbox: "awman-release-0123456789abcdef01234567".into(),
        ready_sandbox: "awman-00000000-claude".into(),
        sandbox_claimed: false,
        ready_claimed: false,
        nonce_sha256: "0".repeat(64),
        workspace: "/w".into(),
        remote_root: Some(remote_root(&job, "awman-release-0123456789abcdef01234567")),
        destination: job.binding.destination.clone(),
        started: Utc::now(),
        deadline: Utc::now(),
    };
    let foreign = PathBuf::from(intent.remote_root.clone().unwrap());
    fs::create_dir_all(&foreign).unwrap();
    fs::write(foreign.join("owner"), "somebody-else").unwrap();
    write_json(&job.out_dir.join("intent.json"), &intent).unwrap();
    assert!(!reconcile(&job).unwrap(), "nothing of ours was there");
    assert!(
        foreign.join("owner").exists(),
        "a foreign root is left untouched"
    );
    fs::remove_dir_all(&foreign).unwrap();
    f.finish();
}
