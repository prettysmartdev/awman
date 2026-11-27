//! Shared Docker Sandboxes (`sbx`) contract between the kit sources and the
//! runtime adapter (work item 0086).
//!
//! Install verbatim as `src/engine/agent/sbx_contract.rs` and declare it with
//! `pub mod sbx_contract;` in `src/engine/agent/mod.rs`. Depends only on
//! `std`, `serde`, `serde_json` and `sha2`, so neither side needs the other's
//! code to compile against it.
//!
//! Three things live here because both sides must agree on them exactly:
//! the per-agent support matrix, the on-disk kit manifest and digest, and the
//! guest paths and wire format of the per-launch policy apply.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ─── CLI admission ──────────────────────────────────────────────────────────

/// A parsed `sbx` client version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SbxCliVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl SbxCliVersion {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// The first `MAJOR.MINOR.PATCH` token in `text` (an optional leading `v`
    /// is accepted; a pre-release or build suffix is ignored).
    pub fn parse(text: &str) -> Option<Self> {
        text.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')')
            .find_map(|token| {
                let token = token.strip_prefix('v').unwrap_or(token);
                let core = token.split(['-', '+']).next().unwrap_or(token);
                let mut parts = core.split('.');
                let major = parts.next()?.parse().ok()?;
                let minor = parts.next()?.parse().ok()?;
                let patch = parts.next()?.parse().ok()?;
                if parts.next().is_some() {
                    return None;
                }
                Some(Self::new(major, minor, patch))
            })
    }
}

impl fmt::Display for SbxCliVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Oldest admitted client. v0.53.0 changed `sbx run ... -- AGENT_ARGS` to
/// keep the workload's entrypoint arguments; every launch tail awman builds
/// assumes that behaviour.
pub const SBX_MIN_CLI: SbxCliVersion = SbxCliVersion::new(0, 53, 0);

/// Exact client versions known not to work. Empty until live qualification
/// finds one.
pub const SBX_KNOWN_BAD_CLI: &[SbxCliVersion] = &[];

/// Outcome of checking the installed client. Only `Admitted` may launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SbxCliAdmission {
    Admitted(SbxCliVersion),
    TooOld {
        found: SbxCliVersion,
        minimum: SbxCliVersion,
    },
    KnownBad {
        found: SbxCliVersion,
    },
    /// No version could be read; `raw` is the first output line, for the error.
    Unparseable {
        raw: String,
    },
}

/// Classify the stdout of `sbx --version`.
pub fn admit_cli(version_stdout: &str) -> SbxCliAdmission {
    let Some(found) = SbxCliVersion::parse(version_stdout) else {
        return SbxCliAdmission::Unparseable {
            raw: version_stdout
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string(),
        };
    };
    if found < SBX_MIN_CLI {
        return SbxCliAdmission::TooOld {
            found,
            minimum: SBX_MIN_CLI,
        };
    }
    if SBX_KNOWN_BAD_CLI.contains(&found) {
        return SbxCliAdmission::KnownBad { found };
    }
    SbxCliAdmission::Admitted(found)
}

// ─── Support matrix ─────────────────────────────────────────────────────────

/// How awman runs one first-party agent on Docker Sandboxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbxDisposition {
    /// Docker's published v3 workload, selected by its built-in name, plus an
    /// awman v3 mixin added with `--kit`.
    PublishedWorkloadWithMixin { workload: &'static str },
    /// An awman-owned v3 workload; the kit directory is the positional selector.
    OwnedWorkload,
    /// No SBX route. `reason` completes "agent 'x' ... on Docker Sandboxes: ".
    Refused { reason: &'static str },
}

/// How one requested option reaches the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbxDelivery {
    /// Appended after `--` on `sbx run`.
    Argv,
    /// Rendered on the host into an awman-managed guest file by the apply step.
    ManagedFile,
    /// No route: the launch is refused before any `sbx` call.
    Refused,
    /// No route, and the option is context rather than policy: the launch
    /// continues with a warning. Never used for mode, tools, model or flags.
    NotedOnly,
}

/// A guest file the apply step owns completely. Every launch rewrites or
/// removes every managed file of its agent, so no earlier launch's policy
/// survives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SbxManagedFile {
    /// Stable identifier used on the apply wire.
    pub id: &'static str,
    /// Absolute guest path.
    pub path: &'static str,
    /// `true`: owned by root, mode 0644, so the agent user cannot rewrite it.
    /// `false`: owned by uid 1000, mode 0644.
    pub root_owned: bool,
}

/// One row of the support matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SbxRoute {
    pub agent: &'static str,
    pub disposition: SbxDisposition,
    /// Plan, auto and yolo.
    pub mode: SbxDelivery,
    /// Allowed and disallowed tool lists.
    pub tools: SbxDelivery,
    pub model: SbxDelivery,
    /// User `agentFlags`.
    pub agent_flags: SbxDelivery,
    pub system_prompt: SbxDelivery,
    /// The `sbx` credential service whose secret authenticates this route.
    pub credential_service: Option<&'static str>,
    pub managed_files: &'static [SbxManagedFile],
    /// Non-secret environment the agent session always receives.
    pub session_env: &'static [(&'static str, &'static str)],
}

impl SbxRoute {
    pub fn is_supported(&self) -> bool {
        !matches!(self.disposition, SbxDisposition::Refused { .. })
    }

    pub fn is_owned_workload(&self) -> bool {
        matches!(self.disposition, SbxDisposition::OwnedWorkload)
    }

    pub fn managed_file(&self, id: &str) -> Option<&'static SbxManagedFile> {
        self.managed_files.iter().find(|file| file.id == id)
    }

    /// Kit handle: the kit directory name, the descriptor stem, and (for an
    /// owned workload) the agent name `sbx` reports. `None` when refused.
    pub fn kit_handle(&self) -> Option<String> {
        match self.disposition {
            SbxDisposition::PublishedWorkloadWithMixin { .. } => {
                Some(format!("awman-{}-mixin", self.agent))
            }
            SbxDisposition::OwnedWorkload => Some(format!("awman-{}", self.agent)),
            SbxDisposition::Refused { .. } => None,
        }
    }
}

/// Every first-party agent, in matrix order.
pub const SBX_AGENTS: [&str; 8] = [
    "claude",
    "codex",
    "copilot",
    "opencode",
    "cline",
    "antigravity",
    "cursor",
    "opencode-v2",
];

pub const CLAUDE_MANAGED_SETTINGS: &str = "claude-managed-settings";
pub const CLAUDE_MEMORY: &str = "claude-memory";
pub const CODEX_AGENTS_MD: &str = "codex-agents-md";
pub const COPILOT_INSTRUCTIONS: &str = "copilot-instructions";
pub const OPENCODE_AGENTS_MD: &str = "opencode-agents-md";

const CLAUDE_FILES: &[SbxManagedFile] = &[
    SbxManagedFile {
        id: CLAUDE_MANAGED_SETTINGS,
        path: "/etc/claude-code/managed-settings.json",
        root_owned: true,
    },
    SbxManagedFile {
        id: CLAUDE_MEMORY,
        path: "/home/agent/.claude/CLAUDE.md",
        root_owned: false,
    },
];
const CODEX_FILES: &[SbxManagedFile] = &[SbxManagedFile {
    id: CODEX_AGENTS_MD,
    path: "/home/agent/.codex/AGENTS.md",
    root_owned: false,
}];
const COPILOT_FILES: &[SbxManagedFile] = &[SbxManagedFile {
    id: COPILOT_INSTRUCTIONS,
    path: "/home/agent/.awman/copilot-instructions/awman-context.md",
    root_owned: false,
}];
const OPENCODE_FILES: &[SbxManagedFile] = &[SbxManagedFile {
    id: OPENCODE_AGENTS_MD,
    path: "/home/agent/.config/opencode/AGENTS.md",
    root_owned: false,
}];

const PRIVATE_SESSION_REASON: &str =
    "requires per-execution state and credential isolation, which Docker Sandboxes does not provide";

/// The support matrix. `None` for any name that is not a first-party agent
/// (custom agents have no SBX route).
pub fn sbx_route(agent: &str) -> Option<SbxRoute> {
    use SbxDelivery::{Argv, ManagedFile, NotedOnly, Refused};
    let published = |agent: &'static str,
                     workload: &'static str,
                     mode: SbxDelivery,
                     tools: SbxDelivery,
                     model: SbxDelivery,
                     service: &'static str,
                     files: &'static [SbxManagedFile],
                     env: &'static [(&'static str, &'static str)]| SbxRoute {
        agent,
        disposition: SbxDisposition::PublishedWorkloadWithMixin { workload },
        mode,
        tools,
        model,
        agent_flags: Refused,
        system_prompt: ManagedFile,
        credential_service: Some(service),
        managed_files: files,
        session_env: env,
    };
    let owned = |agent: &'static str,
                 system_prompt: SbxDelivery,
                 service: &'static str,
                 files: &'static [SbxManagedFile]| SbxRoute {
        agent,
        disposition: SbxDisposition::OwnedWorkload,
        mode: Argv,
        tools: Refused,
        model: Argv,
        agent_flags: Argv,
        system_prompt,
        credential_service: Some(service),
        managed_files: files,
        session_env: &[],
    };
    let refused = |agent: &'static str| SbxRoute {
        agent,
        disposition: SbxDisposition::Refused {
            reason: PRIVATE_SESSION_REASON,
        },
        mode: Refused,
        tools: Refused,
        model: Refused,
        agent_flags: Refused,
        system_prompt: Refused,
        credential_service: None,
        managed_files: &[],
        session_env: &[],
    };
    Some(match agent {
        "claude" => published(
            "claude",
            "claude",
            ManagedFile,
            ManagedFile,
            ManagedFile,
            "anthropic",
            CLAUDE_FILES,
            &[],
        ),
        "codex" => published(
            "codex",
            "codex",
            Refused,
            Refused,
            Argv,
            "openai",
            CODEX_FILES,
            &[],
        ),
        "copilot" => published(
            "copilot",
            "copilot",
            Refused,
            Refused,
            Refused,
            "github",
            COPILOT_FILES,
            &[(
                "COPILOT_CUSTOM_INSTRUCTIONS_DIRS",
                "/home/agent/.awman/copilot-instructions",
            )],
        ),
        "opencode" => owned("opencode", ManagedFile, "anthropic", OPENCODE_FILES),
        "cline" => owned("cline", Argv, "anthropic", &[]),
        "antigravity" => owned("antigravity", NotedOnly, "google", &[]),
        "cursor" => refused("cursor"),
        "opencode-v2" => refused("opencode-v2"),
        _ => return None,
    })
}

// ─── Guest layout and apply wire format ─────────────────────────────────────

/// Version of the apply wire format and of the guest scripts that speak it.
pub const APPLY_PROTOCOL: u32 = 2;
/// Directory inside every awman kit image holding awman's guest files.
pub const GUEST_LIB_DIR: &str = "/usr/local/lib/awman";
/// The apply script. Run as root through `sbx exec`; reads the wire on stdin.
pub const GUEST_APPLY: &str = "/usr/local/lib/awman/awman-apply";
/// Entrypoint gate of owned workloads.
pub const GUEST_LAUNCH: &str = "/usr/local/lib/awman/awman-launch";
/// The kit digest baked into the image at build time.
pub const GUEST_KIT_ID: &str = "/usr/local/lib/awman/kit-id";
/// Root-owned receipt of the last successful apply.
pub const GUEST_RECEIPT: &str = "/var/lib/awman/receipt";
/// Session variable carrying the launch id to the entrypoint gate.
pub const LAUNCH_ID_ENV: &str = "AWMAN_LAUNCH_ID";

/// Apply script exit codes.
pub const APPLY_EXIT_OK: i32 = 0;
/// The image's baked kit digest differs from the one the host expects.
pub const APPLY_EXIT_STALE_KIT: i32 = 3;
/// Malformed wire, unknown file id, or a file not managed for this agent.
pub const APPLY_EXIT_PROTOCOL: i32 = 64;
/// A decoded file or a written file did not match its SHA-256.
pub const APPLY_EXIT_INTEGRITY: i32 = 65;
/// A required helper (`base64`, `sha256sum`, `install`) is missing.
pub const APPLY_EXIT_MISSING_HELPER: i32 = 69;
/// A managed file or the receipt could not be written.
pub const APPLY_EXIT_WRITE: i32 = 73;
/// Not running as root.
pub const APPLY_EXIT_NOT_ROOT: i32 = 77;
/// Entrypoint gate: no receipt for this launch id.
pub const LAUNCH_EXIT_NO_RECEIPT: i32 = 78;
/// `sbx exec`: the command does not exist in the sandbox (documented by
/// Docker). For the apply script this means a sandbox without an awman v3 kit.
pub const EXEC_EXIT_COMMAND_MISSING: i32 = 127;

/// One managed file to write this launch. An id of the agent that is absent
/// from the document is removed from the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyFile {
    pub id: String,
    pub content: Vec<u8>,
}

/// The per-launch apply document. Carries policy and context only; it has no
/// field that could hold a credential, a prompt or an environment value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyDocument {
    /// Unique per launch. Letters, digits and `-` only.
    pub launch_id: String,
    pub agent: String,
    /// Digest of the kit the host expects the sandbox to have been built from.
    pub kit_digest: String,
    pub files: Vec<ApplyFile>,
}

impl ApplyDocument {
    /// Encode for the apply script's stdin:
    ///
    /// ```text
    /// AWMAN-APPLY 2
    /// launch <launch_id>
    /// agent <agent>
    /// kit <kit_digest>
    /// file <id> <sha256-hex> <base64 | ->
    /// end
    /// ```
    ///
    /// One `file` line per file, in the given order; `-` stands for empty
    /// content. Lines end with `\n`. Base64 is standard, padded, unwrapped.
    pub fn encode(&self) -> Result<Vec<u8>, SbxContractError> {
        let token = |name: &str, value: &str| -> Result<(), SbxContractError> {
            let ok = !value.is_empty()
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if ok {
                Ok(())
            } else {
                Err(SbxContractError(format!(
                    "apply document {name} '{value}' is not a plain token"
                )))
            }
        };
        token("launch id", &self.launch_id)?;
        token("agent", &self.agent)?;
        token("kit digest", &self.kit_digest)?;
        let mut out = format!(
            "AWMAN-APPLY {APPLY_PROTOCOL}\nlaunch {}\nagent {}\nkit {}\n",
            self.launch_id, self.agent, self.kit_digest
        );
        let mut seen: Vec<&str> = Vec::new();
        for file in &self.files {
            token("file id", &file.id)?;
            if seen.contains(&file.id.as_str()) {
                return Err(SbxContractError(format!(
                    "apply document lists file '{}' twice",
                    file.id
                )));
            }
            seen.push(&file.id);
            let body = if file.content.is_empty() {
                "-".to_string()
            } else {
                base64_standard(&file.content)
            };
            out.push_str(&format!(
                "file {} {} {}\n",
                file.id,
                sha256_hex(&file.content),
                body
            ));
        }
        out.push_str("end\n");
        Ok(out.into_bytes())
    }

    /// The exact final stdout line a successful apply prints.
    pub fn expected_ack(&self) -> String {
        format!(
            "AWMAN-APPLY-OK {} {} {}",
            self.launch_id,
            self.kit_digest,
            self.files.len()
        )
    }
}

/// Whether `stdout` of the apply script acknowledges exactly `document`.
/// The exit status must be checked separately and must be `APPLY_EXIT_OK`.
pub fn apply_acknowledged(document: &ApplyDocument, stdout: &str) -> bool {
    stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|line| line.trim_end() == document.expected_ack())
}

// ─── Emitted kit: manifest and digest ───────────────────────────────────────

/// Sub-directory of the awman kits root that holds v3 kits; the v1 layout
/// (`<kits>/<agent>/spec.yaml`) is never read or written by v3 code.
pub const KITS_V3_SUBDIR: &str = "v3";
/// Manifest file at the root of every emitted kit directory.
pub const KIT_MANIFEST_FILE: &str = "awman-kit.json";
/// Build-context directory copied to the image root.
pub const KIT_ROOTFS_DIR: &str = "rootfs";
/// Path, relative to the kit directory, of the baked kit digest. It is the
/// one file excluded from the digest it records.
pub const KIT_ID_REL: &str = "rootfs/usr/local/lib/awman/kit-id";
pub const KIT_MANIFEST_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KitKind {
    Mixin,
    Workload,
}

/// `awman-kit.json`. Contains nothing that changes between awman releases
/// unless the kit itself changes, so an awman upgrade alone never makes a
/// sandbox stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KitManifest {
    pub schema: u32,
    pub agent: String,
    pub kind: KitKind,
    /// Directory name and descriptor stem.
    pub handle: String,
    /// Descriptor file name, `<handle>.yaml`.
    pub descriptor: String,
    /// Recipe file name, `<handle>.dockerfile`.
    pub dockerfile: String,
    pub apply_protocol: u32,
}

/// A kit directory that was emitted completely and matches its own digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedKit {
    pub dir: PathBuf,
    pub manifest: KitManifest,
    /// `sha256:<hex>`-free lowercase hex digest of the kit sources.
    pub digest: String,
}

/// What `sbx create` is given to select the composition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SbxCreateSelector {
    /// `sbx create --kit <mixin_dir> ... <workload> <workspace>`
    PublishedWithMixin {
        workload: String,
        mixin_dir: PathBuf,
    },
    /// `sbx create ... <kit_dir> <workspace>`
    OwnedWorkload { kit_dir: PathBuf },
}

/// Directory of `agent`'s v3 kit under the awman kits root, or `None` when
/// the agent has no SBX route.
pub fn kit_dir_v3(kits_root: &Path, agent: &str) -> Option<PathBuf> {
    let handle = sbx_route(agent)?.kit_handle()?;
    Some(kits_root.join(KITS_V3_SUBDIR).join(handle))
}

impl EmittedKit {
    /// Load and verify an emitted kit directory. Fails when the manifest is
    /// missing or unknown, a declared file is absent, the directory contains
    /// a symlink, or the baked `kit-id` differs from the recomputed digest.
    pub fn load(dir: &Path) -> Result<Self, SbxContractError> {
        let manifest_path = dir.join(KIT_MANIFEST_FILE);
        let bytes = std::fs::read(&manifest_path)
            .map_err(|e| SbxContractError(format!("read {}: {e}", manifest_path.display())))?;
        let manifest: KitManifest = serde_json::from_slice(&bytes)
            .map_err(|e| SbxContractError(format!("parse {}: {e}", manifest_path.display())))?;
        if manifest.schema != KIT_MANIFEST_SCHEMA {
            return Err(SbxContractError(format!(
                "{} has unsupported schema {}",
                manifest_path.display(),
                manifest.schema
            )));
        }
        if manifest.apply_protocol != APPLY_PROTOCOL {
            return Err(SbxContractError(format!(
                "{} speaks apply protocol {}, this awman speaks {APPLY_PROTOCOL}",
                manifest_path.display(),
                manifest.apply_protocol
            )));
        }
        let route = sbx_route(&manifest.agent)
            .filter(SbxRoute::is_supported)
            .ok_or_else(|| {
                SbxContractError(format!(
                    "kit manifest names agent '{}', which has no Docker Sandboxes route",
                    manifest.agent
                ))
            })?;
        let expected_kind = if route.is_owned_workload() {
            KitKind::Workload
        } else {
            KitKind::Mixin
        };
        if manifest.kind != expected_kind
            || Some(manifest.handle.as_str()) != route.kit_handle().as_deref()
            || dir.file_name().and_then(|n| n.to_str()) != Some(manifest.handle.as_str())
            || manifest.descriptor != format!("{}.yaml", manifest.handle)
            || manifest.dockerfile != format!("{}.dockerfile", manifest.handle)
        {
            return Err(SbxContractError(format!(
                "{} does not describe the kit awman selects for '{}'",
                manifest_path.display(),
                manifest.agent
            )));
        }
        for name in [&manifest.descriptor, &manifest.dockerfile] {
            if !dir.join(name).is_file() {
                return Err(SbxContractError(format!(
                    "kit {} is missing {name}",
                    dir.display()
                )));
            }
        }
        let digest = kit_digest(dir)?;
        let id_path = dir.join(KIT_ID_REL);
        let baked = std::fs::read_to_string(&id_path)
            .map_err(|e| SbxContractError(format!("read {}: {e}", id_path.display())))?;
        if baked.trim_end() != digest {
            return Err(SbxContractError(format!(
                "kit {} was modified after it was emitted; run `awman ready` again",
                dir.display()
            )));
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            manifest,
            digest,
        })
    }

    /// The `sbx create` selector for this kit.
    pub fn create_selector(&self) -> Result<SbxCreateSelector, SbxContractError> {
        let route = sbx_route(&self.manifest.agent)
            .ok_or_else(|| SbxContractError(format!("unknown agent '{}'", self.manifest.agent)))?;
        match route.disposition {
            SbxDisposition::PublishedWorkloadWithMixin { workload } => {
                Ok(SbxCreateSelector::PublishedWithMixin {
                    workload: workload.to_string(),
                    mixin_dir: self.dir.clone(),
                })
            }
            SbxDisposition::OwnedWorkload => Ok(SbxCreateSelector::OwnedWorkload {
                kit_dir: self.dir.clone(),
            }),
            SbxDisposition::Refused { reason } => Err(SbxContractError(format!(
                "agent '{}' {reason}",
                self.manifest.agent
            ))),
        }
    }
}

/// Digest of a kit directory: SHA-256 over every regular file except
/// [`KIT_ID_REL`], in ascending order of its `/`-separated relative path, each
/// contributing `<path> NUL <decimal length> NUL <bytes>`. File modes and
/// timestamps do not contribute. A symlink or special file is an error.
///
/// The emitter writes all other files, calls this, then writes the result
/// plus a newline to [`KIT_ID_REL`].
pub fn kit_digest(dir: &Path) -> Result<String, SbxContractError> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    collect_files(dir, "", &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (rel, path) in &files {
        if rel == KIT_ID_REL {
            continue;
        }
        let bytes = std::fs::read(path)
            .map_err(|e| SbxContractError(format!("read {}: {e}", path.display())))?;
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update(bytes.len().to_string().as_bytes());
        hasher.update([0u8]);
        hasher.update(&bytes);
    }
    Ok(hex(hasher.finalize().as_slice()))
}

fn collect_files(
    dir: &Path,
    prefix: &str,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), SbxContractError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| SbxContractError(format!("read {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| SbxContractError(format!("read {}: {e}", dir.display())))?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(SbxContractError(format!(
                "kit file name under {} is not UTF-8",
                dir.display()
            )));
        };
        let rel = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| SbxContractError(format!("stat {}: {e}", path.display())))?;
        if meta.is_dir() {
            collect_files(&path, &rel, out)?;
        } else if meta.is_file() {
            out.push((rel, path));
        } else {
            return Err(SbxContractError(format!(
                "kit entry {} is not a regular file or directory",
                path.display()
            )));
        }
    }
    Ok(())
}

// ─── Small helpers ──────────────────────────────────────────────────────────

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes).as_slice())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Standard padded base64 without line wrapping (what `base64 -d` reads).
fn base64_standard(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        out.push(TABLE[(b[0] >> 2) as usize] as char);
        out.push(TABLE[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b[2] & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Error of this module. Callers map it into `EngineError::Sandbox`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SbxContractError(pub String);

impl fmt::Display for SbxContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SbxContractError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_admission_refuses_old_and_unreadable_versions() {
        assert_eq!(
            admit_cli("sbx version v0.54.1 (abc123)\n"),
            SbxCliAdmission::Admitted(SbxCliVersion::new(0, 54, 1))
        );
        assert_eq!(
            admit_cli("0.53.0"),
            SbxCliAdmission::Admitted(SbxCliVersion::new(0, 53, 0))
        );
        assert!(matches!(
            admit_cli("sbx 0.52.9"),
            SbxCliAdmission::TooOld { .. }
        ));
        assert!(matches!(
            admit_cli("Client: unknown"),
            SbxCliAdmission::Unparseable { .. }
        ));
    }

    #[test]
    fn matrix_covers_every_first_party_agent_and_refuses_the_private_session_agents() {
        for agent in SBX_AGENTS {
            let route = sbx_route(agent).expect(agent);
            assert_eq!(route.agent, agent);
            assert_eq!(
                route.is_supported(),
                route.kit_handle().is_some(),
                "{agent}"
            );
            for delivery in [route.mode, route.tools, route.model, route.agent_flags] {
                assert_ne!(delivery, SbxDelivery::NotedOnly, "{agent}");
            }
        }
        for agent in ["cursor", "opencode-v2"] {
            assert!(!sbx_route(agent).unwrap().is_supported(), "{agent}");
        }
        assert!(sbx_route("my-custom-agent").is_none());
    }

    #[test]
    fn apply_document_encodes_and_is_acknowledged_exactly() {
        let document = ApplyDocument {
            launch_id: "0b0e-1".into(),
            agent: "claude".into(),
            kit_digest: "ab12".into(),
            files: vec![
                ApplyFile {
                    id: CLAUDE_MANAGED_SETTINGS.into(),
                    content: b"{}".to_vec(),
                },
                ApplyFile {
                    id: CLAUDE_MEMORY.into(),
                    content: Vec::new(),
                },
            ],
        };
        let text = String::from_utf8(document.encode().unwrap()).unwrap();
        assert_eq!(
            text,
            format!(
                "AWMAN-APPLY 2\nlaunch 0b0e-1\nagent claude\nkit ab12\n\
                 file claude-managed-settings {} e30=\nfile claude-memory {} -\nend\n",
                sha256_hex(b"{}"),
                sha256_hex(b"")
            )
        );
        assert!(apply_acknowledged(
            &document,
            "note\nAWMAN-APPLY-OK 0b0e-1 ab12 2\n"
        ));
        assert!(!apply_acknowledged(
            &document,
            "AWMAN-APPLY-OK 0b0e-1 ab12 1\n"
        ));
        assert!(!apply_acknowledged(
            &document,
            "AWMAN-APPLY-OK other ab12 2\n"
        ));
        let mut bad = document.clone();
        bad.launch_id = "a b".into();
        assert!(bad.encode().is_err());
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64_standard(b"f"), "Zg==");
        assert_eq!(base64_standard(b"fo"), "Zm8=");
        assert_eq!(base64_standard(b"foo"), "Zm9v");
        assert_eq!(base64_standard(b"foob"), "Zm9vYg==");
        assert_eq!(base64_standard(&[0xfb, 0xff]), "+/8=");
    }
}
