//! Typed capabilities (WI 3.4).
//!
//! A test states what it needs with `sandbox.require::<Docker>()`. The tier
//! (from the gate manifest) says what it guarantees or allows:
//!
//! | Situation | Outcome |
//! |---|---|
//! | available to the tier and present | `Granted<C>` |
//! | available to the tier but absent | panic `BLOCKED[<id>]: <reason>` (infrastructure, never a skip) |
//! | not available to the tier | panic `MISPLACED[<id>]: …` (product failure) |
//!
//! Real host resources (Docker, registries, Apple Containers, sbx, a
//! hypervisor, public network, credentials) are used only when named in
//! `AWMAN_TEST_HOST_RESOURCES`; a bare `cargo test` names none, so such a
//! test blocks before touching anything real.

use crate::env::EnvMap;
use crate::runtime_env::RuntimeEnv;
use crate::tier::Tier;
use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// What a probe may look at. It never contains the parent environment.
pub struct ProbeContext {
    runtime: RuntimeEnv,
    sandbox_root: PathBuf,
    exes: BTreeMap<String, PathBuf>,
}

impl ProbeContext {
    pub(crate) fn new(sandbox_root: PathBuf, exes: BTreeMap<String, PathBuf>) -> ProbeContext {
        ProbeContext {
            runtime: crate::runtime_env::get().clone(),
            sandbox_root,
            exes,
        }
    }

    /// A context with an explicit runtime snapshot (tests of this crate).
    #[cfg(test)]
    pub(crate) fn with_runtime(runtime: RuntimeEnv, sandbox_root: PathBuf) -> ProbeContext {
        ProbeContext {
            runtime,
            sandbox_root,
            exes: BTreeMap::new(),
        }
    }

    /// The enumerated runtime inputs.
    pub fn runtime(&self) -> &RuntimeEnv {
        &self.runtime
    }

    /// The sandbox root.
    pub fn sandbox_root(&self) -> &Path {
        &self.sandbox_root
    }

    /// An executable registered with [`crate::TestSandbox::register_exe`].
    pub fn exe(&self, name: &str) -> Option<&Path> {
        self.exes.get(name).map(PathBuf::as_path)
    }

    /// The value of a named host resource, or `Err` saying it was not granted.
    pub fn host_resource(&self, name: &str) -> Result<&str, String> {
        self.runtime.host_resource(name).ok_or_else(|| {
            format!("`{name}` is not named in AWMAN_TEST_HOST_RESOURCES (a bare test run uses no real host resources)")
        })
    }
}

/// A capability a test can require. `ID` equals a `[capabilities]` key of
/// `ci/gates.toml`.
pub trait Capability: Sized + 'static {
    /// Manifest ID.
    const ID: &'static str;
    /// Check presence; `Err(reason)` = absent.
    fn probe(ctx: &ProbeContext) -> Result<Self, String>;
    /// Variables a child needs to use the capability (see [`crate::TestSandbox::grant`]).
    fn grant_env(&self, _env: &mut EnvMap) {}
    /// Executables outside the system directories the capability brings.
    fn tools(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

/// A probed, present capability.
#[derive(Debug, Clone)]
pub struct Granted<C: Capability> {
    value: C,
}

impl<C: Capability> Deref for Granted<C> {
    type Target = C;
    fn deref(&self) -> &C {
        &self.value
    }
}

pub(crate) type Reprobe = fn(&ProbeContext) -> Result<(), String>;

pub(crate) fn reprobe<C: Capability>(ctx: &ProbeContext) -> Result<(), String> {
    C::probe(ctx).map(|_| ())
}

/// Capability IDs implemented by this crate (for `gates check`).
pub const IMPLEMENTED: &[&str] = &[
    TempDir::ID,
    Loopback::ID,
    Subprocess::ID,
    AwmanBinary::ID,
    FakeCliExe::ID,
    LocalGit::ID,
    Docker::ID,
    LocalRegistry::ID,
    GitTransports::ID,
    AppleContainers::ID,
    DockerSbx::ID,
    Hypervisor::ID,
    BuiltinPayload::ID,
    GuestFixtures::ID,
    PreviousRelease::ID,
    ReleaseArtifact::ID,
    Offline::ID,
    PublicNetwork::ID,
    RegistryCredentials::ID,
];

pub(crate) fn require<C: Capability>(tier: Tier, ctx: &ProbeContext) -> Granted<C> {
    let config = crate::config::run_config();
    if !config.capabilities.iter().any(|c| c == C::ID) {
        panic!(
            "UNKNOWN[{}]: capability is not declared in [capabilities] of the gate manifest",
            C::ID
        );
    }
    let tier_config = config.tier(tier);
    let available = tier_config.guarantees.iter().any(|c| c == C::ID)
        || tier_config.allowed_capabilities.iter().any(|c| c == C::ID);
    if !available {
        panic!(
            "MISPLACED[{}]: this test requires {}, which tier {tier} does not guarantee",
            C::ID,
            C::ID
        );
    }
    match C::probe(ctx) {
        Ok(value) => Granted { value },
        Err(reason) => panic!("BLOCKED[{}]: {reason}", C::ID),
    }
}

/// Sandbox temp dir (always present once the sandbox exists).
#[derive(Debug, Clone)]
pub struct TempDir;
impl Capability for TempDir {
    const ID: &'static str = "tempdir";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        if ctx.sandbox_root().is_dir() {
            Ok(TempDir)
        } else {
            Err("sandbox root is missing".into())
        }
    }
}

/// Loopback TCP (and Unix sockets on Unix).
#[derive(Debug, Clone)]
pub struct Loopback;
impl Capability for Loopback {
    const ID: &'static str = "loopback";
    fn probe(_ctx: &ProbeContext) -> Result<Self, String> {
        #[allow(clippy::disallowed_methods)] // probe only, closed immediately
        std::net::TcpListener::bind("127.0.0.1:0")
            .map(|_| Loopback)
            .map_err(|e| format!("cannot bind 127.0.0.1:0: {e}"))
    }
}

/// Owned child processes.
#[derive(Debug, Clone)]
pub struct Subprocess;
impl Capability for Subprocess {
    const ID: &'static str = "subprocess";
    fn probe(_ctx: &ProbeContext) -> Result<Self, String> {
        Ok(Subprocess)
    }
}

/// The debug `awman` executable built with the tests (registered as `awman`).
#[derive(Debug, Clone)]
pub struct AwmanBinary {
    /// Its path.
    pub exe: PathBuf,
}
impl Capability for AwmanBinary {
    const ID: &'static str = "awman-binary";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        registered(ctx, "awman").map(|exe| AwmanBinary { exe })
    }
}

/// The fake CLI helper executable (registered as `fake-cli`).
#[derive(Debug, Clone)]
pub struct FakeCliExe {
    /// Its path.
    pub exe: PathBuf,
}
impl Capability for FakeCliExe {
    const ID: &'static str = "fake-cli";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        registered(ctx, "fake-cli").map(|exe| FakeCliExe { exe })
    }
}

fn registered(ctx: &ProbeContext, name: &str) -> Result<PathBuf, String> {
    let exe = ctx
        .exe(name)
        .ok_or_else(|| format!("`{name}` was not registered with TestSandbox::register_exe"))?;
    if exe.is_file() {
        Ok(exe.to_path_buf())
    } else {
        Err(format!("{} does not exist", exe.display()))
    }
}

/// The git CLI (used only on temporary repositories).
#[derive(Debug, Clone)]
pub struct LocalGit {
    /// Its path.
    pub exe: PathBuf,
}
impl Capability for LocalGit {
    const ID: &'static str = "local-git";
    fn probe(_ctx: &ProbeContext) -> Result<Self, String> {
        find_git()
            .map(|exe| LocalGit { exe })
            .ok_or_else(|| "git was not found in the fixed system locations".into())
    }
}

/// Locate git in fixed locations (never through the inherited `PATH`).
pub(crate) fn find_git() -> Option<PathBuf> {
    static GIT: OnceLock<Option<PathBuf>> = OnceLock::new();
    GIT.get_or_init(|| {
        let candidates: &[&str] = if cfg!(windows) {
            &[
                "C:\\Program Files\\Git\\cmd\\git.exe",
                "C:\\Program Files\\Git\\bin\\git.exe",
            ]
        } else {
            &[
                "/usr/bin/git",
                "/bin/git",
                "/usr/local/bin/git",
                "/opt/homebrew/bin/git",
            ]
        };
        candidates.iter().map(PathBuf::from).find(|p| p.is_file())
    })
    .clone()
}

fn find_tool(name: &str, extra: &[&str]) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let mut dirs: Vec<PathBuf> = crate::sandbox::system_tool_dirs().to_vec();
    dirs.extend(extra.iter().map(PathBuf::from));
    dirs.into_iter()
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
}

/// Put the CLI's directory on `PATH` right after the sandbox's `bin`.
fn cli_dir_env(env: &mut EnvMap, cli: &Path) {
    let Some(dir) = cli.parent() else { return };
    let mut dirs = env.path_dirs();
    if dirs.iter().any(|d| d == dir) {
        return;
    }
    let at = dirs.len().min(1);
    dirs.insert(at, dir.to_path_buf());
    env.set("PATH", std::env::join_paths(dirs).expect("PATH entries"));
}

/// A real Docker Engine, named in `AWMAN_TEST_HOST_RESOURCES` as
/// `docker=<DOCKER_HOST>` (a dedicated test daemon, never a default).
#[derive(Debug, Clone)]
pub struct Docker {
    /// `DOCKER_HOST` value.
    pub host: String,
    /// The docker CLI.
    pub cli: PathBuf,
}
impl Capability for Docker {
    const ID: &'static str = "docker";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        let host = ctx.host_resource("docker")?;
        if host.is_empty() {
            return Err("docker must name its engine: docker=<DOCKER_HOST>".into());
        }
        if let Some(path) = host.strip_prefix("unix://") {
            if !Path::new(path).exists() {
                return Err(format!("{path} does not exist"));
            }
        }
        let cli = find_tool(
            "docker",
            &["/Applications/Docker.app/Contents/Resources/bin"],
        )
        .ok_or("the docker CLI was not found")?;
        Ok(Docker {
            host: host.to_string(),
            cli,
        })
    }
    fn grant_env(&self, env: &mut EnvMap) {
        env.set("DOCKER_HOST", &self.host)
            .set("AWMAN_TEST_DOCKER", "1");
        cli_dir_env(env, &self.cli);
    }
    fn tools(&self) -> Vec<PathBuf> {
        vec![self.cli.clone()]
    }
}

/// A registry on loopback, named as `local-registry=<host:port>` (started by
/// the job or by boundary glue on the granted Docker Engine).
#[derive(Debug, Clone)]
pub struct LocalRegistry {
    /// Its address.
    pub addr: SocketAddr,
}
impl Capability for LocalRegistry {
    const ID: &'static str = "local-registry";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        let value = ctx.host_resource("local-registry")?;
        let addr: SocketAddr = value
            .parse()
            .map_err(|_| format!("local-registry must be <ip:port>, got {value:?}"))?;
        if !addr.ip().is_loopback() {
            return Err(format!("{addr} is not a loopback address"));
        }
        TcpStream::connect_timeout(&addr, Duration::from_secs(2))
            .map_err(|e| format!("{addr}: {e}"))?;
        Ok(LocalRegistry { addr })
    }
}

/// Git over ssh/http to servers on loopback (`sshd` and `git http-backend`).
#[derive(Debug, Clone)]
pub struct GitTransports {
    /// `sshd`.
    pub sshd: PathBuf,
    /// git (its `http-backend` subcommand serves smart HTTP).
    pub git: PathBuf,
}
impl Capability for GitTransports {
    const ID: &'static str = "git-transports";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        ctx.host_resource("git-transports")?;
        let sshd = find_tool("sshd", &[]).ok_or("sshd was not found")?;
        let git = find_git().ok_or("git was not found")?;
        Ok(GitTransports { sshd, git })
    }
}

/// Apple's container CLI with its service running.
#[derive(Debug, Clone)]
pub struct AppleContainers {
    /// The `container` CLI.
    pub cli: PathBuf,
}
impl Capability for AppleContainers {
    const ID: &'static str = "apple-containers";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        ctx.host_resource("apple-containers")?;
        if !cfg!(target_os = "macos") {
            return Err("Apple Containers exist only on macOS".into());
        }
        let cli = find_tool("container", &[]).ok_or("the container CLI was not found")?;
        Ok(AppleContainers { cli })
    }
    fn grant_env(&self, env: &mut EnvMap) {
        env.set("AWMAN_TEST_APPLE_CONTAINER", "1");
        cli_dir_env(env, &self.cli);
    }
    fn tools(&self) -> Vec<PathBuf> {
        vec![self.cli.clone()]
    }
}

/// The Docker Sandboxes `sbx` CLI.
#[derive(Debug, Clone)]
pub struct DockerSbx {
    /// The `sbx` CLI.
    pub cli: PathBuf,
}
impl Capability for DockerSbx {
    const ID: &'static str = "docker-sbx";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        ctx.host_resource("docker-sbx")?;
        let cli = find_tool("sbx", &[]).ok_or("the sbx CLI was not found")?;
        Ok(DockerSbx { cli })
    }
    fn grant_env(&self, env: &mut EnvMap) {
        env.set("AWMAN_TEST_SBX", "1");
        cli_dir_env(env, &self.cli);
    }
    fn tools(&self) -> Vec<PathBuf> {
        vec![self.cli.clone()]
    }
}

/// Hypervisor.framework (`kern.hv_support=1`) or read/write `/dev/kvm`.
#[derive(Debug, Clone)]
pub struct Hypervisor;
impl Capability for Hypervisor {
    const ID: &'static str = "hypervisor";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        ctx.host_resource("hypervisor")?;
        if cfg!(target_os = "linux") {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .map(|_| Hypervisor)
                .map_err(|e| format!("/dev/kvm: {e}"))
        } else if cfg!(target_os = "macos") {
            // A probe, not a test process: sysctl is a fixed system tool.
            #[allow(clippy::disallowed_methods)]
            let out = std::process::Command::new("/usr/sbin/sysctl")
                .args(["-n", "kern.hv_support"])
                .env_clear()
                .output()
                .map_err(|e| format!("sysctl: {e}"))?;
            if String::from_utf8_lossy(&out.stdout).trim() == "1" {
                Ok(Hypervisor)
            } else {
                Err("kern.hv_support is not 1".into())
            }
        } else {
            Err("no supported hypervisor on this platform".into())
        }
    }
    fn grant_env(&self, env: &mut EnvMap) {
        env.set("AWMAN_TEST_BUILTIN", "1");
    }
}

/// A verified msb payload directory, named as `builtin-payload=<dir>`.
#[derive(Debug, Clone)]
pub struct BuiltinPayload {
    /// Payload directory.
    pub dir: PathBuf,
}
impl Capability for BuiltinPayload {
    const ID: &'static str = "builtin-payload";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        let dir = PathBuf::from(ctx.host_resource("builtin-payload")?);
        if dir.is_dir() {
            Ok(BuiltinPayload { dir })
        } else {
            Err(format!(
                "payload directory {} does not exist",
                dir.display()
            ))
        }
    }
}

/// Every pinned guest fixture of the manifest, digest-verified.
#[derive(Debug, Clone)]
pub struct GuestFixtures {
    /// Fixture ID → verified path.
    pub paths: BTreeMap<String, PathBuf>,
}
impl Capability for GuestFixtures {
    const ID: &'static str = "guest-fixtures";
    fn probe(_ctx: &ProbeContext) -> Result<Self, String> {
        let config = crate::config::run_config();
        // Archive fixtures for the guest tier; verified payload records
        // (`verified_record`, no `file`) are the builtin-payload capability.
        let ids: Vec<&String> = config
            .fixtures
            .iter()
            .filter(|(_, f)| f.tiers.iter().any(|t| t == "guest") && f.file.is_some())
            .map(|(id, _)| id)
            .collect();
        if ids.is_empty() {
            return Err("the manifest pins no guest fixture archives".into());
        }
        let mut paths = BTreeMap::new();
        for id in ids {
            let path = crate::fixtures::external_checked(id)?;
            paths.insert(id.clone(), path);
        }
        Ok(GuestFixtures { paths })
    }
}

/// The pinned previous published awman for the host:
/// `[previous_release.<target>]` (`version`, `asset`, `sha256`), stored as
/// `<fixture store>/previous-release/<target>/<asset>`.
#[derive(Debug, Clone)]
pub struct PreviousRelease {
    /// The executable.
    pub exe: PathBuf,
    /// Its version.
    pub version: String,
}
impl Capability for PreviousRelease {
    const ID: &'static str = "previous-release";
    fn probe(_ctx: &ProbeContext) -> Result<Self, String> {
        let target = crate::evidence::target();
        let spec = crate::config::run_config()
            .previous_release
            .get(target)
            .cloned()
            .ok_or_else(|| format!("no [previous_release.{target}] in the gate manifest"))?;
        let dir = crate::fixtures::store()
            .join("previous-release")
            .join(target);
        let exe = crate::fixtures::pinned_file(&format!("previous_release.{target}"), &spec, &dir)?;
        let version = spec.version.clone().unwrap_or_default();
        Ok(PreviousRelease { exe, version })
    }
    fn tools(&self) -> Vec<PathBuf> {
        vec![self.exe.clone()]
    }
}
/// The release executable under test (`AWMAN_TEST_ARTIFACT`), verified
/// against `AWMAN_TEST_ARTIFACT_SHA256` before use.
#[derive(Debug, Clone)]
pub struct ReleaseArtifact {
    /// The executable.
    pub exe: PathBuf,
    /// Its verified SHA-256.
    pub sha256: String,
}
impl Capability for ReleaseArtifact {
    const ID: &'static str = "release-artifact";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        let exe = ctx
            .runtime()
            .artifact
            .clone()
            .ok_or("AWMAN_TEST_ARTIFACT is not set")?;
        let expected = ctx
            .runtime()
            .artifact_sha256
            .clone()
            .ok_or("AWMAN_TEST_ARTIFACT_SHA256 is not set")?
            .to_lowercase();
        let actual =
            crate::fixtures::sha256_file(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
        if actual != expected {
            return Err(format!(
                "{}: sha256 {actual}, expected {expected}",
                exe.display()
            ));
        }
        Ok(ReleaseArtifact {
            exe,
            sha256: actual,
        })
    }
    fn tools(&self) -> Vec<PathBuf> {
        vec![self.exe.clone()]
    }
}

/// No route beyond loopback (enforced by the job's session wrapper).
#[derive(Debug, Clone)]
pub struct Offline;
impl Capability for Offline {
    const ID: &'static str = "offline";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        ctx.host_resource("offline").map(|_| Offline)
    }
}

/// Outbound access to public registries.
#[derive(Debug, Clone)]
pub struct PublicNetwork;
impl Capability for PublicNetwork {
    const ID: &'static str = "public-network";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        ctx.host_resource("public-network").map(|_| PublicNetwork)
    }
}

/// Credentials for the private-registry suite, from a private file named as
/// `registry-credentials=<file>` (lines `registry=…`, `username=…`,
/// `password=…`). Never printed.
#[derive(Clone)]
pub struct RegistryCredentials {
    /// Registry host.
    pub registry: String,
    /// User name.
    pub username: String,
    password: String,
}
impl RegistryCredentials {
    /// The password (never put it in evidence; call `sandbox.redact` first).
    pub fn password(&self) -> &str {
        &self.password
    }
}
impl std::fmt::Debug for RegistryCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RegistryCredentials {{ registry: {:?}, username: <redacted>, password: <redacted> }}",
            self.registry
        )
    }
}
impl Capability for RegistryCredentials {
    const ID: &'static str = "registry-credentials";
    fn probe(ctx: &ProbeContext) -> Result<Self, String> {
        let file = ctx.host_resource("registry-credentials")?;
        let text = std::fs::read_to_string(file).map_err(|e| format!("credentials file: {e}"))?;
        let mut fields = BTreeMap::new();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                fields.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        let get = |k: &str| {
            fields
                .get(k)
                .cloned()
                .ok_or(format!("credentials file lacks `{k}`"))
        };
        Ok(RegistryCredentials {
            registry: get("registry")?,
            username: get("username")?,
            password: get("password")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ProbeContext {
        ProbeContext::new(
            crate::runtime_env::get().system_temp.clone(),
            BTreeMap::new(),
        )
    }

    fn panic_text(f: impl FnOnce()) -> String {
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_err();
        crate::panic_text(&err)
    }

    #[test]
    fn implemented_ids_equal_the_manifest_ids() {
        let mut implemented: Vec<&str> = IMPLEMENTED.to_vec();
        implemented.sort();
        let mut declared: Vec<String> = crate::config::run_config().capabilities.clone();
        declared.sort();
        assert_eq!(
            implemented,
            declared.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn guaranteed_and_present_is_granted() {
        let granted = require::<TempDir>(Tier::Unit, &ctx());
        let _: &TempDir = &granted;
    }

    #[test]
    fn not_available_to_the_tier_is_misplaced() {
        let text = panic_text(|| {
            require::<Docker>(Tier::Component, &ctx());
        });
        assert!(text.starts_with("MISPLACED[docker]: "), "{text}");
        let text = panic_text(|| {
            require::<Loopback>(Tier::Unit, &ctx());
        });
        assert!(text.starts_with("MISPLACED[loopback]: "), "{text}");
    }

    #[test]
    fn guaranteed_but_absent_is_blocked_not_skipped() {
        // A bare run names no host resources: Docker blocks in boundary.
        let bare = ProbeContext::with_runtime(
            RuntimeEnv::from_vars(&BTreeMap::new(), PathBuf::from("/tmp")),
            crate::runtime_env::get().system_temp.clone(),
        );
        let text = panic_text(|| {
            require::<Docker>(Tier::Boundary, &bare);
        });
        assert!(text.starts_with("BLOCKED[docker]: "), "{text}");
        assert!(text.contains("AWMAN_TEST_HOST_RESOURCES"), "{text}");
        // Named, but the engine socket is missing: still blocked.
        let mut vars = BTreeMap::new();
        vars.insert(
            "AWMAN_TEST_HOST_RESOURCES".to_string(),
            std::ffi::OsString::from("docker=unix:///nonexistent/docker.sock"),
        );
        let named = ProbeContext::with_runtime(
            RuntimeEnv::from_vars(&vars, PathBuf::from("/tmp")),
            crate::runtime_env::get().system_temp.clone(),
        );
        let text = panic_text(|| {
            require::<Docker>(Tier::Boundary, &named);
        });
        assert!(
            text.contains("/nonexistent/docker.sock does not exist"),
            "{text}"
        );
        // An unregistered awman binary blocks in component.
        let text = panic_text(|| {
            require::<AwmanBinary>(Tier::Component, &ctx());
        });
        assert!(text.starts_with("BLOCKED[awman-binary]: "), "{text}");
    }

    #[test]
    fn credentials_never_print() {
        let creds = RegistryCredentials {
            registry: "r".into(),
            username: "u".into(),
            password: "test-secret-pw".into(),
        };
        assert!(!format!("{creds:?}").contains("test-secret-pw"));
        assert_eq!(creds.password(), "test-secret-pw");
    }

    #[test]
    fn docker_grant_sets_the_engine_and_opt_in() {
        let docker = Docker {
            host: "unix:///x.sock".into(),
            cli: PathBuf::from("/opt/docker/bin/docker"),
        };
        let mut env = EnvMap::new();
        env.set(
            "PATH",
            std::env::join_paths(["/sandbox/bin", "/usr/bin"]).unwrap(),
        );
        docker.grant_env(&mut env);
        assert_eq!(env.path_dirs()[0], PathBuf::from("/sandbox/bin"));
        assert_eq!(env.get_str("DOCKER_HOST"), Some("unix:///x.sock"));
        assert_eq!(env.get_str("AWMAN_TEST_DOCKER"), Some("1"));
        assert!(env.path_dirs().contains(&PathBuf::from("/opt/docker/bin")));
    }
}
