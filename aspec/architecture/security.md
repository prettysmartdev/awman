# Security

## Custom agent definitions

Repository definitions require approval for their current content hash. Global
definitions also require approval when they declare `[ping]`. Approval covers the
TOML and any referenced Dockerfile bytes; worktree sessions use the invoker's
definition root and trust key. Only the registry constructs a `ResolvedAgent`,
which rendering, provisioning, image preparation and host ping execution require.
Review-only `UntrustedAgent` data cannot be passed to these sinks. Engines never
prompt or grant approval implicitly.

Custom agent definitions never cause host execution except through their optional
`[ping]`, run by `HostAgentPinger` only during a full `awman ready` (never
`--prepare-only`) and only after approval for the exact definition hash and
canonical resolved executable path. Approval also binds a missing-program
sentinel; changes to PATH resolution or a symlink target require approval
again. `ResolvedAgent` pins the approved canonical program for execution, so
the path cannot change between review and spawn. Existing approvals that bind
only the definition hash must be renewed before a custom ping. The approval
prompt shows the exact host command and resolved path; without `[ping]`,
`ready` skips the host check. Non-ping approvals are unchanged.

Custom host pings are argv-only, append one fixed greeting, run in a fresh private
scratch directory and time out after 10 seconds during `ready`, with the child
killed on drop. Ready launches host checks concurrently and reports their
results together after the batch finishes; a timeout is a summary warning.
Credential-refresh pings retain their separate 60-second timeout.
Definition environment fields apply only inside containers. Automatic credential
refresh remains first-party only. Container install steps use JSON exec-form RUN
instructions so shell continuations and heredocs cannot consume later Dockerfile
instructions. Definition strings reject terminal control characters.

Settings mounts reject protected host paths, their descendants and their ancestors,
using ASCII case-insensitive comparisons on every OS. The mount boundary also
canonicalizes the source relative to canonical HOME and refuses symlink targets
outside HOME or overlapping protected paths. Protection is by real location, not
spelling: the session's actual awman data home (including an XDG or
`AWMAN_CONFIG_HOME` location, which holds the trust store and global definitions)
and the canonical target of every protected directory (a relocated `~/.awman` or
`~/.ssh` symlink) are refused too, as are their ancestors. Approval records use bounded private
file reads and atomic mode-0600 writes; malformed or unsafe stores grant no trust.

An existing headerless agent Dockerfile is user-owned and may differ from the
approved definition's recipe. A matching generated header proves only the input
hash, not the body. The engine exposes a typed preview and warns every time a
divergent body is used, including a forged header. Remove the header line to take
ownership, or delete the file to regenerate it. These files are image build inputs
and need review independently when their contents differ from the approved recipe.

Session configuration copies obey the same real-location boundary as settings
mounts, including the actual awman data home and relocated protected aliases.
Copy traversal and file reads use held directory descriptors and no-follow opens;
links and unsafe entries fail the launch rather than silently dropping policy.
An absent config starts empty. Platforms without secure traversal refuse an
existing config. Copies are bounded and private, and never written back.

Built-in OpenCode 2 and Cursor require explicit caller authorization for yolo;
frontend defaults and implied dynamic/squad automation are not authorization.
They are refused on Docker Sandboxes until per-execution state and credential
scope isolation exists. Docker Sandboxes is experimental on macOS arm64 and
Linux x86_64. Version observations are diagnostics, not a launch gate:
an unreadable, unknown or newer version only warns, and only a confirmed wrong
OpenCode major refuses. No exact-version pin or tested-build approval exists.
Resolved env grants and secret overrides are registered before runtime output is
published. Streaming redaction precedes terminal replay, status parsing, logs and
failure tails, and protects secrets spanning byte chunks.

## Guidance:

- **Trusted work item providers** — `workItems.source` may be repository
  scoped, but `workItems.providers.<name>` definitions are global-only. A
  repository checkout is untrusted input and cannot choose the destination
  that receives a user's provider token or replace trusted provider settings.
  HTTP providers must use HTTPS, except for literal loopback HTTP used by a
  local service. Provider URLs must stay under the configured base URL;
  URL validation happens before credentials are attached, redirects are not
  followed, and provider requests have a 5-second connection timeout and a
  15-second total timeout. Responses are limited to 2 MiB and work item
  bodies to 1 MiB. Do not include credentials in provider URLs.

- Never directly execute any code assistant on the developer host machine. Every single code agent action should be executed by running a Docker image that has the agent tool installed and passing an entrypoing command to direct the agentic tool. The one exception below is a confirmed, permanent product decision (WI 0078, audit A02): it is to be preserved and hardened, not removed, and `AGENTS.md` states the same rule.
  - **Sole exception — the `ready` local-agent check** (`HostAgentPinger::ping`
    in `src/engine/ready/host_agent.rs`, originally the
    `ReadyPhase::CheckingLocalAgent` arm): the configured agent's binary IS
    executed on the host only for this one ready-check ping, whose sole effect
    is to cause the host agent to rotate its own credential (and, during
    `awman ready`, to verify it is installed and authenticated).
    **`HostAgentPinger` is the only type in awman permitted to spawn an agent
    binary on the host**; a second such type, or a bare function that does it,
    is a security violation regardless of what it is called. Exactly four
    triggers are authorized, and no others:
    - (a) during `awman ready` under the default `PreparationPolicy::Full` (`ReadyPhase::CheckingLocalAgent`), once for the selected agent and each other repo agent discovered through `.awman/Dockerfile.<agent>`, after resolving its definition through the normal trust boundary; custom definitions without `[ping]` are skipped;
    - (b) periodically by the credential-refresh monitor's tick loop while credentialed agent sessions are live (`CredentialRefreshMonitor::tick`, which reaches the host through the `HostAgentPinger` it holds), bounded by the per-agent backoff and a per-tick timeout;
    - (c) once, synchronously and time-bounded, as the workflow pre-step guard when a step's file-delivered credential is within the refresh threshold of expiry (`refresh_credential_blocking` from `exec_workflow.rs`); and
    - (d) at most once per workflow step, as auth-failure recovery after a step exits with an authentication error the descriptor recognizes (`recover_auth_failure`).

    **Preparation-only operations are excluded.** `awman ready --prepare-only` selects `PreparationPolicy::ImagesOnly`, which never reaches `CheckingLocalAgent`, never constructs a ping, never registers credential refresh and never runs an agent audit (`--refresh` is rejected with it). It keeps definition trust, image inspection/build/import and registry authentication, and its summary reports authentication as `not_checked`. Only an explicit `--prepare-only` selects it; ordinary `ready` and daemon session setup keep the full checks, and no code infers preparation-only intent.

    Every trigger runs the identical, unchanged ping: the prompt is hardcoded (from the `GREETINGS` table), there is no user input, no repo content, the process runs in a dedicated empty directory outside the repository (never the repo working directory), and the cheapest available model is pinned. Triggers (c) and (d) route through the same monitor/ping path as (a) and (b) — a single type, `HostAgentPinger`, now handles all four; they widen *when* the ping runs, never *what* it does. No other code path may execute an agent on the host. Any new host-side agent invocation beyond this check is a security violation.

    **Executable selection (first-party pings).** A scratch working directory does not constrain which program runs, so first-party pings resolve their executable through a dedicated installed-program resolver, never the general host-CLI lookup:
    - PATH entries that are empty, relative, or not absolute after snapshot resolution are skipped; they are never resolved against the invoking checkout or cwd.
    - Each remaining candidate is canonicalized (symlinks resolved). A candidate whose canonical path lies inside the session's canonical Git root or working directory, or inside any linked worktree of that repository, is refused; so is a symlink alias outside the checkout that resolves into it. Lookup continues with later PATH entries; only a validated installed candidate may execute.
    - The canonical path chosen by the resolver is the path that is spawned (argv-only, cleared environment plus the snapshot's spawn environment). There is no fallback to an unchecked name, relative path or `cwd`-joined path.
    - A missing program and a refused program are distinct, truthful results (`not installed` / `refused: resolves inside the checkout`); neither is reported as authenticated, and neither blocks container work.
    - Custom-definition `[ping]` commands keep their own trust rule (approval of the exact definition hash and resolved path, above). First-party lookup never inherits a custom approval, and the resolver does not widen or bypass custom trust.
    - The fixed greeting, absence of task/repository content, empty private cwd, bounded timeouts, kill-on-drop cleanup, the four triggers and the `--prepare-only` exclusion are unchanged. Tests use owned fake executables and private environment snapshots; no test runs a real host provider agent.

    Do not conflate `HostAgentPinger` with `engine::daemon::DaemonSupervisor`,
    which is the only type permitted to spawn the *daemon* binary
    (`awman api start` / `awman squad start`'s detached child) — a distinct
    exception for a distinct purpose.
- **Credential delivery** — For Claude on container-class runtimes, credentials are delivered as an awman-authored, refresh-token-free credential file inside the staged settings overlay (mode `0600`, atomically replaced via `rename`); the host's own credential files and refresh tokens are never mounted into a container.
- **Native plan mode is not a filesystem boundary** — `--plan` only selects the agent's own starting mode; the guarantee that files cannot change is `--read-only-workspace`, an independent runtime mount restriction. Neither option reads the other. awman refuses (`agent_mode_conflict`) contradictory provider mode selectors, drops `OPENCODE_PERMISSION` from a plan launch, and refuses an OpenCode config it cannot inspect (symlinked, relocated, oversized, or a mount replacing an inspected file) rather than assume plan mode will hold. A route that cannot carry the mode is refused before image setup, never launched with write access instead.
- **Settings profiles** — Profile stores live under the private data home (directories `0700`, files `0600`, symlinked ancestors inside the store refused, never inside the mounted workspace). Imports and retained snapshots go through the descriptor-relative, no-follow, bounded copier; credentials, credential-bearing endpoints, executable hooks, plugins and permissive approval rules never enter a retained snapshot, and restrictive rules that cannot be kept cause refusal. Only the execution's private run tree is mounted. One OS-held lease per profile name serializes writers; liveness is decided from saved runtime resource identity, never from a free lock alone, and uncertainty leaves the profile `closing`. Profile selection and `agents profile *` are local-owner operations (UID/SID plus data-home binding), re-checked when a queued command starts; network, browser, forwarded and relay callers cannot select a profile, even through workflow steps. Listings never expose native IDs, transcripts or host paths.
- **Automation ownership** — Automation executions, guarded turns and credential operations are authorized by the owner-local origin (default deny for unknown, network, browser, work-server and relay origins) and by actual possession of the terminal's `InputGrant`; neither a client-supplied argument nor a listener name grants authority. Each owned terminal accepts exactly one writer path for guarded bytes, authorization is re-evaluated at write ingress under the admission lock, and cancellation or revocation drops queued writes. The native lifecycle channel is a private `0700` directory mounted at `/awman/automation` with a per-execution 256-bit nonce; foreign nonces and non-regular or symlinked record files are protocol failures, not trusted input. Prompts, native records and results are redacted before persistence, events or transport, and task text is never logged.
- **Credential material in transit** — Replacement credentials travel only as request-only secret carriers (`SecretString`/`CommandSecrets`) with a strict duplicate-rejecting decoder, bounded bodies, static error text, constant-time equality and zeroing; they are never stored, hashed for deduplication or echoed. Metadata persisted for idempotency is non-secret and canonical. Helper payloads are written atomically through held directory descriptors with no-follow opens and hard-link/symlink rejection; a rename never counts as the agent consuming a credential. Each agent credential has one refresh owner and awman never runs a second refresher. Credential live-update and restart capabilities are withheld until real native consumption is evidenced.
- **Builtin microVM boundary** — The builtin runtime executes untrusted agent code inside a Linux microVM. Its worker is the same awman executable re-executed in a private internal mode; the kernel and guest-agent payloads are embedded build inputs with pinned provenance and verified hashes. Runtime execution must not extract or download a host executable, firmware DSO, or helper bundle. KVM on Linux or the required Hypervisor.framework entitlement on Apple Silicon is mandatory; missing access is a clear startup error, never a host-execution or weaker-backend fallback. Explicit `builtin.builder.enabled` is an image preparation exception, owned by the Layer 1 `AgentImagePreparer`: it may invoke only the local Docker or Apple Containers CLI selected by the resolved `docker-local` (Unix socket) or `apple-local` source, reached only through the `LocalImageBuilder` build handle, and the built image is imported from that same source. Only callers whose `BuildPolicy` permits it may build. With an enabled compatible local builder, `awman ready` (`Ready`), API session setup, the `chat`/`exec prompt` launch check, workflow prerequisite solves (including dynamic workflows and worker leases), and squad evaluation compare source input hashes and may build missing or stale images, even when an older copy is installed. This follows the image freshness contract in the architecture design and WI 0163. Installed images therefore still permit read-only source metadata inspection when that builder is enabled; a failed session preparation retains the previous image with a stale-image warning so the session can repair its build inputs. Explicit `ready --build` fails on a preparation error. `ImportOnly` and `CheckOnly` policies never build or probe a builder; launches with the builder disabled or without a buildable source retain check-only behavior for installed images. These CLIs do not execute agents or start/manage VMs, and agent execution never leaves the builtin runtime; other sources never cause a host build, and a disabled builder, an incompatible source or a missing builder fails closed to normal source import rather than to another source or runtime.
- **Per-session builtin launch settings** — The builtin guest network policy, vCPU count and memory are resolved from each session's effective config at every launch (foreground and background alike), never fixed once per process, so a repo's narrowing of the global network policy applies wherever an agent for that repo runs. A repo block that would widen the global policy is a configuration error at launch. The builtin state directory is process-wide: a repo-level `builtin.stateDir` that resolves to a different directory than the process's is rejected, naming both paths, and is never silently ignored or followed.
- **Tool restrictions fail closed** — An agent that lacks an allowed/disallowed-tools flag is a configuration error on every available runtime (Docker, Apple Containers and builtin); a tool restriction is never silently dropped. The configured `yoloDisallowedTools` list is one: `AgentEngine` adds it to every `--yolo` launch and refuses an agent that cannot deliver it. Docker Sandboxes is experimental on macOS arm64 and Linux x86_64; sandbox restrictions require a verified synchronous apply before agent startup. The engine retains the admitted mount authority and pinned source identity through SBX creation. Existing SBX sandboxes are refused until exclusion across policy apply and agent startup is qualified; an idle inspection and an awman-only lock are insufficient. Agent-home context files require a root-owned last-content digest before replacement or removal, and policy destinations must have no symlink ancestors.
- **Builtin memory accounting** — The configured memory is the requested RAM allocation passed to the VMM. Guest-visible `MemTotal` reflects the platform boot layout and excludes Linux-reserved pages. The pinned x86_64 bundled-kernel layout below the MMIO gap adds low RAM and the kernel extent to that allocation. Native acceptance must bound the observed total using the pinned layout and prove that a per-launch memory increase changes guest RAM accordingly. Host memory budgeting must also cover VM overhead and concurrent sessions; guest OOM handling is not a host-process memory limit.

- **Private runtime state** — Builtin state, worker control sockets, locks, logs, and writable disk images live under a user-private runtime directory with restrictive permissions and collision-safe ownership. Unix socket paths must fit the platform limit; configuration must leave room for derived paths. Do not use predictable world-writable socket locations or place credentials in worker argv, logs, or persistent runtime metadata.
- **Builtin catalog upgrades** — A builtin state directory created by an older awman is upgraded in place only from an allowlist of known protocol versions. The previous catalog is backed up (`db.pre-<protocol>`, with a manifest of its files, published atomically under an exclusively claimed name, never overwritten) before any change, copied without following symlinks (descriptor-relative traversal), and synced before anything depends on it. On failure, sandbox files moved aside during the upgrade are put back and the catalog is restored through a journalled restore; a backup that does not match its manifest is never restored. The upgrade is refused while the older awman's sandbox lifecycle locks, image leases or image-import lock are held, and it keeps the leases and the import lock until it ends, so an older awman cannot launch, import or remove an image underneath it. Unversioned, newer, or unknown catalogs are never modified, and there is no downgrade.
- **Image provenance and source isolation** — Image acquisition uses an explicit typed source (`registry`, `docker-local`, `apple-local`, or `archive`), platform selection, and content digests. A source daemon or registry is used only to acquire an image; execution uses the imported local image. Never scrape private daemon storage or resolve a source against another source kind. Unsupported or blocked source adapters fail explicitly; there is no implicit fallback to another source or runtime. The Apple store is reached only through the in-process XPC bridge (`engine::oci::apple_store`), which pins exact service releases (version and commit) before any image route, has the helper write only into awman's private leased staging directory, and accepts only a single-link regular file owned by the user before normal validation.
- **macOS code signing (ad-hoc)** — Hypervisor.framework requires the `com.apple.security.hypervisor` entitlement, which only a code signature can carry. The released Apple Silicon binary is ad-hoc signed with exactly the entitlements in `packaging/macos/awman.entitlements`; `com.apple.security.cs.disable-library-validation` is deliberately absent because awman loads no dynamic library. An ad-hoc signature proves integrity since signing, not origin: users verify the published SHA-256. It is not Developer ID signed or notarized, so quarantined downloads are blocked by Gatekeeper and background (launchd) processes' folder-access grants do not survive upgrades; both are documented for users.
- **Unsafe code boundary** — The awman library is `#![forbid(unsafe_code)]`. Unsafe code lives only in the `awman-host-shims` workspace crate: the builtin worker's descriptor probe, the embedded version marker, and the macOS libxpc transport (`crates/awman-host-shims/src/apple_xpc.rs`). The CLI owns the single awman binary and connects those safe shim interfaces to the engine through a safe adapter; shims may import only awman's Layer 0 constants. The transport uses only public libxpc/libSystem symbols and one capture-free global block. It extracts, loads and spawns nothing.
- **Opt-in Docker socket bridge** — The builtin runtime does not expose a host Docker socket. If a future builtin bridge is implemented, it must require an explicit user opt-in, mount only the selected socket at the documented guest path, and receive its own threat review and integration gate. The existing `--allow-docker` bridge remains a Docker CLI-backend-only exception and stays off by default. As defense in depth, the builtin driver's spec check (`SandboxSpec::check`) refuses any mount whose host path is not a regular file or directory before a VM is created, so a Unix socket, FIFO, or device node — the Docker socket included — can never be shared into a guest through an overlay or other mount.
- **Builtin guest network policy** — Policy enforcement belongs in the SDK host-side worker stack. Guest traffic must obey configured destinations, DNS, host-port and resource boundaries without TLS interception, ambient proxy inheritance or a host helper. Repo settings may only narrow the global ceiling. The explicit `strict_sni` mode keeps strict checking enabled and admits end-to-end TLS only when visible ClientHello SNI matches an allowed name and that exact name has a DNS binding to the destination IP. Missing/unlisted SNI, missing bindings and sibling-name bindings cannot authorize a name-based TLS flow. Host-side SDK tests exercise a real TLS handshake and denied requests before upstream connection. This boundary does not inspect encrypted HTTP `Host`/`:authority`, domain fronting behind an allowed service, or ECH inner names; an allowed outer SNI establishes only that outer name. Name rules admit TCP only, preventing cached DNS from authorizing UDP/QUIC. Native guest denied-egress, positive-control and isolation tests pass on Apple Silicon (WI 0123); host-side tests alone do not prove guest packet routing, and Linux guests are verified in WI 0124. See the [WI 0121 evidence register](../review-notes/0121-evidence-register.md).
- Never mount any directory to any Docker container other than the current directory. If any parent directories are a Git repo root, the aspec CLI will prompt the user if the mounted directory should be limited to the current CWD or can be expanded to the Git repo root. Follow this instruction for every single container launched.
  - **Explicit case — session worktrees.** `chat` and `exec prompt` with `--branch`, `--pr` or `--worktree` mount a linked worktree of the *same* repository as the working tree, plus that repository's main `.git` directory (a linked worktree's history lives there). No parent directory is mounted. The worktree must be verified as a registered linked worktree of the invoking repository before it is mounted, and a runtime that cannot mount the extra Git directory refuses the launch rather than starting with a dangling pointer. The selectors exist only on the local CLI/TUI frontends; the API refuses them, so remote callers cannot choose a host directory to mount. All configuration (repo config, trust, agents, overlays, the Dockerfile used to infer the container `HOME`, and the base directory for relative overlay host paths) is read from the invoking checkout, never from the worktree's contents. Host `ls-remote`/`fetch` for these selectors runs against the user's repository config, which agents can write, so it refuses repository-scoped (local or worktree) transport programs and credential sources (`core.sshCommand`, `core.gitProxy`, `core.askPass`, `credential.*helper`, `remote.*.vcs|uploadpack|receivepack|proxy`, `http.*` proxy/TLS/cookie/resolve settings), pins the remote's upload/receive-pack programs, runs SSH with `BatchMode=yes` unless the user chose an SSH command in their environment or global config, and is bounded by a timeout. The `gh` lookup names the `github.com` host so an ambient `GH_HOST` cannot redirect it.
- **API working-directory allowlist.** A `local` API session may mount only (1) an exact `--workdirs`/`api.workDirs` entry, or (2) a *direct child* of a `--workdir-root`/`api.workDirRoots` entry whose git root is the child itself (`git rev-parse --show-toplevel` equals the child) and whose `.git` is a real directory, not a pointer file or symlink. The requested path is canonicalized (symlinks resolved) before either rule is applied, and roots are canonicalized again at every request. Rule 2's conditions keep the mounted tree inside the allowlist: the API mounts the session's git root, a non-repository child would resolve to an enclosing repository above the root, and a linked-worktree child would pull its main repository's `.git` into the container. A root that is a git repository or lies inside one is refused at startup and rechecked at request time; other discovery failures fail closed. Grandchildren of a root are refused at request time. Exact workdir mounts must not contain a root (or an ancestor of it), so agents cannot rename or replace other sessions' checkout directories. Workflow commands retain the session's resolved root rather than rereading writable `core.worktree`. Linked-worktree overlays come from the invoking repository's common Git directory only after verifying both the checkout pointer and administrative backlink; stale registrations and replacement symlinks do not establish ownership. Host Git disables hooks, fsmonitor, signing, automatic maintenance, and configured external content drivers (filters, diff commands/textconv and merge drivers), and does not read system Git configuration. Refusals name the failed rule and return HTTP 403.
  - **Sanctioned exception — the Docker socket.** `--allow-docker` mounts the
    host's `docker.sock` into the container (`/var/run/docker.sock` on
    Unix, an `npipe` on Windows) alongside the current-directory-only bind
    mount above, so a container that needs to drive Docker itself (e.g. an
    agent building or running images as part of its task) can reach the host
    daemon. This is a deliberate, opt-in widening of what the container can
    reach beyond its filesystem mount: a socket, not a directory, and
    off by default. `docker.rs`'s `allow_docker` argv builder is the only
    place this mount is constructed.
- **Mount authority (WI 0078, A01).** Admission produces a typed Layer 0 `MountAuthority { root, git_dirs, source }` and the launch may never exceed it. `root` is the canonical directory the caller is authorized to mount at `/workspace`; `git_dirs` lists the canonical auxiliary Git directories (a linked worktree's main `.git`) that were verified for it; `source` is one of `LocalInvocation` (CLI/TUI: the cwd, or the Git root the user confirmed), `ExactWorkdir`, `WorkdirRootChild`, `RemoteClone` (awman-owned clone under the API root) or `VerifiedWorktree`.
  - An exact `workDirs` entry authorizes exactly that directory. If the requested directory is not its own Git root (it lies inside an enclosing checkout, or its `.git` is a pointer file or symlink), admission refuses with HTTP 403 and rule `exact_workdir_not_git_root`, naming the enclosing root the operator would have to allowlist explicitly. A directory with no enclosing repository keeps working as a non-Git workdir. The parent is never mounted, and an interactive confirmation never widens an API caller's allowlist; noninteractive callers fail closed.
  - `SessionCreatePlan` carries the authority into `SessionSetup`; a session opened with an explicit authority must resolve its mount root to `authority.root` or setup fails. The authority is persisted with the session row (`sessions.mount_authority`, versioned JSON).
  - Restore re-evaluates the current allowlist: a saved authority is an upper bound, not a grant. A session whose authority is absent (rows written before this contract), no longer granted, or no longer canonicalizes to the same path is not launchable: commands are refused with `mount_authority_revalidation_required`; nothing is inferred from the saved workdir or from Git discovery. `RemoteClone` authorities are re-derived only from the awman-owned clone path under the API root.
  - Immediately before every runtime create (first launch, retry and recovery alike) the engine canonicalizes the final workspace mount and every auxiliary Git mount again: the workspace host path must equal `authority.root` and each Git mount must be listed in `git_dirs` (or lie inside `authority.root`). Any difference, including a symlink or directory replaced after admission and a root derived from writable `core.worktree`, refuses the launch with `EngineError::MountOutsideAuthority` before any container or VM exists. Code that re-roots a session (worktrees, lease workspaces) derives a new authority through a named, verifying constructor; it never edits the root in place.

Builtin image-import validation applies to the selected image. Before invoking
Microsandbox's load-all archive API, awman projects the archive to exactly that
image and the requested awman tag. Unselected images, layers and extra tags are
not passed to the SDK. Config and layer blobs remain byte-identical; rewriting
outer selection metadata does not flatten the root filesystem. Acquisition
cache references retain their own identity and source metadata even when their
archive bytes are shared.

## Remote session push-back

When a remote API session is created with `push: true`, awman pushes the session branch to `origin` after a successful `exec` command. This is a host-side Git operation; all agent work still happens in containers. The clone lives under `<API root>/sessions/<id>/<repository-folder>`. The API root is selected in order from `AWMAN_API_ROOT`, `AWMAN_CONFIG_HOME/api`, `XDG_DATA_HOME/awman/api`, or `~/.awman/api`.

- **Credentials:** every host-side transport operation in a session (clone, setup fetch, merge-ref `ls-remote`, push-back `ls-remote` and push) disables system Git configuration and uses the session's credential choice. Without `git_auth`, the daemon user's global Git configuration and credentials (including SSH keys and user/global credential helpers) are used. With `git_auth`, inherited helpers and global authentication files are excluded, and the supplied credential is used. Prompting is disabled (`GIT_TERMINAL_PROMPT=0`; default SSH uses `BatchMode=yes`).
- **No force:** awman never force-pushes. A rejected push fails the command with a `push` stage error and leaves local commits in the clone.
- **Untrusted metadata:** agents can write the clone's `.git`, so absence of hooks immediately after clone provides no protection. Push-back imports only inert objects and refs into a private Git directory outside the mount, rejects symlinks/special files in imported metadata, disables hooks and fsmonitor explicitly, and never imports the clone's config, index, alternates, attributes, or signing/transport helpers. Work-tree staging/committing uses no system/global config, preventing configured filters from executing on agent files. Only identity values and the inert object format are preserved; transport uses a newly authored `origin` pinned to the session's persisted `repo_url`, with trusted host credentials. No agent-authored configuration is evaluated as host commands. Commit objects and the resulting index/ref are copied back without executing Git in the writable clone.
- **Scope:** only `refs/heads/<session branch>` on `origin` is pushed, and only when the session was explicitly created with `push: true`. Explicit workflow `push_branch` steps execute in the container, accept branch names rather than force options/refspecs, and delimit positional arguments with `--`.

### Host Git configuration and session credentials

Every host-side Git network operation disables the system Git configuration. A
push session without `git_auth` may use the daemon user's global Git
configuration, credential helpers, SSH agent and SSH configuration. It cannot
use system-wide Git settings. Repository-scoped transport settings are not
trusted for host transport.

When a session supplies `git_auth`, awman uses that credential consistently for
clone, fetch, remote-ref lookup and push. It clears inherited Git credential
helpers, does not consult global credential configuration, and uses a private
Git home so global authentication files such as `netrc` are not fallback
sources. The credential is held in daemon memory and served to Git through a
short-lived owner-only Unix socket or Windows named pipe; it is not written to
the session database or placed in process arguments or environment variables.
Only phase containers that need Git access receive the credential, through
private stdin into a container-local, memory-only Git credential cache. Its
socket is scoped to the session's protocol, host and repository path; removing
the phase container destroys the cache. A phase receiving it is trusted
with the reusable credential and can copy it; repository matching is not a
confinement boundary for a malicious recipient. Use repository-scoped,
short-lived credentials.

Repository URLs containing user information, query strings or fragments are
rejected. Supply credentials through `git_auth` or the daemon user's Git setup.
If the daemon restarts after a credentialed session is created, the secret is
gone. The session fails closed with `credentials_lost`; create a new session
with credentials rather than retrying the old one. Authentication refusal is
reported as `git_auth_failed`.

## Workflow prerequisite credentials

`HostSecretCommand` is a narrowly scoped host-execution exception for obtaining a workflow GitHub token. It accepts an argv vector and never invokes a shell; resolves and canonicalizes the executable before execution; refuses known first-party agent binaries, custom-agent binaries supplied by the command layer, and wrappers that would launder those binaries; clears the inherited environment to an explicit allowlist; uses a private temporary working directory; defaults stdin to null; and enforces a bounded timeout and process-group termination. Token output is validated and returned only as a secret value. When user approval is required, review shows the exact argv, resolved executable path, and timeout. Repository-scope commands require approval keyed to the canonical repository root and a hash of the exact argv; changing argv requires new approval. No credential-acquisition path may execute an agent binary on the host.

Workflow grants are ephemeral command state, never session or workflow-state data. Each grant is limited to the repository/run, the relevant remote and credential kind, and only the setup or teardown steps whose declared needs require it. A selected SSH key is mounted read-only to only the relevant phase-step container; token environment values are injected only into the relevant step, and token-authenticated CI receives the token through an explicit typed engine input. Before non-cancelled teardown, granted credentials are rechecked; only an existing host-command token grant configured for `beforeTeardown` may be refreshed. Cancellation cleanup receives no grants and skips publishing steps. Dynamic workflows narrow grants to the generated workflow's actual needs and discard unused grants.

An identity selected in a task prerequisite interview retains a typed
commit-step-only scope when saved on the task. It does not become a run-wide
fixed identity. Explicit fixed-identity configuration/flags retain their
existing run-wide scope. Older serialized policies without the scope field
retain that existing scope.

Remembered decisions are stored in `~/.awman/prerequisite-grants.json`, scoped by canonical repository root, normalized remote, and credential kind. The store contains decisions and key selection metadata only, never private key bytes or token values. Revocation is supported. Repository configuration can never grant host credential use: it may narrow policy, suggest a key, or define an acquisition command that needs its own exact-argv approval. Credential `allow` policy is limited to trusted global/task/explicit-flag sources; repository `allow` is ignored. Repository policy also cannot turn preflight off, relax global unresolved `fail`, expand the configured token environment-variable set, or disable globally enabled token verification. API clients cannot widen credential grants unless the global-only `workflows.prerequisites.allowApiClientGrants` setting enables that behavior.

Workflow prerequisite grants exclude `run_shell`, `run_script`, main agent steps,
`on_failure` remediation agents and dynamic/squad leaders. A single SSH key is
mounted read-only as `~/.ssh/awman_push_key`, with `IdentitiesOnly=yes` and a
writable private copy of `known_hosts`. Credentialed Git transport uses private,
controlled metadata, a pinned approved URL, no checkout hooks or URL rewrites,
and no checkout-selected helpers or SSH commands. HTTPS helpers answer only for
the approved host and path. New generated destinations require a fresh decision.

Host token tools are trusted installed programs outside Git checkouts. Known
agent identities (including canonical paths and content hashes of configured
agents), shells, interpreters and launch wrappers are refused. Review binds argv,
canonical executable and executable content; execution uses a verified private
snapshot. Changing installed content requires a new approval. This boundary
cannot infer arbitrary program semantics: approving an installed token tool also
trusts its interpreter, libraries and transitive behavior. Do not authorize a tool
that delegates to an agent. Output is bounded while running, errors withhold
untrusted diagnostics, and the process group is cleaned up on every exit. Tokens
stay in secret-typed launch overrides until the runtime child environment is
constructed; runs holding token grants use filtered phase streams, including
CLI/TUI, so raw terminal or attach output cannot precede redaction.

### WI 0144 test and release execution boundaries

No self-hosted runner executes repository workflows. All workflow jobs,
including every release asset build and verification, run on GitHub-hosted
runners, and `release.yml` uses no environment, release secret or local action.
Only its `produce` job can write attestations and OIDC tokens; its `validate`
job only reads. Release publication is performed solely by the maintainer's
authenticated `gh` login from `make release`, which creates the tag at the
commit every attestation names as its source. Branch protection, approval rules
and tag rulesets are optional repository settings; preflight reports the ones
this login cannot satisfy but they are not admission requirements.

Native guest, Apple Containers and Docker checks run on the operator's Apple
Silicon Mac and on a Linux host reached over SSH, from prebuilt kits whose
archives are attested and whose files are bound by a digest manifest. The kit is extracted only after
verification, into a fresh owned root, rejecting absolute or traversing paths,
links, devices, duplicates and oversized expansion; only allowlisted kit files
keep executable bits. Nothing is compiled, signed or rebuilt on a native host,
no GitHub token or other credential is sent to it, no checkout is required on
it, and any host work is tied to an owner lease so that only identified owned
descendants are ever terminated. Hosts allocate private short roots (mode 0700,
no-follow). Synthetic task agents run only inside the guest or container started
by the exact asset; every host mount lies within the check's private state, no
parent of the private test repository is mounted, and the controller checkout is
never mounted. Apple Containers and Docker objects are removed only by names,
tags and an owner label reserved for that run, never by pruning, and a leaked or
unverifiable object voids the result.

The release SBX smoke uses the operator's existing `sbx` login on the Mac and,
only when explicitly selected, on the Linux host. It never logs in or out, reads
no provider key or keychain, passes no secret to a sandbox and runs no agent or
provider task: a disposable shell sandbox on a private scratch workspace, then
preparation-only readiness through the exact asset with authentication mode
`none`, credential refresh disabled, private `AWMAN_HOME` and a private empty
repository. The Linux host receives a fixed script and the asset bytes over SSH
with agent forwarding disabled. Ownership is recorded before the first mutation
and cleanup removes only what that record names. It does not establish provider
authentication or effective agent policy.

The `HostAgentPinger` exception above is unchanged: the release command adds no
trigger and no second host-agent mechanism. Native and SBX results are operator
evidence, not Actions attestations; the build provenance of every asset is
an Actions attestation, and publication requires both. Native kill and
containment behavior on macOS and Windows must be validated on the real
platforms; a Linux-only fake does not certify it.

The release gate and the build evidence are verified by the operator's tool from
the authenticated service record, never from an uploader's summary: run,
attempt, job and artifact identities, the exact compiled test inventory and
passing JUnit must reconcile, and a skipped, missing, duplicated or short
result cannot satisfy a required cell. The release plan's publication policy
digest is re-checked before sealing and again before landing, tagging and
publishing; a change to publication-critical policy blocks rather than being
trusted. Evidence expires after at most 14 days. The accepted residual risks of
the narrower release gate are in `aspec/devops/release.md`.

The test supervisor rejects registered host keychain/clipboard/agent names before
checking ordinary executable allowlists, and still examines registered
interpreter arguments. Only explicit FakeCli installations receive the separate
fake provenance exception. This is a test discipline guard, not a transitive
execution sandbox: a permitted native executable can start other executables.
Linux session creation is constrained and escaped children retaining private
root environment values are detected, killed with PID creation-time checks and
reported as leaks. A hostile program that erases ownership data requires a
kernel containment boundary. macOS and Windows containment/kill recovery must be
validated on native runners before release cutover.
