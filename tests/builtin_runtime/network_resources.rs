//! Guest network policy and resource limits (`builtin_hw_*`, D-05).
//!
//! Every guest test boots real VMs through `examples/builtin_net_driver`
//! (`network_resources/driver.rs`) and first passes the ordinary hardware
//! gate (`AWMAN_TEST_BUILTIN=1`, KVM / the hypervisor entitlement, a feature
//! binary, the fixture archive) plus its own opt-in:
//!
//! * `AWMAN_TEST_BUILTIN_NETWORK=1` for tests that need outbound internet
//!   from the runner (public DNS/HTTPS). Host-local services are started by
//!   the test itself on the host loopback.
//! * `AWMAN_TEST_BUILTIN_PRESSURE=1` for the bounded guest OOM test (a
//!   256 MiB guest exhausts its own memory; the host is never pressured).
//!
//! A test whose prerequisites are missing reports SKIP/BLOCKED and asserts
//! nothing about a guest; under `AWMAN_TEST_BUILTIN_REQUIRE_HW=1` that is a
//! failure. The guest scripts print `PASS <check>` / `FAIL <check> ...` lines
//! and the host decides: a check that never printed is a failure too.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::gate::{decide, hypervisor, Decision, Facts};

const TAG: &str = "awman-hw/fixture:latest";
/// Bearer token the guest presents to the host-local service. Synthetic: it
/// proves the header crosses the network boundary unmodified.
const SYNTHETIC_TOKEN: &str = "awman-synthetic-token-7f3c";

/// A name the runner can resolve and fetch over HTTPS.
fn allowed_name() -> String {
    std::env::var("AWMAN_TEST_BUILTIN_NET_ALLOWED").unwrap_or_else(|_| "example.com".into())
}

/// A second public name that allowlist tests must NOT reach.
fn denied_name() -> String {
    std::env::var("AWMAN_TEST_BUILTIN_NET_DENIED").unwrap_or_else(|_| "example.org".into())
}

pub fn net_driver_path() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_awman"))
        .parent()
        .expect("target dir")
        .join("examples")
        .join("builtin_net_driver")
}

fn truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Extra opt-ins a network/resource test needs beyond the hardware gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    Hardware,
    Internet,
    Pressure,
}

/// Pure policy: the hardware decision first (its SKIP/BLOCKED wins), then
/// this test's own opt-in, which is a SKIP when absent.
pub fn decide_net(facts: &Facts, need: Need, internet: bool, pressure: bool) -> Decision {
    match decide(facts, true) {
        Decision::Run => {}
        other => return other,
    }
    match need {
        Need::Internet if !internet => Decision::Skip(
            "AWMAN_TEST_BUILTIN_NETWORK=1 is not set (needs outbound internet from the runner)"
                .into(),
        ),
        Need::Pressure if !pressure => Decision::Skip(
            "AWMAN_TEST_BUILTIN_PRESSURE=1 is not set (bounded guest OOM opt-in)".into(),
        ),
        _ => Decision::Run,
    }
}

fn record(test: &str, outcome: &str, detail: &str) {
    eprintln!("{outcome}: {test}: {detail}");
    if let Ok(path) = std::env::var("AWMAN_TEST_BUILTIN_REPORT") {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{outcome}\t{test}\t{}", detail.replace('\n', " "));
        }
    }
}

fn gate(test: &str, need: Need) -> Option<PathBuf> {
    let driver = net_driver_path();
    let facts = Facts {
        gate_on: truthy("AWMAN_TEST_BUILTIN"),
        hypervisor: hypervisor(&driver),
        binary_has_builtin: crate::binary::has_builtin_runtime(),
        driver_built: driver.is_file(),
        archive: crate::hardware::fixture_archive().map(|p| p.to_string_lossy().into_owned()),
    };
    let decision = decide_net(
        &facts,
        need,
        truthy("AWMAN_TEST_BUILTIN_NETWORK"),
        truthy("AWMAN_TEST_BUILTIN_PRESSURE"),
    );
    let (outcome, reason) = match decision {
        Decision::Run => {
            record(test, "RUN", "prerequisites satisfied");
            return crate::hardware::fixture_archive();
        }
        Decision::Skip(reason) => ("SKIP", reason),
        Decision::Blocked(reason) => ("BLOCKED", reason),
    };
    record(test, outcome, &reason);
    if truthy("AWMAN_TEST_BUILTIN_REQUIRE_HW") {
        panic!("{test}: hardware is required (AWMAN_TEST_BUILTIN_REQUIRE_HW=1) but: {reason}");
    }
    None
}

/// A host-loopback HTTP service standing in for a host-local MCP endpoint.
/// `/auth` answers `auth-ok` only for the synthetic bearer token.
struct HostService {
    port: u16,
    hits: Arc<AtomicUsize>,
}

impl HostService {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind host service");
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < 16 * 1024 {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
                let body = if text.starts_with("get /auth ") {
                    if text.contains(&format!("authorization: bearer {SYNTHETIC_TOKEN}")) {
                        "auth-ok"
                    } else {
                        "auth-missing"
                    }
                } else {
                    "host-ok"
                };
                let _ = write!(
                    stream,
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        HostService { port, hits }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

/// Shell prelude for guest scripts: `ok NAME CMD...` expects success,
/// `refused NAME CMD...` expects failure. Every command is time-bounded so a
/// silently dropped flow fails instead of hanging.
const PRELUDE: &str = r#"
for executable in timeout wget grep head tr mktemp; do command -v "$executable" >/dev/null 2>&1 || { echo "FAIL probe-setup missing $executable"; exit 77; }; done
bounded() { case "$1" in fetch) shift; timeout 20 wget -T 8 -O - "$@";; *) timeout 20 "$@";; esac; }
ok() { n=$1; shift; if bounded "$@" >"$probe_output" 2>&1; then echo "PASS $n"; else echo "FAIL $n $(head -c 200 "$probe_output" | tr '\n' ' ')"; fi; }
refused() { n=$1; shift; bounded "$@" >"$probe_output" 2>&1; status=$?; case "$status" in 0) echo "FAIL $n unexpectedly succeeded";; 125|126|127) echo "FAIL $n probe could not execute ($status)";; *) if grep -Eiq 'unrecognized option|invalid option|applet not found|command not found|certificate|unknown ca|401 Unauthorized|403 Forbidden|404 Not Found' "$probe_output"; then echo "FAIL $n setup, TLS trust or HTTP failure is not a policy refusal"; else echo "PASS $n"; fi;; esac; }
has() { n=$1; want=$2; shift 2; out=$(bounded "$@" 2>&1); status=$?; if [ "$status" != 0 ]; then echo "FAIL $n probe failed ($status)"; else case "$out" in *"$want"*) echo "PASS $n";; *) echo "FAIL $n got: $(printf %s "$out" | head -c 200 | tr '\n' ' ')";; esac; fi; }
is() { n=$1; want=$2; shift 2; out=$(bounded "$@" 2>&1); status=$?; if [ "$status" = 0 ] && [ "$out" = "$want" ]; then echo "PASS $n"; else echo "FAIL $n got: $(printf %s "$out" | head -c 200 | tr '\n' ' ')"; fi; }
only_lo() { got=$(ls /sys/class/net | tr '\n' ' '); if [ "$got" = "lo " ]; then echo "PASS $1"; else echo "FAIL $1 interfaces: $got"; fi; }
probe_output=$(mktemp)
trap 'rm -f "$probe_output"' EXIT
"#;

#[test]
fn network_probe_executes_fetch_and_rejects_missing_tools() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let wget = dir.path().join("wget");
    std::fs::write(&wget, "#!/bin/sh\nprintf fetch-executed\\n\n").unwrap();
    std::fs::set_permissions(&wget, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new("sh")
        .arg("-c")
        .arg(script("ok positive fetch ignored\nrefused negative fetch ignored\nrefused missing awman-nonexistent-probe-0121"))
        .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
        .output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("PASS positive"), "{stdout}");
    assert!(stdout.contains("FAIL negative"), "{stdout}");
    assert!(stdout.contains("FAIL missing"), "{stdout}");
    assert!(!stdout.contains("PASS negative") && !stdout.contains("PASS missing"));
}

#[test]
fn network_probe_rejects_tls_tool_and_http_failures_as_denial() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let wget = dir.path().join("wget");
    for diagnostic in [
        "certificate verify failed",
        "unrecognized option --header",
        "HTTP/1.1 401 Unauthorized",
        "404 Not Found",
    ] {
        std::fs::write(
            &wget,
            format!("#!/bin/sh\necho '{diagnostic}' >&2\nexit 1\n"),
        )
        .unwrap();
        std::fs::set_permissions(&wget, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new("sh")
            .arg("-c")
            .arg(script("refused negative fetch ignored"))
            .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("FAIL negative"), "{diagnostic}: {stdout}");
        assert!(!stdout.contains("PASS negative"));
    }
}

fn script(body: &str) -> String {
    format!("{PRELUDE}\n{body}\n")
}

/// Parsed `PASS`/`FAIL` lines of one guest.
#[derive(Debug, Default)]
pub struct Checks {
    pub passed: BTreeSet<String>,
    pub failed: Vec<String>,
}

pub fn parse_checks(stdout: &str) -> Checks {
    let mut checks = Checks::default();
    for line in stdout.lines() {
        if let Some(name) = line.strip_prefix("PASS ") {
            checks.passed.insert(name.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("FAIL ") {
            checks.failed.push(rest.trim().to_owned());
        }
    }
    checks
}

/// Every expected check passed and nothing failed.
pub fn check_problems(checks: &Checks, expected: &[&str]) -> Vec<String> {
    let mut problems: Vec<String> = checks.failed.iter().map(|f| format!("FAIL {f}")).collect();
    for name in expected {
        if !checks.passed.contains(*name) {
            problems.push(format!("missing {name}"));
        }
    }
    problems
}

struct Run {
    work: tempfile::TempDir,
    facts: BTreeMap<String, Vec<String>>,
    raw: String,
}

impl Run {
    fn stdout(&self, vm: &str) -> String {
        std::fs::read_to_string(self.work.path().join(format!("{vm}.stdout"))).unwrap_or_default()
    }
    fn exit(&self, vm: &str) -> Option<i32> {
        self.facts.get("EXIT")?.iter().find_map(|line| {
            let (name, code) = line.split_once('\t')?;
            (name == vm).then(|| code.parse().ok())?
        })
    }
    fn leftover(&self) -> Option<usize> {
        self.facts.get("LEFTOVER")?.first()?.parse().ok()
    }
    fn expect(&self, vm: &str, expected: &[&str]) {
        let problems = check_problems(&parse_checks(&self.stdout(vm)), expected);
        assert!(
            problems.is_empty(),
            "{vm}: {problems:#?}\n--- driver ---\n{}",
            self.raw
        );
    }
}

fn vm(name: &str, vcpus: u8, memory_mib: u32, network: Value, body: &str) -> Value {
    json!({"name": name, "vcpus": vcpus, "memoryMib": memory_mib,
           "network": network, "script": script(body)})
}

/// Run a plan through the driver. `env` is added to an otherwise empty
/// environment (no PATH, no HOME of the developer).
fn run_plan(test: &str, archive: &Path, plan: Value, env: &[(&str, &str)]) -> Run {
    let state = tempfile::Builder::new()
        .prefix("awn.")
        .tempdir_in("/tmp")
        .unwrap();
    let work = tempfile::Builder::new()
        .prefix("awman-net-")
        .tempdir_in("/tmp")
        .unwrap();
    let plan_path = work.path().join("plan.json");
    std::fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();
    let mut command = Command::new(net_driver_path());
    command
        .env_clear()
        .env("HOME", work.path().join("home"))
        .env("TMPDIR", work.path());
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command
        .arg("--state-dir")
        .arg(state.path().join("s"))
        .arg("--archive")
        .arg(archive)
        .args(["--tag", TAG, "--work"])
        .arg(work.path())
        .arg("--plan")
        .arg(&plan_path)
        .output()
        .expect("spawn the network driver");
    let raw = format!(
        "{}\n--stderr--\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let code = output.status.code().unwrap_or(-1);
    // Past the gate the host was judged capable: a driver BLOCKED here is a
    // failure, not a quiet skip.
    assert_eq!(code, 0, "{test}: driver failed ({code}): {raw}");
    let mut facts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((key, value)) = line.split_once('\t') {
            facts
                .entry(key.to_owned())
                .or_default()
                .push(value.to_owned());
        }
    }
    assert!(facts.contains_key("IMPORT"), "{test}: {raw}");
    Run { work, facts, raw }
}

/// Default (`public`) policy with one authorised host port: public DNS and
/// HTTPS with the image's own CA bundle, an authenticated request to the
/// host-local service through the host alias with its header intact, and
/// refusals for an unauthorised host port, guest loopback (never the host),
/// the controlled host endpoint. Private/LAN and metadata enforcement needs
/// a separately provisioned reachable endpoint; an arbitrary unrouted address
/// is not a positive control. The host's proxy variables are set on
/// the driver and must not affect guest traffic.
#[test]
fn builtin_hw_network_dns_auth_ca_proxy_mcp() {
    let test = "builtin_hw_network_dns_auth_ca_proxy_mcp";
    let Some(archive) = gate(test, Need::Internet) else {
        return;
    };
    let mcp = HostService::start();
    let other = HostService::start();
    let allowed = allowed_name();
    let (p, q) = (mcp.port, other.port);
    let body = format!(
        r#"
ok dns-public nslookup {allowed}
ok https-public-image-ca fetch https://{allowed}/
has mcp-host-alias host-ok fetch http://host.microsandbox.internal:{p}/
has api-auth-header-intact auth-ok fetch --header "Authorization: Bearer {SYNTHETIC_TOKEN}" http://host.microsandbox.internal:{p}/auth
refused host-port-not-authorised fetch http://host.microsandbox.internal:{q}/
refused guest-loopback-is-not-host fetch http://127.0.0.1:{p}/
"#
    );
    let control = format!(
        "has denied-port-positive-control host-ok fetch http://host.microsandbox.internal:{q}/"
    );
    let plan = json!({"phases": [
        [vm("control", 1, 384, json!({"hostPorts": [q]}), &control)],
        [vm("public", 2, 512, json!({"hostPorts": [p]}), &body)]
    ]});
    let run = run_plan(
        test,
        &archive,
        plan,
        &[
            ("HTTPS_PROXY", "http://127.0.0.1:9"),
            ("HTTP_PROXY", "http://127.0.0.1:9"),
            ("ALL_PROXY", "socks5://127.0.0.1:9"),
        ],
    );
    run.expect(
        "public",
        &[
            "dns-public",
            "https-public-image-ca",
            "mcp-host-alias",
            "api-auth-header-intact",
            "host-port-not-authorised",
            "guest-loopback-is-not-host",
        ],
    );
    run.expect("control", &["denied-port-positive-control"]);
    assert_eq!(run.exit("public"), Some(0), "{}", run.raw);
    assert!(mcp.hits() >= 2, "the authorised host service was reached");
    assert_eq!(
        other.hits(),
        1,
        "the denied port must see exactly its positive control, no denied guest connection"
    );
    assert_eq!(run.leftover(), Some(0), "{}", run.raw);
}

/// `allowlist` denies every destination except the listed name: other names
/// do not resolve, direct resolvers, DNS-over-HTTPS, direct IPv4/IPv6
/// addresses and proxies (a guest-configured one, and a host "proxy" on an
/// unauthorised port) are all refused, while the authorised host port stays
/// reachable.
#[test]
fn builtin_hw_denied_egress_blocks_dns_ip_and_proxy_bypass() {
    let test = "builtin_hw_denied_egress_blocks_dns_ip_and_proxy_bypass";
    let Some(archive) = gate(test, Need::Internet) else {
        return;
    };
    let mcp = HostService::start();
    let proxy = HostService::start();
    let (allowed, denied) = (allowed_name(), denied_name());
    let (p, q) = (mcp.port, proxy.port);
    let body = format!(
        r#"
ok dns-allowed nslookup {allowed}
ok https-allowed fetch https://{allowed}/
refused dns-denied nslookup {denied}
refused dns-direct-resolver nslookup {denied} 8.8.8.8
refused dns-direct-resolver-v6 nslookup {denied} 2001:4860:4860::8888
refused https-denied fetch https://{denied}/
refused doh-by-address fetch --header "accept: application/dns-json" "https://1.1.1.1/dns-query?name={denied}&type=A"
refused doh-by-name fetch "https://cloudflare-dns.com/dns-query?name={denied}&type=A"
refused direct-ipv4 fetch http://1.1.1.1/
refused direct-ipv6 fetch "http://[2606:4700:4700::1111]/"
refused host-proxy-unauthorised env http_proxy=http://host.microsandbox.internal:{q} https_proxy=http://host.microsandbox.internal:{q} wget -T 8 -O - http://{denied}/
has mcp-authorised host-ok fetch http://host.microsandbox.internal:{p}/
"#
    );
    // Every public negative target is exercised with the same executable and
    // CA bundle in a preceding permissive guest. Unreachable IPv6, missing CA
    // roots or a stopped resolver therefore fail the positive control instead
    // of masquerading as successful policy enforcement.
    let control = format!(
        r#"
ok dns-denied nslookup {denied}
ok dns-direct-resolver nslookup {denied} 8.8.8.8
ok dns-direct-resolver-v6 nslookup {denied} 2001:4860:4860::8888
ok https-denied fetch https://{denied}/
ok doh-by-address fetch --header "accept: application/dns-json" "https://1.1.1.1/dns-query?name={denied}&type=A"
ok doh-by-name fetch "https://cloudflare-dns.com/dns-query?name={denied}&type=A"
ok direct-ipv4 fetch http://1.1.1.1/
ok direct-ipv6 fetch "http://[2606:4700:4700::1111]/"
has host-proxy-unauthorised host-ok env http_proxy=http://host.microsandbox.internal:{q} https_proxy=http://host.microsandbox.internal:{q} wget -T 8 -O - http://{denied}/
"#
    );
    let network = json!({"mode": "allowlist", "allow": [allowed], "hostPorts": [p]});
    let plan = json!({"phases": [
        [vm("control", 1, 384, json!({"hostPorts": [q]}), &control)],
        [vm("allowlist", 2, 512, network, &body)]
    ]});
    let run = run_plan(test, &archive, plan, &[]);
    run.expect(
        "allowlist",
        &[
            "dns-allowed",
            "https-allowed",
            "dns-denied",
            "dns-direct-resolver",
            "dns-direct-resolver-v6",
            "https-denied",
            "doh-by-address",
            "doh-by-name",
            "direct-ipv4",
            "direct-ipv6",
            "host-proxy-unauthorised",
            "mcp-authorised",
        ],
    );
    run.expect(
        "control",
        &[
            "dns-denied",
            "dns-direct-resolver",
            "dns-direct-resolver-v6",
            "https-denied",
            "doh-by-address",
            "doh-by-name",
            "direct-ipv4",
            "direct-ipv6",
            "host-proxy-unauthorised",
        ],
    );
    assert_eq!(
        proxy.hits(),
        1,
        "the proxy must see exactly its authorised positive control, no denied guest connection"
    );
    assert!(mcp.hits() >= 1);
    assert_eq!(run.leftover(), Some(0), "{}", run.raw);
}

/// Three VMs with different policies run at the same time; each sees only
/// its own policy.
#[test]
fn builtin_hw_network_policy_isolated_between_vms() {
    let test = "builtin_hw_network_policy_isolated_between_vms";
    let Some(archive) = gate(test, Need::Internet) else {
        return;
    };
    let (allowed, denied) = (allowed_name(), denied_name());
    // A shared start delay makes the three policies overlap in time.
    let public = format!(
        "sleep 3\nok reach-denied-name fetch https://{denied}/\nok reach-allowed-name fetch https://{allowed}/"
    );
    let allowlist = format!(
        "sleep 3\nrefused reach-denied-name fetch https://{denied}/\nok reach-allowed-name fetch https://{allowed}/"
    );
    let none = format!(
        "sleep 3\nonly_lo only-loopback\nrefused no-dns nslookup {allowed}\nrefused no-egress fetch https://{allowed}/"
    );
    let plan = json!({"phases": [[
        vm("public", 1, 384, json!({}), &public),
        vm("allowlist", 1, 384, json!({"mode": "allowlist", "allow": [allowed]}), &allowlist),
        vm("none", 1, 384, json!({"mode": "none"}), &none),
    ]]});
    let run = run_plan(test, &archive, plan, &[]);
    run.expect("public", &["reach-denied-name", "reach-allowed-name"]);
    run.expect("allowlist", &["reach-denied-name", "reach-allowed-name"]);
    run.expect("none", &["only-loopback", "no-dns", "no-egress"]);
    assert_eq!(run.leftover(), Some(0), "{}", run.raw);
}

/// A cached image runs with no network device, an empty PATH on the host
/// side and no acquisition service: the import is a local archive and the
/// second import is served from the cache.
#[test]
fn builtin_hw_offline_cached_execution_under_denial() {
    let test = "builtin_hw_offline_cached_execution_under_denial";
    let Some(archive) = gate(test, Need::Hardware) else {
        return;
    };
    let body = "only_lo only-loopback\nrefused no-dns nslookup example.com\nrefused no-egress fetch http://1.1.1.1/\necho offline-ran";
    let plan = json!({"phases": [[vm("offline", 1, 256, json!({"mode": "none"}), body)]]});
    let run = run_plan(test, &archive, plan, &[]);
    run.expect("offline", &["only-loopback", "no-dns", "no-egress"]);
    assert!(run.stdout("offline").contains("offline-ran"));
    assert_eq!(run.exit("offline"), Some(0), "{}", run.raw);
    assert_eq!(run.facts["IMPORT"].len(), 2, "{}", run.raw);
    assert_eq!(run.leftover(), Some(0), "{}", run.raw);
}

/// A 256 MiB guest sees at most 256 MiB, an allocation past it is killed by
/// the guest's own OOM killer (inside the VM and as the agent itself), a
/// concurrent peer VM and the host are unaffected, a VM started afterwards
/// works, and nothing is left running.
#[test]
fn builtin_hw_memory_oom_preserves_host_and_peer() {
    let test = "builtin_hw_memory_oom_preserves_host_and_peer";
    let Some(archive) = gate(test, Need::Pressure) else {
        return;
    };
    // MemTotal is in KiB; the kernel keeps some memory for itself.
    let inner = r#"
has memtotal-bounded bounded sh -c 'kb=$(awk "/^MemTotal:/ {print \$2}" /proc/meminfo); if [ "$kb" -le 262144 ] && [ "$kb" -ge 131072 ]; then echo bounded; else echo "memtotal=$kb"; fi'
tail /dev/zero >/dev/null 2>&1
code=$?
if [ "$code" = 137 ]; then echo "PASS inner-allocation-oom-killed"; else echo "FAIL inner-allocation-oom-killed exit=$code"; fi
echo "PASS guest-survives-inner-oom"
"#;
    let agent = "exec tail /dev/zero";
    let peer = "sleep 15\necho PASS peer-unaffected";
    let after = "echo PASS runtime-still-starts-vms";
    let plan = json!({"phases": [
        [
            vm("inner", 1, 256, json!({"mode": "none"}), inner),
            vm("agent", 1, 256, json!({"mode": "none"}), agent),
            vm("peer", 1, 256, json!({"mode": "none"}), peer),
        ],
        [vm("after", 1, 256, json!({"mode": "none"}), after)],
    ]});
    let run = run_plan(test, &archive, plan, &[]);
    run.expect(
        "inner",
        &[
            "memtotal-bounded",
            "inner-allocation-oom-killed",
            "guest-survives-inner-oom",
        ],
    );
    assert_eq!(run.exit("inner"), Some(0), "{}", run.raw);
    assert_eq!(
        run.exit("agent"),
        Some(137),
        "an agent killed by the guest OOM killer reports SIGKILL: {}",
        run.raw
    );
    run.expect("peer", &["peer-unaffected"]);
    assert_eq!(run.exit("peer"), Some(0), "{}", run.raw);
    run.expect("after", &["runtime-still-starts-vms"]);
    assert_eq!(run.leftover(), Some(0), "{}", run.raw);
}

/// Integer vCPUs: a 2-vCPU and a 1-vCPU guest see exactly that many CPUs, and
/// under a busy loop per CPU every vCPU of the 2-vCPU guest accumulates
/// run time. This shows the allocation, not a share of host CPU time.
#[test]
fn builtin_hw_vcpu_load_and_cleanup() {
    let test = "builtin_hw_vcpu_load_and_cleanup";
    let Some(archive) = gate(test, Need::Hardware) else {
        return;
    };
    let load = |n: u8| {
        format!(
            r#"
is nproc-{n} {n} nproc
is cpuinfo-{n} {n} sh -c 'grep -c ^processor /proc/cpuinfo'
awk '/^cpu[0-9]/ {{print $1, $2+$3+$4}}' /proc/stat > /tmp/before
i=0; while [ $i -lt {n} ]; do timeout 3 sh -c 'while :; do :; done' & i=$((i+1)); done; wait
awk '/^cpu[0-9]/ {{print $1, $2+$3+$4}}' /proc/stat > /tmp/after
awk 'NR==FNR {{b[$1]=$2; next}} {{d=$2-b[$1]; if (d >= 50) print "PASS busy-" $1; else print "FAIL busy-" $1 " ticks=" d}}' /tmp/before /tmp/after
"#
        )
    };
    let plan = json!({"phases": [[
        vm("two", 2, 384, json!({"mode": "none"}), &load(2)),
        vm("one", 1, 384, json!({"mode": "none"}), &load(1)),
    ]]});
    let run = run_plan(test, &archive, plan, &[]);
    run.expect("two", &["nproc-2", "cpuinfo-2", "busy-cpu0", "busy-cpu1"]);
    run.expect("one", &["nproc-1", "cpuinfo-1", "busy-cpu0"]);
    assert_eq!(run.exit("two"), Some(0), "{}", run.raw);
    assert_eq!(run.exit("one"), Some(0), "{}", run.raw);
    assert_eq!(run.leftover(), Some(0), "{}", run.raw);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capable() -> Facts {
        Facts {
            gate_on: true,
            hypervisor: Ok(()),
            binary_has_builtin: true,
            driver_built: true,
            archive: Some("/tmp/x.tar".into()),
        }
    }

    #[test]
    fn network_and_pressure_tests_need_their_own_opt_in() {
        assert_eq!(
            decide_net(&capable(), Need::Hardware, false, false),
            Decision::Run
        );
        assert!(matches!(
            decide_net(&capable(), Need::Internet, false, true),
            Decision::Skip(r) if r.contains("AWMAN_TEST_BUILTIN_NETWORK")
        ));
        assert!(matches!(
            decide_net(&capable(), Need::Pressure, true, false),
            Decision::Skip(r) if r.contains("AWMAN_TEST_BUILTIN_PRESSURE")
        ));
        assert_eq!(
            decide_net(&capable(), Need::Internet, true, false),
            Decision::Run
        );
        assert_eq!(
            decide_net(&capable(), Need::Pressure, false, true),
            Decision::Run
        );
    }

    #[test]
    fn a_host_that_cannot_boot_is_blocked_whatever_the_opt_ins() {
        let mut facts = capable();
        facts.hypervisor = Err("no kvm".into());
        assert!(matches!(
            decide_net(&facts, Need::Internet, true, true),
            Decision::Blocked(r) if r == "no kvm"
        ));
        let mut facts = capable();
        facts.driver_built = false;
        assert!(matches!(
            decide_net(&facts, Need::Pressure, true, true),
            Decision::Blocked(_)
        ));
    }

    #[test]
    fn a_missing_or_failed_guest_check_is_a_problem() {
        let checks = parse_checks("PASS a\nFAIL b detail\nnoise\nPASS  c \n");
        assert_eq!(
            check_problems(&checks, &["a", "c", "d"]),
            vec!["FAIL b detail".to_string(), "missing d".to_string()]
        );
        assert!(check_problems(&parse_checks("PASS a\n"), &["a"]).is_empty());
    }

    #[test]
    fn guest_scripts_bound_every_probe_and_keep_the_prelude() {
        let s = script("ok x true");
        assert!(s.contains("timeout 20"));
        assert!(s.contains("timeout 20 wget -T 8"));
        assert!(s.trim_end().ends_with("ok x true"));
    }

    #[test]
    fn the_host_service_checks_the_bearer_token() {
        let service = HostService::start();
        let get = |path: &str, header: &str| {
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", service.port)).unwrap();
            write!(stream, "GET {path} HTTP/1.0\r\n{header}\r\n").unwrap();
            let mut out = String::new();
            stream.read_to_string(&mut out).unwrap();
            out
        };
        assert!(get(
            "/auth",
            &format!("Authorization: Bearer {SYNTHETIC_TOKEN}\r\n")
        )
        .ends_with("auth-ok"));
        assert!(get("/auth", "").ends_with("auth-missing"));
        assert!(get("/", "").ends_with("host-ok"));
        assert_eq!(service.hits(), 3);
    }
}
