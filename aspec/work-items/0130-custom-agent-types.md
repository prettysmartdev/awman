# Work Item: Feature

Title: custom agent types
Issue: issuelink

## Summary:
- This work item lets a user define their own agent type, which awman then "learns" to support.

For example, a user who wants to use the pi agent could create `.awman/agents/pi.toml` (in their repo or in global scope) containing everything awman needs to build, run and operate images for that agent or harness.

This should allow any agent to be supported by awman. Lesser-used agents such as crush and maki move out of the first-party support list and become example custom agents.

awman also gains a set of "built in" skills. They are hosted in the GH repo and installed through the skill library management that already exists. The first builtin skill is `create-custom-agent`, which teaches any code agent how to write a custom agent TOML file. A user can then ask their code agent, running inside awman, to add support for a new agent to awman.

**Decisions (confirmed with the maintainer):**

1. **Image definition: install steps plus an escape hatch.** A custom agent's TOML declares `[image] install = [...]` (plus optional `needs_node`, `apt` and `binary`). awman renders these into one hardened Dockerfile wrapper (root install → strict shell → `awman` user → `/workspace` → smoke test) and writes it to `.awman/Dockerfile.<name>`. Setting `image.dockerfile = "<file>"` uses a hand-written Dockerfile instead.
2. **First-party agents stay a Rust table but share one struct.** `AgentMatrix` (all `&'static`) becomes an owned `AgentDefinition`. The first-party table builds `AgentDefinition` values in Rust, and custom TOML files deserialize into a Layer 0 file schema that converts into the same `AgentDefinition`. Every consumer (option builders, overlays, sbx, launch policy) reads `AgentDefinition` and does not care where it came from. First-party-only strategies (Claude settings sanitising, keychain credentials) can't be selected from TOML. A custom agent's host ping is optional and comes only from its own `[ping]` table (§1, §6).
3. **Repo-scoped definitions need confirmation once per content hash.** Global definitions (`~/.awman/agents/`) are trusted, unless they declare a host `[ping]` (see the defaults below). A repo-scoped definition (`.awman/agents/`) must be reviewed and approved once per content hash before awman builds or launches it. The approval is recorded in `~/.awman/trusted-agents.json`. A host-path deny-list applies in every scope.
4. **The builtin skills library lives in the awman repo and is cloned sparse and shallow.** Skills live under `skills/` in `prettysmartdev/awman`. `awman new skill --pull builtin` maps to that repo and subdir. The skill-library clone path learns a sparse, shallow mode so only `skills/` is fetched; a full clone is about 300 MB because of `third_party/`.
5. **Container-class runtimes only.** Custom agents run on Docker, Apple Containers and builtin-experimental, which all consume the rendered Dockerfile. Docker Sandboxes (`docker-sbx-experimental`) support for custom agents is deferred; the design is kept in the [appendix](#appendix-deferred-docker-sandboxes-support-for-custom-agents).

**Defaults chosen in this work item (not separately confirmed; change them during review if needed):**

- **First-party names are reserved.** A custom file named after a first-party agent (`claude.toml`) is ignored with a warning and never shadows the built-in. If repo and global scope both define the same name, **repo wins**.
- **crush and maki are removed outright**, with no compatibility shim. They ship as example definitions (`awman new agent crush --from crush`). A config naming `crush`/`maki` with no definition fails with an error that names that command.
- **The host ping is opt-in per definition.** The `awman ready` host ping (the one sanctioned host-side agent execution, `src/engine/ready/host_agent.rs`) runs for a custom agent only when its TOML declares `[ping] command = [...]`. Without that table, `ready` **skips** the host check. A custom agent is never run on the host in any other way, and the credential refresh monitor never pings custom agents.
- **A definition with a host ping always needs approval, in any scope.** Global definitions without `[ping]` stay trusted as in decision 3. A definition that declares `[ping]`, whether repo or global, is approved once per content hash, and the approval prompt shows the exact host command.
- **A small `awman new agent <name>` scaffold command** is added (`--global`, `--from <example>`) so that creating a definition by hand has a CLI path that matches `awman new skill`.


## User Stories

### User Story 1:
As a: user

I want to:
drop a `pi.toml` into `.awman/agents/` (or `~/.awman/agents/`) describing how to install and invoke the pi agent, then run `awman ready --agent pi` and `awman chat --agent pi`

So I can:
use any code agent or harness with awman's containers, overlays, workflows and runtimes without waiting for awman to ship first-party support for it

### User Story 2:
As a: user running a code agent inside awman

I want to:
pull awman's builtin skills (`awman new skill --pull builtin`), mount `skill(awman/create-custom-agent)`, and ask my agent to "add support for the X agent to awman"

So I can:
have my agent research the X CLI and write a valid `.awman/agents/x.toml` for me, which I then review and approve with `awman ready --agent x`

### User Story 3:
As a: user who currently runs `crush` or `maki`

I want to:
get a clear error that tells me the exact command to recreate my agent as a custom definition (`awman new agent crush --from crush`), and keep my existing `.awman/Dockerfile.crush`

So I can:
keep using the agent with one command, while awman's first-party list stays small and well-maintained


## Implementation Details:

### 1. Custom agent TOML format (schema 1)

**Location and naming**

- Repo scope: `<git_root>/.awman/agents/<name>.toml`. Global scope: `<data_home>/agents/<name>.toml`, where `data_home` is `GlobalConfig::data_home_with(env)` (so it respects `AWMAN_CONFIG_HOME`).
- **The file stem is the agent name.** There is no `name` key, which removes a whole class of mismatch errors. The stem must match `^[a-z0-9][a-z0-9_-]{0,63}$`. This is stricter than `AgentName` (lowercase only) because the name ends up in image tags (`agent_image_tag`), and Docker tags must be lowercase. A file whose stem breaks this rule is skipped with a warning that names the file.
- `#[serde(deny_unknown_fields)]` applies to every table, so typos fail loudly with the toml crate's line/column span.
- Whenever a definition is loaded, the error output shows the file path, its scope (repo/global) and the toml span.

**Reference example (illustrative: verify pi's real flags against `pi --help` while implementing):**

```toml
# .awman/agents/pi.toml
schema = 1                                   # required; > supported → "requires a newer awman"
display_name = "Pi"                          # required; shown in ready/status/TUI
description = "Pi coding agent"              # optional
homepage = "https://github.com/badlogic/pi-mono"  # optional; surfaced in errors and the trust prompt

[run]
command = ["pi"]                  # required; argv of the interactive entrypoint (no shell)
non_interactive = "-p"            # optional; token appended for -n / exec / workflow steps
interactive_only_flags = []       # optional; appended only for interactive launches
seed_prompt = "positional"        # "positional" (default) | { flag = "--prompt" }
model = "space"                   # "space" (--model NAME, default) | "equals" (--model=NAME) | "unsupported"
env = { PI_TELEMETRY = "0" }      # optional; literal env always set in the container

[modes]                           # every key optional; an absent key means the mode is a silent no-op (same as today)
yolo = ["--yolo"]
auto = []
plan = []                         # an empty/absent plan means --plan is rejected with PlanModeUnsupported

[tools]                           # optional
allowed_flag = "--allowed-tools"
disallowed_flag = "--disallowed-tools"

[system_prompt]                   # optional; default delivery = "none"
delivery = "append-file"          # "append-file" | "append-inline" | "replace" | "agents-md" | "env-file" | "add-dir" | "none"
flag = "--append-system-prompt"   # required for append-file / append-inline / replace / add-dir
# key = "developer_instructions"  # required for append-inline
# env = "PI_SYSTEM_MD"            # required for env-file

[acp]                             # optional; presence ⇒ supports_acp = true
command = ["pi", "--acp"]

[ping]                            # optional; absent ⇒ `awman ready` skips the host check for this agent
command = ["pi", "-p"]            # argv run ON THE HOST by `awman ready`; awman appends one fixed greeting as the last argument

[settings]                        # optional
host_dir = ".pi"                  # relative to $HOME; mounted rw at <container_home>/.pi when it exists on the host
skills_dir = ".pi/agent/skills"   # relative to container $HOME; absent ⇒ skills overlay skipped with the existing warning

[auth]                            # optional
env = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]  # passed through from the host when set; unset vars are skipped silently

[image]                           # required
needs_node = true                 # optional; renders the nodesource Node 20 block used by today's npm-based agents
apt = ["ripgrep"]                 # optional; extra Debian packages
install = [                       # required unless `dockerfile` is set; one RUN per entry, run as root
  "npm install -g --prefix=/usr/local @mariozechner/pi-coding-agent",
]
binary = "pi"                     # optional; smoke-tested binary, default = run.command[0]
# dockerfile = "pi.Dockerfile"    # escape hatch; relative to this TOML's directory; excludes needs_node/apt/install
```

**Validation rules** live in one function, `AgentDefinitionFile::validate`, which returns every violation rather than only the first:

| Field | Rule |
|---|---|
| `schema` | Must be `1`. A higher value fails with `"<path>: schema N requires a newer awman (this build supports 1)"`. |
| argv tokens (`run.command`, flags, `acp.command`) | Must be non-empty, contain no NUL or newline characters, and not exceed 1024 bytes. `run.command[0]` must be a bare binary name or an absolute container path, with no `/` other than as an absolute prefix. Nothing is ever passed through a shell. |
| `ping.command` | Optional. When present it must be non-empty and follows the argv token rules. `command[0]` must be a bare binary name (resolved on the host `PATH`) or an absolute host path; relative paths such as `./pi` or `bin/pi` are rejected, because the ping runs in a temporary directory and must never pick up a repo-planted file. At most 16 tokens. |
| `system_prompt` | Each delivery requires the keys shown above, and unrelated keys are rejected. |
| `run.env` keys | Must be POSIX env names. `HOME`, `PATH`, `USER`, `AWMAN_*` and `LD_*` are rejected. |
| `auth.env` | Must be POSIX env names. `AWMAN_*` and `SSH_AUTH_SOCK` are rejected. |
| `settings.host_dir` | Must be relative and normalised: no `..`, no leading `/`, not empty and not `.`. It must not be on or under the **deny-list**: `.ssh`, `.gnupg`, `.aws`, `.azure`, `.config/gcloud`, `.kube`, `.docker`, `.awman`, `.netrc`, `.git-credentials`, `.config/gh`, `.password-store`, `.local/share/keyrings`, `Library/Keychains`. |
| `settings.skills_dir` | Must be relative with no `..`. |
| `image.install` entries | Must be non-empty, single-line (no `\n`) and at most 4096 bytes. Multi-step installs use multiple entries, because heredoc `RUN` would require BuildKit, which the Apple and builtin builders do not guarantee. |
| `image.apt` | Each entry must match `^[a-z0-9][a-z0-9+.-]*$`. |
| `image.dockerfile` | Must be a relative path inside the TOML's own directory (no `..`, no symlink escaping it). It is mutually exclusive with `install`, `apt` and `needs_node`. If neither `install` nor `dockerfile` is set, the file is invalid. |
| `sandbox` | Not accepted in schema 1. A top-level `[sandbox]` table fails with `"<path>: custom agents do not support Docker Sandboxes yet; remove [sandbox] and use docker, apple or builtin-experimental"` rather than the generic unknown-field error, and the key stays reserved for the deferred design. |

### 2. One shared type: `AgentDefinition`

**Module layout.** This replaces the single file `src/engine/agent/agent_matrix.rs`:

```
src/data/config/agent_definition.rs   // Layer 0: AgentDefinitionFile (serde, deny_unknown_fields), validate(), content hash
src/data/fs/agent_dirs.rs             // Layer 0: AgentDirs { repo_dir(), global_dir(), discover() }, like SkillDirs
src/data/fs/agent_trust.rs            // Layer 0: TrustStore read/write (~/.awman/trusted-agents.json)
src/engine/agent/definition.rs        // AgentDefinition + the enums (moved out of agent_matrix.rs), validate_run/mode_flags/entrypoint helpers
src/engine/agent/first_party.rs       // the Rust table: FIRST_PARTY_AGENTS + first_party(name) -> Option<AgentDefinition>
src/engine/agent/custom.rs            // AgentDefinitionFile -> AgentDefinition conversion; Dockerfile rendering
src/engine/agent/registry.rs          // AgentRegistry: resolve(name, git_root) -> ResolvedAgent; list(git_root)
```

`agent_matrix.rs` is deleted rather than kept as a re-export shim. `engine/agent/mod.rs`'s module doc ("All agent-name branching lives in `agent_matrix.rs`") is updated to point to `first_party.rs`.

**`AgentDefinition`** keeps the current `AgentMatrix` fields and semantics with these changes:

- Every `&'static str` / `&'static [&'static str]` / `Vec<&'static str>` field becomes `String` / `Vec<String>`. `SystemPromptMode::AppendInline { key }`, `EnvFile { var }` and `AddDir { flag }` carry `String`. `SettingsMount::Direct(String)`. `static_env: Vec<(String, String)>`.
- `yolo_flag: Option<&str>` becomes `yolo_flags: Vec<String>`, which matches `auto`/`plan` and the TOML's arrays (an empty vec is the old `None`).
- New `origin: AgentOrigin`:
  ```rust
  pub enum AgentOrigin {
      FirstParty,
      Custom { scope: DefinitionScope /* Repo | Global */, path: PathBuf, sha256: String },
  }
  ```
- New `display_name: String` and `homepage: Option<String>` (first-party values come from the table).
- New `image: AgentImageSource`:
  ```rust
  pub enum AgentImageSource {
      Download,                                 // first-party: GitHub raw URL + bundled fallback (today's download.rs)
      Rendered { spec: ImageSpec },             // custom install steps
      UserDockerfile { path: PathBuf },         // custom escape hatch
  }
  ```
- First-party agents keep their embedded sbx kit templates; `SbxKitKind` stays as it is. There is no custom sandbox field (§4).
- The `agent` field is renamed `name: String`.

**Fields that TOML can't set.** `AgentDefinitionFile → AgentDefinition` hardcodes these for every custom agent, which keeps the first-party special cases out of reach of untrusted input:

| Field | Custom value |
|---|---|
| `settings_mount` | `Direct(host_dir)` or `None`, never `Claude` or `Antigravity` |
| `credential_source` | `None` |
| `supports_stdin_injection` | `false` |
| sandbox fields (`sbx_kit_kind`, `sandbox_permission_mode_supported`, `sandbox_auth_env_vars`, `sandbox_model_note`) | neutral defaults (`Agent`, `false`, empty, `None`). They are never read, because custom agents are refused under sandbox-class runtimes before any sbx code runs (§4). |
| `deprecation_note` | `None` |

**`ping_argv` is no longer universal.** It becomes `Option<Vec<String>>`: `Some` for every first-party agent (unchanged values), and `ping.command` or `None` for a custom agent. `AgentDefinition::ping_command` returns `Option<(String, Vec<String>)>`. Remove the `.expect("every agent declares a ping binary")`. Custom pings get no cheapest-model pinning; a definition that wants one puts the model flag in `ping.command`.

**Lookups:**

```rust
// registry.rs
pub struct ResolvedAgent(pub Arc<AgentDefinition>);

impl AgentRegistry {
    /// First-party → repo `.awman/agents/<name>.toml` → global `~/.awman/agents/<name>.toml`.
    /// Reads at most two small files; no caching. Callers resolve once per launch and pass the Arc down.
    pub fn resolve(env: &EnvSnapshot, name: &AgentName, git_root: Option<&Path>) -> Result<ResolvedAgent, EngineError>;
    /// Every agent available for this repo, first-party first, for `unknown agent` errors, `config set agent`, and `new agent`.
    pub fn list(env: &EnvSnapshot, git_root: Option<&Path>) -> AgentListing; // { first_party, custom: Vec<(name, scope, path)>, skipped: Vec<(path, reason)> }
}
```

- `matrix_for(name)` is removed. Callers that only care about first-party behaviour (`engine::auth::keychain::credential_source`, `refreshable_spec_for`) call `first_party::first_party(name)` and treat `None` as `CredentialSource::None`, which is exactly what their `_ =>` fallbacks already do. They gain no new I/O.
- Every other current `matrix_for` caller moves to `AgentRegistry::resolve` with the session's git root. Today these are `AgentEngine::build_options_with_credentials` / `build_sandbox_options` (`agent/mod.rs:371, 656`), `launch_policy.rs` (100, 126, 249), `workflow_preflight.rs:61`, `overlay/skills.rs:41`, `overlay/agent_settings.rs:61`, `sandbox/options.rs:138`, `sandbox/dsbx/backend.rs:110, 428`, `sandbox/dsbx/auth.rs:35` and `ready/host_agent.rs:158`. Inside `AgentEngine`, resolve **once** at the top of each option builder and pass `&AgentDefinition` to the helpers. Don't re-resolve per helper, because a file edited mid-launch must not produce a mixed definition.
- The unknown-agent error becomes `EngineError::UnknownAgent { name, first_party: Vec<String>, custom: Vec<String> }` and renders as:
  `unknown agent 'foo'; first-party: claude, codex, …; custom: pi (repo); define one with: awman new agent foo`
  If the name is `crush` or `maki`, a dedicated line is appended: `'crush' is no longer built in — recreate it with: awman new agent crush --from crush`. When the active runtime is sandbox-class, it adds: `(custom agents run on docker, apple or builtin-experimental; Docker Sandboxes support is not available for them yet)`.
- `SUPPORTED_AGENTS` is renamed `FIRST_PARTY_AGENTS` and loses `crush` and `maki`. The test loops over it in `matrix_tests.rs`, `agent_settings.rs`, `sandbox/options.rs` and `launch_policy.rs:306` keep working against the 7 first-party entries.
- The `self.agent != "claude"` check in `validate_tool_restrictions` stays name-based. That is safe because first-party names are reserved, but it gets a comment saying so.

**Remaining name-keyed tables to audit**, so that an unknown name degrades instead of misbehaving: `src/data/fs/auth_paths.rs` (`resolve(agent)`), `src/engine/auth/mod.rs`, `src/data/image_tags.rs`, `src/data/ready_summary.rs`, and squad evaluation's agent pool. Each must take the `_ =>` path for custom names. Add one test per table that uses a custom name.

### 3. Discovery and precedence (`src/data/fs/agent_dirs.rs`)

- `AgentDirs::discover()` lists `*.toml` in the repo dir and the global dir. Non-`.toml` files are ignored, which leaves room for escape-hatch Dockerfiles next to them. Entries that are skipped (bad stem, reserved name, symlink pointing outside the dir, unreadable file) are returned with a reason, not dropped silently.
- Precedence when resolving a name: **first-party, then repo, then global**. A reserved-name file is listed in `skipped` with `"'claude' is a first-party agent; custom definitions cannot replace it"`. `awman ready` prints `skipped` entries as warnings.
- Parse errors are **not** skipped when the name is requested. `awman chat --agent pi` with a broken `pi.toml` fails with the full validation report. It never falls through to a global `pi.toml`, because silently picking a different definition is worse than failing.
- Definitions are re-read on every resolve. There is no cache and no file watching.

### 4. Image / Dockerfile handling for custom agents

**Rendering (`custom.rs`, template `templates/Dockerfile.custom`).** Add one embedded template that is the common skeleton of today's `Dockerfile.crush`/`Dockerfile.maki`: `FROM {{AWMAN_BASE_IMAGE}}` → `USER root` → env reset → strict `SHELL` → `ENTRYPOINT []` → `{{AWMAN_NODE_BLOCK}}` → `{{AWMAN_APT_BLOCK}}` → `{{AWMAN_INSTALL_BLOCK}}` (one `RUN <step>` per entry) → idempotent `awman` user/workspace block → `USER awman` → `RUN test -x "$(command -v {{AWMAN_BINARY}})"`. The first line is a header:

```
# awman-generated from <scope>:<path> — definition sha256: <hash>
# Edit the agent definition and re-run `awman ready`; do not edit this file.
```

- The content hash is sha256 over the TOML bytes plus, for the escape hatch, the referenced Dockerfile's bytes. This hash is also the trust key (§6).
- The escape hatch copies the user's Dockerfile into `.awman/Dockerfile.<name>` with `{{AWMAN_BASE_IMAGE}}` substituted and the same header prepended. Copying is needed because the build context is the git root, and global-scope Dockerfiles live outside it. If the file has no `FROM {{AWMAN_BASE_IMAGE}}`, awman warns (the project image won't be layered underneath) but doesn't fail.
- The output always lands at `RepoDockerfilePaths::agent_dockerfile(name)`, so every existing `discover_agent_dockerfiles` consumer (workflow preflight, dynamic workflows, squad gateway/evaluation, image prep, ready's non-default agents) sees custom agents with no changes.

**Sync rule.** One function replaces each direct `download_agent_dockerfile` call:

```rust
/// engine/agent/mod.rs
pub async fn provision_agent_dockerfile(def: &AgentDefinition, paths: &RepoDockerfilePaths, project_tag: &str)
    -> Result<DockerfileSync, EngineError>;
pub enum DockerfileSync { Unchanged, Written, UserOwned }
```

| `.awman/Dockerfile.<name>` state | First-party (`Download`) | Custom |
|---|---|---|
| absent | download, falling back to the bundled copy (today's behaviour) → `Written` | render/copy → `Written` |
| present, awman header, hash matches | `Unchanged` | `Unchanged` |
| present, awman header, hash differs | n/a | re-render → `Written` |
| present, **no** awman header | `Unchanged` (today's behaviour) | `UserOwned`: used as-is, with a one-line note ("using hand-maintained .awman/Dockerfile.pi; delete it to regenerate from pi.toml") |

The `UserOwned` row is what keeps existing crush/maki users' `.awman/Dockerfile.crush` working after migration.

The call sites to switch are `agent/mod.rs:262` (`ensure_available`), `image_prep/helpers.rs:18`, `init/mod.rs:252`, `ready/phases.rs:253` and `squad/launcher.rs:171`. The squad call (`ensure_directory_workspace_project`) currently uses `agent_dockerfile_for` directly; it renders custom definitions through the same function, using the workspace root as `git_root`.

**Rebuilds.** In `awman ready`, `DockerfileSync::Written` forces `needs_build` for that agent. In `chat`/`exec`/workflow launches (`ensure_available`), `Written` with an existing image emits a warning: `agent definition 'pi' changed since its image was built; run 'awman ready --agent pi --build'`. It does not rebuild implicitly, so a launch never runs a surprise long build. The exception is the case where the image is absent and the `BuildPolicy` already allows a build.

**Every runtime works without runtime-specific code.** Docker, Apple Containers and builtin-experimental (via `AgentImagePreparer` and its builder or import sources) all consume a Dockerfile path plus `agent_image_tag`, so custom agents need nothing new from them. The Layer 1 `BuildPolicy` rules in `security.md` apply unchanged.

**Docker Sandboxes (sbx): not supported for custom agents.**

- First-party agents keep their embedded `sbx-kit.<agent>.yaml` / `sbx-apply.<agent>.sh`, and the sbx code path is unchanged for them. `DSbxKitEmitter::emit_for_agent` keeps taking the agent name.
- A custom agent under a sandbox-class runtime is refused with `EngineError::CustomAgentRuntimeUnsupported { name, runtime }`, which renders as `custom agent 'pi' runs only on container runtimes (docker, apple, builtin-experimental); 'docker-sbx-experimental' is not supported for custom agents yet`. It maps to the same exit class as other unsupported-runtime errors in `aspec/uxui/cli.md`.
- **Enforcement point:** one check in `AgentEngine`, at the top of the option builders, on the same `runtime.capabilities()` branch that already chooses the sandbox paradigm (F-40b). The check runs before any kit emission, Dockerfile render or image work. `ready`, `chat`, `exec prompt`, `exec workflow` (during `workflow_preflight`, per agent) and squad all reach it, so none of them has its own copy. `awman ready` under sbx reports it as a failed step.
- The trust check runs first, so an untrusted definition still gets the trust error rather than the runtime error.

**Removed files:** `templates/Dockerfile.crush`, `Dockerfile.maki`, `sbx-kit.crush.yaml`, `sbx-kit.maki.yaml`, `sbx-apply.crush.sh` and `sbx-apply.maki.sh`, along with their arms in `src/data/templates/mod.rs`. Older awman binaries that try to download `Dockerfile.crush` from `main` get a 404 and fall back to their bundled copy, so removing the upstream file doesn't break them. Removing the crush/maki kits means **crush and maki lose sbx support** until the deferred design lands; this is accepted for now (see §8 and the appendix).

### 5. Launch-time behaviour for custom agents

- **Settings overlay:** `SettingsMount::Direct(host_dir)` goes through the existing `Direct` path in `OverlayEngine::agent_settings_overlays_*` (read-write, only when the host directory exists). The deny-list is re-checked here as well as at parse time, as defence in depth (the same pattern as `validate_skill_reference_segment`).
- **Skills overlay:** `skills_dir` maps to the existing `skills_mount`.
- **Auth env:** each `auth.env` var set on the host becomes a `ContainerOption::EnvPassthrough`, masked in displayed commands exactly as `env()` overlays are. A user's explicit `env()` overlay for the same var is de-duplicated, with the overlay taking precedence.
- **Static env:** `run.env` maps to `static_env`.
- **`agentFlags` and `--agent-flag`** already key on agent name and work unchanged. The same is true of `--model`, `--yolo`/`--auto`/`--plan`, `-n`, `--acp`, system prompt delivery and context overlays, because they all read `AgentDefinition`.
- **Container home:** `detect_container_home` already reads `USER` from `.awman/Dockerfile.<name>`. The rendered template always yields `/home/awman`.

### 6. Security

- **Host execution:**
  - `HostAgentPinger::ping_command` takes the resolved `&AgentDefinition` and returns `AgentDefinition::ping_command(greeting)`. Remove the pre-F-32 `<name> --print <greeting>` catch-all: an unknown name now fails with `UnknownAgent` at resolve time, before `ready` reaches the ping, so no argv is ever invented for an agent.
  - In `ReadyPhase::CheckingLocalAgent`, `None` reports `StepStatus::Skipped("no [ping] command in <path>; host check skipped")`, sets `summary.local_agent = Skipped`, and spawns nothing.
  - A custom `Some` ping runs through the same sanctioned path as first-party pings, with the same constraints: argv only (no shell), the prompt is one entry from the hardcoded `GREETINGS` table appended as the last argument, and the process runs in a fresh empty `0700` temporary directory outside the repository. `run.env`, `auth.env` and every other TOML field are **not** applied to the host process.
  - Results are reported exactly as for a first-party agent (not installed, not authenticated, ok).
  - The ping runs only from `awman ready`. `CredentialRefreshMonitor` never registers custom agents, because `refreshable_spec_for` is `None` for them.
  - Update the module doc in `host_agent.rs` and the `SECURITY EXCEPTION` comment in `ready/phases.rs`: the exception covers first-party pings and the `[ping]` command of an **approved** custom definition, nothing else.
- **Trust store (`src/data/fs/agent_trust.rs`):** `~/.awman/trusted-agents.json` is written atomically with mode `0600`:
  ```json
  { "version": 1, "entries": [
    { "scope": "repo", "git_root": "/abs/canonical/repo", "name": "pi", "sha256": "…", "approved_at": "RFC3339" },
    { "scope": "global", "path": "/abs/canonical/.awman/agents/pi.toml", "name": "pi", "sha256": "…", "approved_at": "RFC3339" }
  ] }
  ```
  - **Repo-scope** definitions are always checked.
  - **Global-scope** definitions are checked only when they declare `[ping]`, because that is the one field that runs on the host. A global definition without `[ping]` is user-authored, outside any cloned repo, and needs no approval.
  - The repo key is `(canonical git_root, name, sha256)`. The global key is `(canonical path, sha256)`, approved once for every repo.
  - Any edit to the TOML (including its `[ping]`), or to its escape-hatch Dockerfile, changes the hash and requires a new approval. The old entry for the same repo/name or global path is replaced.
- **Enforcement points:**
  - **Engine (fail-closed):** `AgentRegistry::resolve` returns `EngineError::AgentDefinitionUntrusted { name, path, sha256 }` for an unapproved definition that needs approval (any repo definition, or a global one with `[ping]`). Commands that only *list* agents call `AgentRegistry::list`, which doesn't enforce trust. This guarantees that no build, launch, kit emission or Dockerfile render happens for an untrusted definition, however the request arrives (CLI, TUI, API, squad daemon, workflows).
  - **Command layer (prompt):** a new `launch_policy::ensure_agent_trusted(frontend, …)` runs before the resolve in `ready`, `chat`, `exec prompt`, `exec workflow` (once per distinct agent across all steps, during `workflow_preflight`) and `new agent`. On a CLI or TUI with a user present, it shows a review summary and asks for confirmation through the existing confirmation dialog trait. On a yes, it writes the trust entry and continues. On a no, it exits with code 1 and the message `agent definition 'pi' was not approved`.
  - **Non-interactive contexts** (the API, the squad daemon, `-n` with no TTY, `--yes`-less headless) never prompt. They fail with `agent definition 'pi' (<path>) has not been approved for this repo; run 'awman ready --agent pi' interactively to review it`. `--yes` does **not** imply trust: approving code that runs builds must be an explicit, per-definition act.
- **Review summary contents** (one renderer, shared by CLI and TUI, in the command-layer outcome type):
  - **The host command, first and set apart**, when `[ping]` is present:
    ```
    ⚠ `awman ready` will run this command ON YOUR HOST (not in a container):
        pi -p '<greeting>'
      from an empty temporary directory outside the repo, where <greeting> is one of
      awman's built-in greetings (e.g. 'Hello'). Check that this is the agent you expect.
    ```
    The argv is rendered shell-quoted, token by token, exactly as it will be passed (it is never run through a shell). When `command[0]` is a bare name, the line below it shows the path it resolves to on the host `PATH` right now, or "not found on PATH". When `[ping]` is absent, the summary says "no host command: `awman ready` will skip the host check".
  - display name, scope and path
  - homepage
  - each `image.install` step verbatim, or the escape-hatch Dockerfile path and line count
  - `settings.host_dir`, flagged ⚠ when it is another first-party agent's settings dir (e.g. `.claude`, `.codex`)
  - `auth.env` names (never values)
  - `run.env` keys
- **`awman new agent` auto-approves** the content it writes, since the user ran it. When that content has a `[ping]` (for example `--from crush`), its success output prints the same host-command block as the review summary, so the user still sees what will run on their host. Later edits need re-approval as usual.
- **The only host execution is the approved `[ping]`.** Install steps run only inside the image build (the same trust boundary as `Dockerfile.dev` and today's agent Dockerfiles). No TOML field is ever evaluated by a host shell, and all argv fields, `ping.command` included, are passed as argv.
- Add a short "Custom agent definitions" section to `aspec/architecture/security.md` stating: a custom agent runs on the host only through its optional, approved `[ping]` command during `awman ready`; repo-scope definitions, and global ones with `[ping]`, need per-hash approval that shows the host command; host-path deny-list; and argv-only semantics. This is the only spec edit, and it belongs to the implementation, not to this planning step.

### 7. CLI and catalogue changes

- **`init --agent`** (`catalogue/core.rs:77`) changes from `FlagKind::Enum(AGENT_VALUES)` to `FlagKind::String`. Its help text lists the first-party names and says "or the name of a custom agent defined in .awman/agents/ or ~/.awman/agents/". `AGENT_VALUES` is deleted, and `init` validates the value through `AgentRegistry::list`. Check `projections/tui_hints.rs`: if completion depended on the Enum values, feed it `FIRST_PARTY_AGENTS` plus the discovered custom names at hint time, or fall back to first-party only.
- **`config set agent` and the `agentsToModels`/`agentFlags` keys** (`config.rs:96`): delete `VALID_AGENT_VALUES`.
  - Validate `agent` against `AgentRegistry::list` for the scope being written. Repo writes accept first-party, repo-custom and global-custom names; global writes accept first-party and global-custom names.
  - `agentFlags` and `agentsToModels` keys keep the existing shape-only validation (`data/config/repo.rs:289`), because a key for an agent defined later is harmless.
  - The hint at `config.rs:429` uses the listing.
- **New `awman new agent <name>`** (`catalogue/new.rs`, `NEW_AGENT` registered in `NEW.subcommands`; `src/command/commands/new/agent.rs`):
  - Positional `name` (required, validated with the stem regex and reserved names).
  - `--global` writes to `~/.awman/agents/` instead of the repo.
  - `--from <example>`, with `FlagKind::Enum(&["crush", "maki"])`, copies the embedded example instead of the commented skeleton (`templates/agent-definition.toml`, which documents every key).
  - `--force` overwrites an existing file. Without it, an existing file is an error that names the path.
  - `api_allowed: false` and `requires_runtime: false`. It writes files only and never builds.
  - On success it prints the path and the next step, `awman ready --agent <name>`. It writes the trust entry for repo scope.
  - Update `docs/14-command-reference.md` from the markdown projection (`COMMITTED_REFERENCE`).
- **Tips (`agents/tips.rs`):** add one tip for `` `awman new agent <name>` `` and one for `` `awman new skill --pull builtin` ``. These are covered by the catalogue tip tests and must stay ≤100 chars.
- **`awman ready`** adds a step `AgentDefinition` (custom only): "loaded from <scope>:<path>", trusted ✓ / not approved ✗ / approval not needed. It also prints the `skipped` warnings from discovery. The existing local-agent step reports `Skipped` with "no [ping] command in <path>" when the definition has no `[ping]`.

### 8. crush / maki migration

- Remove both from `FIRST_PARTY_AGENTS`, the first-party table, the templates and every per-agent test fixture that names them. Tests that used crush/maki as the representative `kind: agent`, `Unsupported`-delivery or no-settings agent (for example `dsbx/backend.rs:1019–1361`, `agent/mod.rs:1333, 2074, 2226, 3305`, `dsbx/kit.rs:298–561` and `dsbx/auth.rs:597, 703`) are re-pointed to `cline`/`antigravity` where a first-party agent with the same property exists. Container-paradigm tests with no first-party equivalent are rewritten against a **custom fixture definition** built from the crush example, which doubles as coverage that custom agents behave like the agents they replaced.
- **The sbx `kind: agent` path loses its only users.** crush and maki are the only `kind: agent` kits today; every remaining first-party kit is `kind: mixin`. Keep `SbxKitKind::Agent` and its emitter branch, because the deferred design (appendix) reuses them. Its tests (`dsbx/kit.rs`, `dsbx/backend.rs`, `dsbx/auth.rs`) switch to a test-only `AgentDefinition` with `sbx_kit_kind = Agent` and an inline kit template, so the branch stays covered without shipping a crush/maki kit.
- Example definitions live at `skills/create-custom-agent/examples/crush.toml` and `maki.toml`. These are the single source: `awman new agent --from` embeds them with `include_str!("../../../../skills/create-custom-agent/examples/<x>.toml")`.
  - `crush.toml` uses **install steps**: `needs_node`, `npm install -g --prefix=/usr/local @charmland/crush`, `non_interactive = "run"`, `yolo = ["--yolo"]`, `delivery = "none"`, `host_dir = ".config/crush"`, `skills_dir = ".config/crush/commands"`, `auth.env` = the provider vars documented today in `docs/03-agent-sessions.md` "Crush authentication", and `[ping] command = ["crush", "run"]` (today's ping argv). It has no `[sandbox]` table; the current `sbx-kit.crush.yaml` values are recorded in the appendix for when sbx support returns.
  - `maki.toml` uses the **escape hatch** (`dockerfile = "maki.Dockerfile"`, a copy of today's `Dockerfile.maki` with the trailing awman-user block intact), so the two examples together show both modes. Its fields are `yolo = ["--yolo"]`, no `non_interactive`, `delivery = "none"`, `[ping] command = ["maki", "--print"]` (today's ping argv) and no settings.
  - A test asserts that each example parses, validates, and converts to an `AgentDefinition` whose launch-relevant fields equal the old matrix entries: entrypoint, non-interactive token, mode flags, model delivery, system prompt delivery, settings and skills mounts, and ping argv. This proves the migration is behaviour-preserving.
- Persisted config/session values naming `crush`/`maki` keep deserializing, because `AgentName` is free-form. They fail only at resolve time, with the migration hint from §2.

### 9. Builtin skills library and the `create-custom-agent` skill

**Repo layout (new top-level `skills/`):**

```
skills/
  README.md                                   // what the builtin library is; how to pull it
  create-custom-agent/
    SKILL.md                                  // frontmatter + procedure
    reference.md                              // full schema-1 field reference + validation table (mirrors docs/15-custom-agents.md)
    examples/crush.toml                       // install-steps example (also embedded in the binary)
    examples/maki.toml + examples/maki.Dockerfile   // escape-hatch example
```

**Pulling it:**

- In `resolve_pull_target` (`src/command/commands/skill_library.rs`), the inputs `builtin` and `awman` are special-cased *before* the `ExistingByName` arm. They map to `PullTarget::Slug(prettysmartdev/awman)` with `subdir = "skills"` and `sparse = true`, so `awman new skill --pull builtin` works on first use and on every refresh.
- The library slug is `awman`, so overlays are written `skill(awman)` / `skill(awman/create-custom-agent)`.
- A user who already pulled an unrelated `*/awman` repo hits the existing owner/repo mismatch error, which is correct.

**Sparse, shallow clone:**

- `SkillLibraryMeta` gains `#[serde(default)] sparse: bool`, so existing `.awman.json` files keep working.
- Add `GitEngine::clone_repo_sparse(url, dest, subdir)` (plus a `_logged` variant, matching every other `GitEngine` method). It runs `git clone --depth 1 --filter=blob:none --sparse <url> <dest>`, then `git -C <dest> sparse-checkout set <subdir>`.
- `pull_latest` with `sparse` uses `git fetch --depth 1 origin` before the existing `reset --hard origin/HEAD`.
- Only the builtin target sets `sparse` in this work item. A `--sparse` user flag is out of scope.
- `--pull-all` refreshes the builtin library like any other.

**Version skew.** The library tracks `main`, so a skill can describe a newer schema than an older binary supports. `SKILL.md` tells the agent to write `schema = 1` explicitly, and `reference.md` is headed with the schema version it documents. A newer schema is rejected with the clear "requires a newer awman" message (§1).

**`SKILL.md` content outline** (the frontmatter `description` should trigger on "add support for <agent> to awman", "create an awman custom agent", "awman agent toml"):

1. **Context:** you are running inside an awman container. The repo is at `/workspace`. You cannot run `awman`, and you can't write to the user's `~/.awman`. Write the file to `/workspace/.awman/agents/<name>.toml`.
2. **Research the agent CLI.** If it can be installed in the current container, use `<bin> --help` and its subcommands; otherwise use its docs/README. Fill in this checklist:
   - install method (npm, curl script, binary release)
   - binary name
   - interactive entrypoint
   - non-interactive/print mode
   - how an initial prompt is passed
   - yolo/auto/plan flags
   - model flag form
   - system-prompt mechanism (maps to one `delivery`)
   - host config dir
   - skills/commands dir
   - auth env vars
   - ACP support
3. **Pick a mode:** install steps (default) or a Dockerfile escape hatch (multi-stage builds, unusual base needs). Include the base-image placeholder rule.
4. **Write the TOML** using `reference.md` and the closest example. Leave `[ping]` out unless the user asks for a host check: it is the one command that runs on the user's host, it is optional, and without it `awman ready` simply skips the host check. If the user does want one, use the agent's cheapest one-shot print mode (e.g. `["pi", "-p"]`), and tell the user that the approval prompt will show it.
5. **Self-validate** against the validation table: stem regex, single-line install steps, deny-listed host dirs, required keys per `delivery`, no shell metacharacter reliance in argv fields, and `schema = 1`.
6. **Hand off:** tell the user to run `awman ready --agent <name>` on the host. That step reviews and approves the definition and builds the image. They can then run `awman chat --agent <name>`, and move the file to `~/.awman/agents/` to use it in every repo. Never claim the image was built or tested.

### 10. API frontend parity

- No new API routes are needed. Launch routes resolve agents through the same registry and therefore get the same `UnknownAgent` / `AgentDefinitionUntrusted` errors, mapped to exit/HTTP classes: unknown maps to exit 4 / 404-class, untrusted to exit 1 / 403-class, following the existing error-class mapping in `aspec/uxui/cli.md`.
- `new agent` is `api_allowed: false`, like the rest of the `new` subtree.
- `GET`-style config/agent listings that currently enumerate `SUPPORTED_AGENTS` (if any; check `projections/api_schema.rs`) switch to `AgentRegistry::list`.


## Edge Case Considerations:

- **Same name in repo and global scope:** repo wins. `awman ready` prints which file was used. An invalid repo file does **not** fall back to the global one (§3).
- **Custom file named after a first-party agent** (`claude.toml`): skipped with a warning and never shadows the built-in. `new agent claude` is rejected.
- **Custom file named `crush.toml` or `maki.toml`:** allowed and is the intended migration path. Any pre-existing `.awman/Dockerfile.crush` without the awman header is `UserOwned` and is used as-is.
- **Definition edited while a container is running:** the running container is unaffected. The next launch sees the new hash, which needs re-approval in repo scope and triggers a re-render plus the stale-image warning.
- **Definition deleted while its rendered `.awman/Dockerfile.<name>` remains:** `discover_agent_dockerfiles` still lists the agent, so resolving it fails with `UnknownAgent`. Workflow preflight and squad must surface that error per agent instead of treating the agent as available. Add a test for that.
- **Rendered Dockerfile hand-edited by the user, header intact:** the hash still matches the TOML, so the file stays `Unchanged` and the edits survive until the TOML changes and it is re-rendered. Document this: "remove the header line to take ownership".
- **Global definition used in many repos:** each repo renders its own `.awman/Dockerfile.<name>` and image (images are per-repo, layered on that repo's project image). There is no cross-repo sharing.
- **The trust store** sits in `~/.awman`, which is already deny-listed for mounts, so an agent inside a container can't approve its own definition. A repo moved to a new path needs re-approval, because the key uses the canonical root. A corrupt or unreadable trust store is treated as empty (re-prompt) with a warning, never as "all trusted".
- **Two awman processes approving at once:** writes are atomic (tmp + rename). Last writer wins, which loses at most one approval and causes a re-prompt, never a false trust.
- **Escape-hatch Dockerfile is a symlink** or resolves outside the TOML's directory: rejected (same rule as `ensure_not_symlinked` in skill libraries). If it is missing, the error names the expected path.
- **`[ping]` binary not installed on the host:** the local-agent step reports it exactly as a missing first-party host agent does. The container image is unaffected.
- **`[ping]` added to, or changed in, an already-approved definition:** the hash changes, so the next launch or `ready` re-prompts with the new host command before anything runs. This applies to global definitions too: adding `[ping]` to a previously approval-free global file makes it need approval.
- **A code agent inside the container writes a repo definition with `[ping]`:** nothing runs until the user approves it on the host, and the approval prompt leads with the host command.
- **`binary` not on PATH after install:** the smoke-test `RUN` fails the build with Docker's output. The ready step reports `Failed` and includes the install-step list in the hint.
- **`run.command[0]` differs from `image.binary`** (a wrapper script, for example): allowed. The smoke test checks `binary`, and the entrypoint is used verbatim.
- **Agent with no non-interactive mode** (no `run.non_interactive`): `-n`/workflow steps keep today's behaviour for maki (the prompt is delivered with no extra token). Document in `docs/15-custom-agents.md` that such agents may not exit on their own in workflows.
- **`plan` requested but undefined:** `PlanModeUnsupported`, as today. An undefined `yolo`/`auto` is a silent no-op, as today.
- **`--acp` with no `[acp]` table:** `AcpUnsupported`, as today.
- **Custom agent under the sbx runtime:** `CustomAgentRuntimeUnsupported` (§4), before any kit or image work. `awman ready` reports it as a failed step, not a panic. A crush/maki user on sbx gets this error after recreating the agent with `awman new agent`, so the migration hint (§2) also says that sbx is not supported for custom agents yet.
- **A TOML with `[sandbox]`** (for example written from an older draft of this spec, or by an agent guessing): rejected with the dedicated "do not support Docker Sandboxes yet" message (§1).
- **Builtin runtime on macOS without a builder enabled:** custom images follow the existing `BuildPolicy` and import rules. With no source store image and the builder disabled, `ensure_available` gives the same "build it with ready" error as for first-party agents.
- **Windows hosts:** paths in the TOML are relative and joined with `Path`, and container-side paths are always `/`-joined strings.
- **Very large or malicious TOML:** cap the file at 64 KiB before parsing, and reject arrays longer than 64 entries.
- **`awman new skill --pull builtin` without network:** existing pull error path. With git older than 2.25 (no `sparse-checkout set`), detect the failure and retry once with a plain shallow clone (`--depth 1`, no sparse). That costs more bandwidth but still works, and a warning says why.
- **A user's own library slug is already `awman`** (e.g. a hand-authored `~/.awman/skills/awman/`): the existing rule "a hand-authored skill takes precedence" applies. Document it.
- **Config referencing an agent that is defined only in another repo** (global config `agent = "pi"` with pi in one repo only): `config set` rejects it for global scope (§7). A pre-existing value fails at resolve time with `UnknownAgent`.


## Test Considerations:

**Unit (Layer 0: `data/config/agent_definition.rs`, `data/fs/agent_dirs.rs`, `data/fs/agent_trust.rs`):**
- Every row of the validation table, accepting and rejecting, and asserting that *all* violations are reported together.
- `deny_unknown_fields` typo errors carry the line/column. `schema = 2` gives the newer-awman message. The 64 KiB cap is enforced.
- Stem regex and reserved names. Discovery ordering, the `skipped` reasons, symlink escape and non-toml files ignored.
- Content hash is stable, changes when the escape-hatch Dockerfile changes, and does not change with file mtime.
- Trust store round-trip, replacement of the old hash on re-approval, corrupt file treated as empty, `0600` mode on Unix, and canonical-root keying.

**Unit (Layer 1):**
- `first_party.rs`: port every existing `agent_matrix.rs` test (ping table, settings/skills mounts, system-prompt delivery, mode flags, static env, sandbox fields) to the owned struct, minus crush/maki. Assert that `FIRST_PARTY_AGENTS` has exactly 7 entries and that every one resolves.
- `custom.rs`: TOML → `AgentDefinition` for each `delivery`, `seed_prompt` form, `model` form, and `[acp]`, `[settings]` and `[ping]` present or absent. Assert the hardcoded fields (`credential_source`, `sbx_kit_kind`, …) are always the custom values, and that `ping_argv` is exactly `ping.command` or `None`.
- **Migration equivalence:** the crush and maki example files convert to definitions whose launch-relevant fields equal the pre-change matrix entries (snapshot the old values into the test as literals).
- Dockerfile rendering: snapshot of the rendered crush example; the header hash; `needs_node`/`apt` blocks; one `RUN` per install step; the escape-hatch copy with placeholder substitution; the missing-`FROM {{AWMAN_BASE_IMAGE}}` warning.
- `provision_agent_dockerfile`: every row of the §4 sync table, for both origins.
- sbx refusal: every option builder returns `CustomAgentRuntimeUnsupported` for a custom definition under a sandbox-class runtime, before any kit emission or Dockerfile render (assert with a recording runtime that nothing was emitted or built); an untrusted definition gets the trust error first; first-party agents under sbx are unaffected.
- `[sandbox]` in a TOML gives the dedicated "do not support Docker Sandboxes yet" error.
- The `kind: agent` emitter branch is still covered through the test-only definition.
- `AgentRegistry::resolve`: precedence, invalid repo file not falling back, `UnknownAgent` listing contents and the crush/maki hint, `AgentDefinitionUntrusted` for an unapproved repo definition, and global definitions never requiring trust.
- `HostAgentPinger::ping_command` is `None` for a custom agent without `[ping]`: the ready phase reports `Skipped` with the definition path and never spawns a process (assert with the existing ping test seam). With `[ping]`, the argv is exactly `ping.command` plus one `GREETINGS` entry, it runs in an empty temp directory, and no `run.env`/`auth.env` reaches the host process.
- `ping.command` validation: bare name and absolute path accepted; `./pi`, `bin/pi`, empty arrays, oversized and NUL/newline tokens rejected.
- Overlay: a custom `host_dir` mounts at `<container_home>/<host_dir>`, deny-listed dirs are rejected at the overlay layer too, and `auth.env` passthrough is masked and de-duplicated against `env()` overlays.
- `keychain::credential_source` for a custom name is `None` with no file I/O.
- Skill library: `resolve_pull_target("builtin")` and `("awman")` map to the sparse slug; the `sparse` default for legacy `.awman.json`; the `clone_repo_sparse` argv; the sparse-failure fallback.

**Unit (Layer 2):**
- `ensure_agent_trusted`: prompt shown with the full summary, approve writes the entry, decline exits 1, non-interactive fails with the `awman ready` hint, and `--yes` doesn't approve.
- Review summary: with `[ping]`, the host-command block comes first and shows the shell-quoted argv, the `<greeting>` placeholder and the resolved host path (or "not found on PATH"); without it, the "no host command" line. A global definition with `[ping]` prompts; one without does not.
- `new agent`: skeleton vs `--from`, `--global` path, refuses an existing file without `--force`, rejects reserved names, auto-approves the written content (repo and, when it has `[ping]`, global), and prints the host-command block for `--from crush`.
- `config set agent`: per-scope acceptance rules.
- Catalogue: `init --agent` is a String with no `AGENT_VALUES`, `NEW_AGENT` is registered, the tips reference real commands/flags, and the committed reference regenerated.

**Integration (`tests/`):**
- `tests/engine/`: a temp repo with `.awman/agents/pi.toml` (install-step mode) runs ready, renders the Dockerfile, and passes a recording `LocalImageBuilder` the expected Dockerfile and tag. Edit the TOML and check re-render plus forced rebuild. Repeat with escape-hatch mode.
- `tests/command/dispatch_real_engines.rs`: `chat --agent pi` with a fake runtime produces the expected `ContainerOption`s: entrypoint, mode flags, model, system prompt, settings overlay, auth env.
- Workflow preflight with a mix of first-party and custom agents, one of them an untrusted repo definition, fails preflight with the untrusted error before any container starts.
- Squad: `ensure_directory_workspace_project` renders a custom definition, and an untrusted one is refused in the daemon without prompting.
- sbx: `awman ready --agent crush` (custom, from the example) under `docker-sbx-experimental` exits with `CustomAgentRuntimeUnsupported` as a failed ready step, and writes no kit directory.
- Every container-class runtime (Docker, Apple Containers, builtin-experimental) runs the same custom agent through its existing test harness and gate (Docker-gated, Apple-gated, builtin hardware-gated), so support is claimed only where a runtime's test actually ran.

**End-to-end (CLI binary):**
- `awman new agent crush --from crush`, then `awman ready --agent crush` (with the Docker-gated test harness, as for existing image tests) builds and smoke-tests the image.
- `awman chat --agent maki` with no definition exits with the migration hint (exit code 4).
- `awman new skill --pull builtin` against a local bare-repo fixture (no network; reuse the skill-library test pattern) yields `~/.awman/skills/.library/awman/skills/create-custom-agent/SKILL.md`, and only the `skills/` tree is checked out.
- The `create-custom-agent` skill files exist and the examples it ships parse, which ties the skill to the binary's schema.

**Regression guards:**
- Grep-style test: no `include_str!` of `Dockerfile.crush`, `Dockerfile.maki` or `sbx-*.crush|maki` remains, and no `"crush"`/`"maki"` literal appears in `src/engine/agent/first_party.rs`.
- The existing builtin-runtime `matrix_tests.rs` still loops over every first-party agent.


## Codebase Integration:
- Follow established conventions, best practices, testing and architecture patterns from the project's aspec.
- **Layering** (`aspec/architecture/design.md`):
  - TOML schema, discovery and trust store are Layer 0 (`src/data/`), with no engine imports.
  - Conversion, rendering, registry and enforcement are Layer 1 (`src/engine/agent/`).
  - Prompts, catalogue entries and `new agent` are Layer 2 (`src/command/`).
  - The review summary is a typed outcome that the CLI and TUI render (Layer 3 is presentation only). Do not put trust prompting in the engine.
- **Mirror existing patterns** rather than inventing new ones:
  - `SkillDirs` / `skill_library.rs` for `AgentDirs` and metadata files
  - `download.rs::atomic_write` for all file writes
  - `ensure_not_symlinked` for symlink containment
  - `GitEngine::*_logged` pairs for the sparse clone
  - `catalogue/new.rs` `NEW_SKILL` for `NEW_AGENT`
  - the `squad.rs` catalogue layout
- Keep `AgentDefinition` a plain data struct with the existing helper methods (`validate_run`, `mode_flags`, `entrypoint_for`, `entrypoint_for_acp`, `model_flag_for`). There is no trait object per agent and no dynamic dispatch. "Readable by an intermediate Rust developer" (CLAUDE.md) is the bar.
- Errors: add `EngineError::UnknownAgent`, `AgentDefinitionInvalid { path, problems: Vec<String> }` and `AgentDefinitionUntrusted`, mapped to exit classes per `aspec/uxui/cli.md`. Don't stringify into `EngineError::Other`.
- Keep the security-exception comments accurate (`host_agent.rs`, `ready/phases.rs`), and add the custom-definition section to `aspec/architecture/security.md` as part of the implementation.
- Regenerate `docs/14-command-reference.md` through the markdown projection, never by hand.
- **Out of scope (follow-ups):**
  - `awman new agent --interview` (launch the configured agent with `skill(awman/create-custom-agent)` pre-mounted)
  - publishing community agent definitions as a pullable library
  - a configurable model flag *name* (custom agents use `--model` in this work item, as all current agents do)
  - per-definition credential refresh
  - Docker Sandboxes (`docker-sbx-experimental`) support for custom agents, including sbx launch-time auto-auth for custom kits ([appendix](#appendix-deferred-docker-sandboxes-support-for-custom-agents))

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **New user guide `docs/15-custom-agents.md`:**
  - what a custom agent is and where definitions live (repo/global, precedence)
  - the full TOML reference with the validation table
  - install steps vs escape-hatch Dockerfile
  - how images are rendered and rebuilt, and how to take ownership of a rendered Dockerfile
  - supported runtimes: Docker, Apple Containers and builtin-experimental; Docker Sandboxes is not supported for custom agents yet
  - the approval prompt and trust store
  - the optional `[ping]` host check: what runs on the host, that the approval prompt shows it, and that `ready` skips the host check without it
  - `awman new agent`
  - the builtin `create-custom-agent` skill workflow
  - crush/maki migration
  Add it to `docs/contents.md`.
- **`docs/03-agent-sessions.md`:** remove crush/maki from every agent table and list, replace the "Crush authentication" section with a pointer to the crush example and `auth.env`, and add a short "Custom agents" section that links to doc 15.
- **`docs/00-getting-started.md`:** update the supported-agents list and the `--agent` values; mention custom agents in one line.
- **`docs/07-configuration.md`:** `agent` accepts custom agent names; describe the `~/.awman/agents/` and `.awman/agents/` locations and `~/.awman/trusted-agents.json`.
- **`docs/08-overlays.md`:** remove the crush/maki "no native injection" mentions (replace them with "agents whose definition uses `delivery = \"none\"`"); document `skill(awman/...)` and `awman new skill --pull builtin`.
- **`docs/11-runtimes.md`:** remove crush/maki from the kit tables, and state that custom agents (including crush and maki) run on Docker, Apple Containers and builtin-experimental but not on Docker Sandboxes yet.
- **`docs/04-security-and-isolation.md`:** custom definitions, approval and the deny-list.
- **`docs/14-command-reference.md`:** regenerated.
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs).
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`.
- **Docs are for end users**, not for developers trying to understand implementation.

See `CLAUDE.md` for more guidance on documentation standards.


## Appendix: deferred Docker Sandboxes support for custom agents

Not part of this work item. This records the design that was dropped from scope, so it can be picked up later without redoing the research. The crush/maki kit templates it would replace were last present at commit `f38ad465` (`templates/sbx-kit.crush.yaml`, `sbx-kit.maki.yaml`, `sbx-apply.crush.sh`, `sbx-apply.maki.sh`).

**TOML: a `[sandbox]` table (presence ⇒ `docker-sbx-experimental` is supported).** This is the key that schema 1 reserves and rejects today.

```toml
[sandbox]
install = ["for i in 1 2 3 4 5; do npm i -g @mariozechner/pi-coding-agent && break; sleep 3; done"]  # default = image.install
allowed_domains = ["api.anthropic.com", "registry.npmjs.org"]   # required when [sandbox] is present
proxy_env = ["ANTHROPIC_API_KEY"] # optional; must map to a known sbx service (engine::auth::service_for_credential)
```

**Validation:**

| Field | Rule |
|---|---|
| `sandbox.install` entries | Same as `image.install`: non-empty, single-line, at most 4096 bytes. |
| `sandbox.allowed_domains` | Required when `[sandbox]` is present. Hostnames only (no scheme or path). |
| `sandbox.proxy_env` | Each entry must resolve through `service_for_credential`. |

**Model:** add `sandbox: Option<SandboxSpec>` to `AgentDefinition` (custom agents only; first-party agents keep their embedded kit templates and `SbxKitKind`). `DSbxKitEmitter::emit_for_agent` takes `&AgentDefinition` instead of `&str`. The hardcoded custom values stay: `sbx_kit_kind = Agent`, `sandbox_permission_mode_supported = false`, `sandbox_auth_env_vars` empty (today's `kind: agent` kits do not take part in launch-time auto-auth), `sandbox_model_note = None`.

**Kit rendering (`custom.rs`):**

- Build the `kind: agent` kit **as a `serde_yaml` value**, never by string substitution, so a user string can't inject YAML.
- Base `docker/sandbox-templates:shell-docker`, `agent.entrypoint.run` = `run.command`, `persistence: persistent`.
- `commands.install` from `sandbox.install`, each entry with the same `description`.
- `network.allowedDomains` from `allowed_domains`.
- `credentials.sources` and `environment.proxyManaged` from `proxy_env` via `service_for_credential`.
- `commands.startup` runs one generic `templates/sbx-apply.custom.sh`: today's `sbx-apply.crush.sh` with the agent name replaced by `$AWMAN_AGENT_NAME` (the crush and maki scripts were already generic). `set_executable` is a no-op on Windows.

**Runtime behaviour:**

- Replace `CustomAgentRuntimeUnsupported` with a per-definition check: a custom agent **without** `[sandbox]` fails at `DSbxKitEmitter::emit_for_agent` with `agent 'pi' does not declare [sandbox] support in <path>; add a [sandbox] table or use another runtime`, and `awman ready` reports it as a failed step.
- The `kind: agent` emitter branch kept by this work item is the one these kits use.

**Security and review:**

- The review summary adds each `sandbox.install` step verbatim and `sandbox.allowed_domains`.
- The content hash already covers the whole TOML, so a `[sandbox]` edit needs re-approval with no extra work.

**Migration:** give `examples/crush.toml` and `maki.toml` `[sandbox]` tables carried over from the old kits: `allowed_domains = ["api.anthropic.com", "registry.npmjs.org"]`, `proxy_env = ["ANTHROPIC_API_KEY"]`, and the retrying npm install (`@charmland/crush` / `maki`) as `sandbox.install`. The migration-equivalence test then also compares the emitted kit with the old templates.

**Tests:**

- Emitted YAML parses. User strings containing `:`, `#` and quotes round-trip safely; newlines in domains are rejected at validation.
- The `[sandbox]`-missing error.
- `tests/engine/sbx.rs`: kit emission for the crush example equals a stored golden YAML (the old `sbx-kit.crush.yaml` rendered).

**Docs:** `docs/11-runtimes.md` and `docs/15-custom-agents.md` describe sbx support via `[sandbox]`. The `create-custom-agent` skill's research checklist gains "network domains it needs".

**Still out of scope even then:** sbx launch-time auto-auth for custom kits.
