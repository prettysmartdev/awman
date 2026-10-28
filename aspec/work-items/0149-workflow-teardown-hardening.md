# Work Item: Enhancement

Title: workflow teardown hardening
Issue: issuelink

Related: 0140 (per-session secrets, git credential helper, redaction), 0146/0147 (daemon modes, worker mode).

> **To the leader implementing this work item:**
>
> 1. **Mandatory security reviews by a Fable- or Astra-class model.** This item adds a new host-execution path (`HostSecretCommand`), handles GitHub tokens and SSH private keys, and changes what repo-level config may do. You **must** use a Fable- or Astra-class model for **two** security reviews:
>    - **Plan review.** Before writing code, have the model review your implementation plan against `aspec/architecture/security.md`, this work item and the threat surface listed below. Resolve or explicitly reject every finding.
>    - **Implementation review.** After implementation and before you consider the work item done, have the model review the final diff against the same material.
>
>    No smaller or faster model may stand in for either review. Record both reviews' findings and how each was resolved in a `## Security Review` section appended to this file.
>    The threat surface to review includes:
>    - host command execution and agent-binary refusal;
>    - token handling, redaction and zeroization;
>    - SSH key mounts;
>    - repo-config "may only narrow" enforcement and approval-by-hash;
>    - API and squad widening;
>    - grant scoping (no grants to remediation agents, `run_shell` steps or leaders);
>    - persisted files (`prerequisite-grants.json`, the trust store).
> 2. **Read `aspec/architecture/2026-grand-architecture.md` and `aspec/architecture/design.md` before starting.** Follow the layer placement in §0 below. Where something here seems to conflict with those documents, ask the developer rather than choosing yourself.

## Summary:
- Awman can run a variety of teardown steps as part of workflows including creating branches, committing code, and opening PRs.

Currently, it is very easy for those kinds of steps to fail because they do not have the required access, secrets, folders, etc.

awman needs to enhance its workflow handling to make it easier for users to use these teardown steps successfully.

When running a workflow with teardown steps (or any dynamic workflow, since the existence of teardown steps cannot be known at the beginning therefore any could be in play), awman needs to run checks to see if the various types of teardown steps will have everything they need in order to run successfully.

for example, successfully creating a new commit requires git be configured with an identity (name and email) otherwise it will fail

pushing branches and creating PRs require some kind of authentication such as SSH keys or GH and a github token.

When a workflow is started via CLI, TUI, or API, awman should run checks to ensure everything those steps need are accessible by awman, warn the user if they are not, and offer to remediate in-situ where possible for interactive frontends.

For example, if a workflow includes a push_branch teardown step, awman must check to see if SSH keys are available on the host, and EXPLICITLY ASK the user if those SSH keys can be used for the workflow by mounting them into the teardown step's container via an overlay

Another example, if a workflow includes a create_pr step, awman must ensure that a github token is available via environment variable, and EXPLICITLY ASK the user if it can be used for teardown steps. If no token is available in the environment, awman must prompt the user if 1) They would like to paste in a token 2) They would like awman to execute a gh command ON THE HOST to create a token 3) Proceed without a token (with a warning that creating a PR will fail)

Similar checks, warning, and automated offers to remediate should be designed and implemented for each teardown step. For non-interactive frontends and scenarios like API and Squad, propose a solution that allows for clients/users to configure their preferences for behaviour. Also allow for choices to be encoded in awman config.json files with appropriate safety measures for repo-level settings. These settings should include the ability to pre-define a command that can be run on the host to acquire a token or secret if needed when it is missing. This should apply to squad as well. Design with security, UX, helpfulness, and hardened behaviour in mind to prevent accidental errors at the end of a long workflow, and always assume a dynamic workflow will need any or all of the teardown types since it is unknowable upfront.

### How things stand today
- Teardown steps (`TeardownStep`, `src/data/workflow_definition.rs:127`) run as shell strings (`teardown_step_to_shell`, `src/engine/workflow/step_commands.rs:128`), each in its own fresh container built from the project base image.
  - `commit_changes` runs `git commit`.
  - `push_branch` runs `git push -u` with `GIT_SSH_COMMAND=ssh -o StrictHostKeyChecking=accept-new`.
  - `create_pull_request` runs `gh pr create`.
  - `poll_ci` is the exception: the engine runs it on the host (`src/engine/workflow/poll_ci.rs`) through `gh` or `GITHUB_TOKEN`.
- These containers get no credentials automatically. Users must add `ssh()` / `env(GITHUB_TOKEN)` overlays to each step, and so must the dynamic leader (`src/assets/dynamic/leader-prompt.md:133`). Nothing is checked before the run, and a missing credential surfaces only as a failed step at the very end of a long run.
- The only preflight is the warning for `commit_changes` with missing git identity (`prepare.rs:469`). It checks the **host** view (`GitEngine::identity_configured`), so a global `~/.gitconfig` passes it even though the container never sees that file and the commit fails there.
- The stock `templates/Dockerfile.project` installs neither `gh` nor `openssh-client`, so `create_pull_request` and an SSH `push_branch` fail on a default project image.
- An HTTPS `push_branch` has no credential path at all.
- The API and squad cannot ask a question. Every decision comes from `HeadlessDefaults` (`src/command/headless.rs`), and `CreateCommandRequest` has no field for preferences.

## User Stories

### User Story 1:
As a: developer running `awman exec workflow --dynamic --work-item 42` from the TUI or CLI

I want to:
be told before the leader starts that my git identity, SSH keys or GitHub token won't reach the teardown containers, and be asked explicitly whether awman may use them, with options to fix each gap there and then (enter an identity, mount my key read-only, paste a token, or run `gh auth token` on my host)

So I can:
come back to a pushed branch and an open PR instead of a run that failed at its last step after an hour of agent work

### User Story 2:
As a: API client or squad task author running workflows unattended

I want to:
declare my teardown preferences once, per request, per task or in global config (for example "use `GH_TOKEN` from my secrets", "never mount SSH keys", "fail before starting if a required credential is missing", "run `op read op://ci/github/token` on the host when no token is set")

So I can:
get a deterministic, machine-readable go/no-go before any container starts, rather than finding a `teardown_failed` outcome hours later

### User Story 3:
As a: developer who clones repositories I don't fully trust

I want to:
be sure that a repo's `.awman/config.json` can make teardown stricter but can never silently grant my SSH keys or tokens, or run a host command, without my explicit approval

So I can:
use teardown automation without handing credentials to whoever wrote the checkout's config


## Implementation Details:

### 0. Layer placement (grand architecture)
Each piece sits in the layer the grand architecture assigns it. Layers depend only downward. Lower layers reach upward only through traits.

| Layer | Owns | Items |
|---|---|---|
| 0 data | serialisable types, config resolution, filesystem, env | `WorkflowPrerequisite`, `WorkflowPrerequisites`, `PrerequisiteCheck`, `WorkflowPrerequisiteReport` (secret-free, serialisable; `src/data/workflow_prerequisites.rs`); `PrerequisitePolicy` and its layering with the repo-may-only-narrow rules, resolved by `EffectiveConfig::prerequisite_policy()` (`WorkflowsConfig` / `PrerequisitesConfig` in `src/data/config/workflows.rs`, modelled on `builtin_network.rs`); `SshKeyInventory` (reads key headers and `known_hosts`; `src/data/fs/ssh_keys.rs`); `PrerequisiteGrantStore` (`~/.awman/prerequisite-grants.json`; `src/data/fs/prerequisite_grants.rs`); `PhaseToolsCache` (`src/data/fs/phase_tools_cache.rs`); the `TextPrompt.masked` field (`src/data/prompt.rs`); the `EventPayload::WorkflowPrerequisites` payload (`src/data/execution_event.rs`); the `Task.prerequisites` field (`src/data/fs/task_store.rs`) |
| 1 engine | runtime primitives | `GitEngine`: `local_identity`, `remote_url`, `set_local_identity` (hardened `host_command.rs`). `AuthEngine`: `GitHubTokenResolver` (env, `gh auth token`, `acquire`, verification) and `HostSecretCommand` (`src/engine/auth/host_secret_command.rs`). `ContainerRuntime::probe_phase_tools(image)`: an inherent container-tier method, selected by capability and never by runtime name. `PrerequisiteGrants`: holds `SecretString`, which is Layer 1, so it cannot live in Layer 0 (`src/engine/workflow/prerequisite_grants.rs`). `OverlayEngine::prerequisite_grant_overlays(step_kind, &grants)`: the only place grants become mounts or env. `PrerequisitePreflight`: built with typed options, `run() -> WorkflowPrerequisiteReport`. `WorkflowEngine`: receives the CI token as an option for `poll_ci` |
| 2 command | business logic, prompts, dispatch | `WorkflowPrerequisiteFlow`: turns prerequisite report + policy + frontend into grants. `WorkflowPrerequisiteFrontend` trait. Prompt construction in `src/command/prompts.rs`. `HeadlessDefaults` rows. Catalogue flags and the `config prerequisite-grants` subcommand. Widening checks per `FrontendKind` (API/squad) |
| 3 frontend | presentation and input only | CLI table and masked read (crossterm raw mode, an existing dependency; no new crate). TUI prerequisites modal, the `DialogRequest::SecretInput` render and terminal release for attached host commands. API rendering of the event. Squad log rendering. No frontend computes prerequisites, choices, defaults or policy |

Rules that follow from this:
- Tenet 3 (typed objects over free functions): prerequisite derivation is `WorkflowPrerequisites::from_workflow(&Workflow)` / `WorkflowPrerequisites::assume_all()`, not a free function. The preflight is a `PrerequisitePreflight` built with options. The host runner is a `HostSecretCommand` value.
- `PrerequisiteGrants` is held by `ExecWorkflowCommand` for the life of the run and passed down. It is **never** put in `Session`/`SessionState`, which are mirrored to frontends and the API. Only the secret-free `WorkflowPrerequisiteReport` goes there, for display.
- Engines never prompt. `WorkflowEngine` and `PrerequisitePreflight` return data. Every question goes through the Layer 2 `WorkflowPrerequisiteFlow` and its frontend trait.

### 1. Concepts
- **Naming: "workflow prerequisites".** The concept is generic and is named that way everywhere: types, modules, config keys, flags, events, prompts and docs. Teardown steps are its **first** consumer. Later work can add prerequisites for setup steps, agent steps or the workflow as a whole without renaming anything.
  - Never introduce `teardown`-prefixed names for this machinery. "Teardown" appears only where a statement really is about teardown steps (for example "`create_pull_request` needs `GitHubToken`").
  - Every prerequisite records which step kinds and phases need it (`needed_by: Vec<StepRef>`), so the framework is not tied to the teardown phase.
- **Workflow prerequisite** (`WorkflowPrerequisite`, Layer 0 data in `src/data/workflow_prerequisites.rs`): one thing a workflow step needs to succeed. The kinds below are the ones this item introduces; all are needed by teardown steps.

  | Prerequisite | Needed by | What "satisfied" means |
  |---|---|---|
  | `GitIdentity` | `commit_changes` | `user.name` and `user.email` resolve **inside the step container**: from repo-local `.git/config` (or the worktree's common dir), or from a granted identity awman injects |
  | `GitRemote{remote}` | `push_branch`, `create_pull_request` | the named remote (default `origin`) exists; its URL is classified as `Ssh{host}`, `Https{host}` or `Other` |
  | `PushAuthSsh{host}` | `push_branch` to an SSH remote | a granted SSH key is mounted |
  | `PushAuthHttps{host}` | `push_branch` to an HTTPS remote | a granted token is available for that host (GitHub only in this item) |
  | `GitHubToken` | `create_pull_request` (container), `poll_ci` (host) | a granted token, or for `poll_ci` only, an authenticated host `gh` |
  | `ContainerTool{name}` | `git` (commit/push/PR), `gh` (PR), `ssh` (SSH push) | the binary exists in the phase image (`resolve_phase_image`) |
  | `PushedBranch` | `create_pull_request` | a `push_branch` step comes before it, or the branch already has an upstream; otherwise `gh pr create` tries to prompt and fails in a headless container |
  | `NotDetachedHead` | `push_branch` with default `branch` | HEAD is on a branch (always true in a worktree run) |

- **Prerequisite derivation** (`WorkflowPrerequisites::from_workflow(&Workflow)` / `::assume_all()`, Layer 0, pure):
  - Static workflow: the union of each teardown entry's prerequisites. `run_shell` / `run_script` are opaque and add none, but a heuristic scan for `git push`, `git commit` and `gh ` emits an **advisory** hint ("this step looks like it pushes; grants are not applied to run_shell steps, add overlays yourself"). Never a hard check.
  - Dynamic workflow (`--dynamic`), a squad evaluation, or any generated workflow: `WorkflowPrerequisites::assume_all()` against the session's `origin`. The workflow does not exist yet, so assume it may use every teardown kind.
  - After a dynamic leader's `workflow.toml` validates (`validate_generated_workflow`), recompute from the real teardown steps. Grants for unused prerequisites are **dropped** (no SSH mount if nothing pushes). A newly needed prerequisite that was declined produces a warning, plus a fresh prompt if a human is present.
  - Setup steps `pull_branch` and `clone_repo` with SSH or private URLs reuse the same `PushAuth*` checks (mapped to fetch auth). Low cost, same machinery.
- **Grant** (`PrerequisiteGrants`, Layer 1 `src/engine/workflow/prerequisite_grants.rs`, in memory, one per run, owned by `ExecWorkflowCommand`): the concrete things the user approved.
  - `identity: Option<GitIdentityGrant{name, email, source}>`
  - `ssh: Option<SshGrant{key_path, known_hosts: Option<PathBuf>}>`
  - `token: Option<TokenGrant{secret: SecretString, source: Env(NAME) | Pasted | HostCommand(label) | ApiSecret(NAME)}>`
  - Never serialized. Its `Debug` output redacts the token.
- **Prerequisite report** (`WorkflowPrerequisiteReport { checks: Vec<PrerequisiteCheck> }`): each check carries `prerequisite`, `status: Satisfied | NeedsConsent | Missing | Unknown`, an `explanation`, the affected step names (or "any teardown step" for dynamic), and an ordered `remediations: Vec<Remediation>`. Every frontend renders this one value, and it is the payload of the API event (§6).

### 2. Probes (`PrerequisitePreflight`, `src/engine/workflow/prerequisites.rs`)
`PrerequisitePreflight` is built with typed options:
- the prerequisites;
- the session's git root and worktree;
- the phase image;
- `Arc<dyn AgentRuntimeEngine>` plus the optional typed `Arc<ContainerRuntime>`;
- the policy.

Each source it consults is behind a small trait so tests inject fakes. Every source sits in the layer that owns that concern:
- host git: `GitEngine`, through the hardened `engine/git/host_command.rs` (hooks, fsmonitor and drivers disabled);
- filesystem and env: Layer 0 (`SshKeyInventory`, `EnvSnapshot`, `PhaseToolsCache`);
- tokens: `AuthEngine` (`GitHubTokenResolver`);
- images: `ContainerRuntime`.
- **Identity** (`GitEngine::local_identity`): `git -C <worktree or root> config --local user.name/email` (and the worktree common dir). This replaces today's host-global probe, which gives a false OK. The host's global values are read separately, only to *offer* them.
- **Remote** (`GitEngine::remote_url`): `git remote get-url <remote>`. Classify scp-style `git@host:` and `ssh://` as SSH, and `https://` as HTTPS. Credentials embedded in a URL are stripped before the URL is displayed anywhere.
- **SSH keys** (`SshKeyInventory`, Layer 0): list `~/.ssh/id_*` private keys (skip `*.pub`). For each, read the header to detect a passphrase (`Proc-Type: 4,ENCRYPTED`, or OpenSSH `kdfname != none`), and read `~/.ssh/known_hosts` for the remote host. Never read more than the header. Report file mode problems (a private key that isn't `0600`).
- **Token in env** (`GitHubTokenResolver`, reading `EnvSnapshot`): check `GH_TOKEN`, then `GITHUB_TOKEN` (configurable list, §5) via `EnvSnapshot`. A value is never logged; the report says only "found in `GH_TOKEN`".
- **Host gh** (`GitHubTokenResolver`): `gh auth status --hostname <host>`. `poll_ci`'s `gh_is_available` already uses it. Move it into `AuthEngine` and have `poll_ci` call it there.
- **Container tools** (`ContainerRuntime::probe_phase_tools`): run `sh -c 'command -v git gh ssh'` in a throwaway container of the phase image, with no mounts and no network (`--network none` where supported). This is container execution, not host execution. Use it only when the session has a typed container-tier handle. The sandbox tier has no background phase-container primitive and reports `Unknown`. Never match on the runtime name. The result is cached per image digest by `PhaseToolsCache` (`~/.awman/cache/phase-tools.json`), so repeat runs pay nothing. Under `AWMAN_TEST_ISOLATION` without `AWMAN_TEST_DOCKER`, the result is `Unknown`, not `Missing`.
- **Token verification** (`GitHubTokenResolver`; optional, best effort): `GET https://api.github.com/user` with the token. Report the login and, for classic PATs, whether `X-OAuth-Scopes` includes `repo`. A network failure gives `Unknown` and never blocks. Disabled under test isolation.
- Probes run concurrently with a 10 s budget each. A probe that times out yields `Unknown` with a hint. It never stalls the run.

### 3. When checks run
- `execute.rs::run_with_frontend`, after `Workflow::load` and agent approval but **before** the worktree is created and before any container starts. For `--dynamic`, in `dynamic.rs::run_dynamic`, before the leader launches. So questions are asked while the person is still at the keyboard, not after an hour of agent work.
- Re-run just before the teardown phase starts (in `execute_prepared`, before `prepare.rs:662`), without prompting. Only re-acquire a token from a host command marked `refresh: "beforeTeardown"`, and re-check that a granted env var is still set (squad env may have changed). Failures here become one clear `WorkflowPrerequisites` warning, not N step failures.
- Resume (`ask_workflow_resume` → yes): grants were never persisted, so the preflight runs again before resuming.
- New flag `awman exec workflow <path> --check-prerequisites` (also `--dynamic --check-prerequisites`) runs only the preflight, prints the report, and exits: 0 if all satisfied, 1 if anything is missing. Useful in CI and before `squad add`.
- Cancelled teardown (`run_teardown_cancelled`) already skips push, PR and poll_ci, so the pre-teardown re-check is skipped there.

### 4. Interactive remediation (CLI on a TTY, TUI)
All questions are `Prompt<D>` / `TextPrompt` values built in `src/command/prompts.rs`, so CLI and TUI show identical labels and defaults (aspec/uxui/cli.md "Prompt and dialog conventions"). The new frontend trait `WorkflowPrerequisiteFrontend` (in `src/command/commands/workflow_prerequisites.rs`, added to `ExecWorkflowCommandFrontend`'s supertraits) has these methods:
- `show_workflow_prerequisites(&WorkflowPrerequisiteReport)`: a summary first ("3 of 5 workflow prerequisites need your attention"). The TUI shows a modal list; the CLI prints a table.
- `ask_prerequisite_identity(&Prompt<IdentityChoice>)`:
  - `Use host identity "Ada <ada@x>"`: injected as `GIT_AUTHOR_NAME/EMAIL` + `GIT_COMMITTER_NAME/EMAIL` env on `commit_changes` containers only.
  - `Enter a name and email`: two `TextPrompt`s, validated as non-empty, no newlines, and email contains `@`.
  - `Set it in this repo's .git/config`: runs host `git config --local` through `GitEngine::set_local_identity` (hardened) after showing the exact command. Persistent, so it is listed last.
  - `Continue without` (dismiss default): warns "commit_changes will fail".
- `ask_prerequisite_ssh(&Prompt<SshChoice>)`. Title: "push_branch pushes to git@github.com:org/repo.git over SSH. Allow awman to mount an SSH key into the push_branch container?" The body names the key, the container path and read-only.
  - `Mount ~/.ssh/id_ed25519 read-only for this run` (one choice per detected key).
  - `…and remember for this repo` (stored as described below).
  - `Mount the whole ~/.ssh read-only`: today's `ssh()` behaviour. Offered last, because it also exposes `config`, other keys and agent sockets in config files.
  - `Don't use SSH keys` (dismiss default): warns "push_branch will fail".
  - A passphrase-protected key is listed with "(passphrase-protected: will fail unattended)". If it is the only key, the prompt says so and recommends a deploy key or HTTPS+token.
- `ask_prerequisite_token_consent(&Prompt<TokenConsent>)`, shown when a token is found in the environment. Title: "create_pull_request and poll_ci need a GitHub token. Use the token in GH_TOKEN for teardown steps?" Choices: `Yes, this run` / `Yes, and remember for this repo` / `No`. The token goes **only** to the containers that need it (§7).
- `ask_prerequisite_token_source(&Prompt<TokenSource>)`, shown when no token is found or consent was declined:
  1. `Paste a token`: `TextPrompt` with a new `masked: true` field. The TUI needs a new `DialogRequest::SecretInput` that renders `•`; the CLI reads the TTY with echo off using crossterm raw mode, an existing dependency (no new crate). The value is held in a `SecretString` and never written anywhere.
  2. `Run "gh auth token" on this host`, shown only when `gh` resolves on `PATH`. The body shows the exact argv and the resolved path, and says it runs ON YOUR HOST. If `gh auth status` says not logged in, a follow-up choice offers `gh auth login --hostname github.com --web --git-protocol https` in the foreground. The command layer runs it through `HostSecretCommand` in attached mode, and asks the frontend for the terminal through a new `WorkflowPrerequisiteFrontend::with_terminal_released(f)` hook. The CLI's implementation just runs `f`. The TUI's implementation leaves the alternate screen and restores it afterwards, with the same user-visible behaviour as commit signing (docs/04 "Commit signing"). The frontend never decides what runs. After that, `gh auth token` runs.
  3. `Run the configured token command` (label and argv shown), present when `workflows.prerequisites.githubToken.acquire` resolves (§5).
  4. `Continue without a token` (dismiss default): warns "create_pull_request will fail; poll_ci will fall back to unauthenticated, rate-limited requests".
- Missing container tool: there is no in-situ fix for an existing project, because awman never edits Dockerfiles silently. (New projects get `gh` and `openssh-client` from the updated template; see Codebase Integration.) `Prompt<ToolChoice>`: `Continue (the step will fail)` / `Abort the run` (default). The body shows the exact Dockerfile line to add (`RUN apt-get install -y gh openssh-client`) and the file (`.awman/Dockerfile.project` or the configured base image). For dynamic runs, the gap is also passed to the leader (§8).
- Missing remote, detached HEAD, or PR without a preceding push: warning plus `Continue` / `Abort`. No grant applies.
- "Remember" choices go to `~/.awman/prerequisite-grants.json`, written atomically with mode 0600 like `TrustStore` (`src/data/fs/agent_trust.rs`). Each record is keyed by canonical git root + remote URL + prerequisite, and stores **only the decision** (`ssh_key: "~/.ssh/id_ed25519"`, `token_env: "GH_TOKEN"`), never a secret. A changed remote URL invalidates the record. `awman config prerequisite-grants list|revoke` manages the records. A malformed file grants nothing.
- Dismissing any prompt (Esc / Ctrl-C) applies nothing for that prerequisite, matching cli.md. Ctrl-C on the summary aborts the run with exit 130.

### 5. Configuration (`workflows.prerequisites` block, both scopes)
The settings live under a new top-level `workflows` object, at `workflows.prerequisites`. There is no top-level `prerequisites` key.
- `workflows` is the home for workflow-wide settings. Its Layer 0 type is `WorkflowsConfig`, with a `prerequisites: Option<PrerequisitesConfig>` field, on both `RepoConfig` and `GlobalConfig`.
- **`workflows.dynamic` is reserved** for WI 0150, which moves today's `dynamicWorkflows` block there. This item must not use the `dynamic` key for anything else, and must not reject it.
  - If 0150 lands first, it creates `WorkflowsConfig` and this item adds the `prerequisites` field to it.
  - If this item lands first, `WorkflowsConfig` has no `dynamic` field yet, and a `workflows.dynamic` key is ignored.
- **Unknown keys:**
  - Under `workflows`, they are ignored. This keeps config written by a newer awman loadable, the same forward-compatibility rule `DynamicWorkflowsConfig` follows.
  - Under `workflows.prerequisites`, an unknown key produces a load **warning** naming the key, rather than an error. A typo in a credential setting is surfaced, and the safe default (`ask`) applies, without making the whole repo config unloadable for an older awman.
- The existing `dynamicWorkflows` block is unchanged and out of scope here.

```json
{
  "workflows": {
    "prerequisites": {
      "preflight": "warn",
      "unresolved": "warn",
      "gitIdentity": { "use": "ask", "name": "awman bot", "email": "bot@example.com" },
      "ssh": { "use": "ask", "key": "~/.ssh/id_ed25519_deploy" },
      "githubToken": {
        "use": "ask",
        "envVars": ["GH_TOKEN", "GITHUB_TOKEN"],
        "acquire": { "argv": ["op", "read", "op://ci/github/token"], "timeoutSecs": 30, "refresh": "start" },
        "verify": true
      },
      "allowApiClientGrants": false
    }
  }
}
```
Keys below are relative to `workflows.prerequisites`:

| Key | Values | Meaning |
|---|---|---|
| `preflight` | `warn` (default) / `off` | `off` skips probes; prerequisites are then all `Unknown` |
| `unresolved` | `warn` / `fail` | What happens when a prerequisite is still unsatisfied after prompts or policy. `fail` stops before any container starts (exit 2, `error.stage = "workflow_prerequisites"`). Default: `warn` for interactive frontends; for headless frontends, `fail` when a **static** workflow explicitly needs it, otherwise `warn` (dynamic and squad, where need is only assumed) |
| `*.use` | `ask` (default) / `allow` / `deny` | `allow` grants without asking; `deny` never uses the credential (and never asks) |
| `gitIdentity.name/email` | strings | A fixed identity used when `use` is `allow` |
| `ssh.key` | path under `~/.ssh` | Which key `allow` mounts; it must be a regular file inside the real `~/.ssh` (canonicalized, no symlink escape) |
| `githubToken.envVars` | names | Env vars searched, in order |
| `githubToken.acquire` | `{argv, timeoutSecs, refresh: "start" \| "beforeTeardown"}` | A host command whose stdout is the token, used when no env var holds one and `use` is `allow`, or offered as a choice when `use` is `ask` |
| `githubToken.verify` | bool (default true) | Best-effort `GET /user` check |
| `allowApiClientGrants` | bool (default false), **global only** | Lets API clients grant beyond this policy with `--prerequisite-*` flags (§6). Ignored with a warning in repo config |

**Precedence:** flags (CLI, TUI and API alike, §6) > squad task policy > repo config > global config > default. All of it is resolved in Layer 0 by `EffectiveConfig::prerequisite_policy()`, the same place every other config merge happens. No layer above re-merges it. The merge applies the **repo-level safety rules** below. All of them follow the `builtin.network` "repo may only narrow" precedent (docs/07, `builtin_network.rs`), because repo config ships with the checkout and may be untrusted:
- A repo value may set `use: "deny"` or `"ask"`, `unresolved: "fail"`, `preflight: "warn"`, and `envVars` (a subset of the global list, if global sets one).
- A repo `use: "allow"` is **ignored with a warning** ("repo config cannot grant credentials; set this in ~/.awman/config.json or answer the prompt"). Exception: `gitIdentity` with a fixed name and email, which is not a secret. It is shown in the summary as "identity from repo config".
- Repo `ssh.key` is honoured only as a *suggestion* (pre-selected in the prompt), never as a grant.
- **Repo `githubToken.acquire`**: a repo-defined host command can only run after the user approves it for its exact content hash, reusing the agent-trust flow (`AgentTrustFrontend` pattern, `HostCommandReview` rendering "will run this command ON YOUR HOST", records in the existing trust store keyed by repo + sha256 of the argv). API, squad and non-interactive CLI never approve it, and `--yes` does not approve it, the same as agent definitions. An unapproved repo `acquire` is skipped with a warning.
- Global `acquire` is trusted (the user wrote it), but the full argv is still shown once in the prerequisite summary each run.
- `fields.rs` `CONFIG_FIELDS`: add `workflows.prerequisites.*` entries. `workflows.prerequisites.githubToken.acquire` is `GlobalOnly` for `awman config set` (edit the JSON by hand to put it in a repo, which then needs approval). `RepoConfig` and `GlobalConfig` load validate the block (values, scopes and the unknown-key warning above).

**Host command runner** (`HostSecretCommand`, part of `AuthEngine`: `src/engine/auth/host_secret_command.rs`), the only new host-execution path. It must be added to `aspec/architecture/security.md`.
- argv only, no shell. `argv[0]` is resolved with `which`, and the resolved path is shown in prompts.
- **Refuses** a resolved binary that is a known code agent (`claude`, `codex`, `opencode`, `gemini`, `copilot`, `cline`, and any custom agent definition's binary), so that `HostAgentPinger` stays the only path that runs an agent on the host.
- Runs in a fresh private 0700 scratch dir outside the repo, with a minimal environment (`PATH`, `HOME`, `USER`, `LANG`, `TMPDIR`, plus `XDG_*`/`OP_*`/`GH_*` passthrough so credential managers work). Stdin is the TTY only for interactive frontends, otherwise `/dev/null`. Timeout `timeoutSecs` (default 30, max 300), with the process group killed on drop (reuse the `host_cli.rs` cancellation pattern).
- Output: the first non-empty stdout line, trimmed. Rejected if it is empty, over 8 KiB, or contains whitespace or control characters. Stderr is shown to the user (capped at 4 KiB) but scrubbed of the token value. A non-zero exit means `Missing`, with stderr as the explanation.
- `gh auth token` / `gh auth login` are built-in uses of the same runner with a fixed argv.

### 6. Non-interactive frontends
- **CLI `--non-interactive` / no TTY / `--json`**: `ask` resolves to `deny` through new `HeadlessDefaults` rows (documented in the `headless.rs` table). Then `unresolved` applies. Override flags, declared in the catalogue (CLI, API, TUI-hidden):
  - `--prerequisite-identity host|"Name <email>"|deny`
  - `--prerequisite-ssh <key-path>|all|deny` (`all` = whole `~/.ssh`)
  - `--prerequisite-token env:NAME|acquire|deny`
  - `--prerequisite-unresolved warn|fail`
  - `--check-prerequisites`

  A flag value is an explicit grant, so the interactive prompt for that prerequisite is skipped on a TTY too. `--yes` does **not** grant credentials.
- **API**: preferences are expressed **only** through the same `--prerequisite-*` catalogue flags, in the existing `args` channel of `CreateCommandRequest`. There is no API-only request field (developer decision), so `Dispatch` parses and validates them identically for every frontend. This keeps grand-architecture parity: the API frontend does no validation.
  - `--prerequisite-token secret:NAME` names a key in the command's `secrets` map once WI 0140 lands. Until then, Dispatch rejects it with a usage error ("unsupported until per-command secrets are available"). Other token sources are the server environment (`env:NAME`) and the server's global `acquire`. There is no flag for defining a host command, so a request can never supply one.
  - Widening is checked in Layer 2, by `FrontendKind`. For `Api`, a flag that grants beyond the server's global policy (for example `--prerequisite-ssh <key>` when global `ssh.use` is `ask` or `deny`) is refused unless the server's global config sets `workflows.prerequisites.allowApiClientGrants: true` (global-only, default false). So a client cannot make the server mount its operator's `~/.ssh` unless the operator opted in. Narrowing flags (`deny`, `--prerequisite-unresolved fail`) are always accepted.
  - New event `EventPayload::WorkflowPrerequisites { phase: "start" | "before_teardown", report }` (serialised `WorkflowPrerequisiteReport`, no secret values). It is emitted before any step event, and SSE clients see it first.
  - With `unresolved: "fail"`, the command ends `failed` with `error.stage = "workflow_prerequisites"` and the report, before any container starts.
  - `POST /v1/commands` with `args: ["--check-prerequisites"]` gives a dry run.
  - `flags_applied` reports the resolved prerequisite policy.
- **Squad**:
  - `squad add` / `squad edit` (run from an interactive client) run the preflight against the task workspace with `WorkflowPrerequisites::assume_all()`, ask the same questions, and store the **decisions** on the `Task` (`task_store.rs`, new field `prerequisites: PrerequisitePolicy`, for example `ssh: {use: allow, key: "~/.ssh/id_ed25519"}`, `githubToken: {use: allow, envVars: ["GH_TOKEN"]}`). Never values.
  - The same `--prerequisite-*` catalogue flags on `squad add/edit` set the stored policy, whether they arrive from the CLI, the TUI or the squad routes' `args`. `squad show` displays it.
  - At each run, the daemon resolves tokens from its env store (existing `env_sync` push; the chosen env var is added to the task's required names, so the existing "daemon has no value for" warning covers it). Otherwise it uses the global `acquire` command, which the daemon may run as the user since it is configured globally. A task-level or repo `acquire` needs prior interactive approval (§5).
  - The prerequisite summary is written to the run log, and gaps are appended to the leader prompt the same way missing `env()` names are today (`overlay_summary.rs`).
  - The global `squad.workflows.prerequisites` block (same shape as `workflows.prerequisites`) provides daemon-wide defaults, layered under the task policy.
- **Worker mode (WI 0147)**, when it lands, uses the squad behaviour. This item only needs `PrerequisitePolicy` to be a reusable value.

### 7. Applying grants to containers
- `OverlayEngine::prerequisite_grant_overlays(step_kind, &grants)` (Layer 1) is the only code that turns grants into typed overlay and env options. The grand architecture gives `OverlayEngine` everything mounted from the host into a container. The Layer 2 `collect_single_entry_overlays` (`prepare.rs:1255`) only appends its result after the user and step overlays. It produces **grant overlays by step kind**:
  - `commit_changes`: identity env (`GIT_AUTHOR_*`, `GIT_COMMITTER_*`), passed as step env literals. The name and email are not secret.
  - `push_branch` SSH: single-file read-only mounts of the chosen key at `~/.ssh/awman_push_key` and (if present) `known_hosts` at `~/.ssh/known_hosts`. `GIT_SSH_COMMAND` becomes `ssh -i ~/.ssh/awman_push_key -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=<writable tmp copy>`. The "whole ~/.ssh" choice instead uses today's `ssh()` directory mount.
  - `push_branch` HTTPS to a GitHub host (`step_commands.rs` builds the argv and env from a typed `PushAuth` option; Layer 2 does no string concatenation): token via env `AWMAN_GIT_TOKEN` (passed by name, never argv), with `git -c credential.helper= -c 'credential.helper=!f(){ echo username=x-access-token; echo "password=$AWMAN_GIT_TOKEN"; }; f'`. When WI 0140's socket helper exists, switch to it. The URL in `.git/config` is never rewritten.
  - `create_pull_request`: `GH_TOKEN` env (by name), plus `GH_PROMPT_DISABLED=1` so `gh` never blocks.
  - `poll_ci` (host-side): `WorkflowEngine` receives the granted token as a typed option at construction and hands it to `CiPoller`, instead of reading `host_var("GITHUB_TOKEN")`.
- Grants are **never** applied to:
  - `run_shell` / `run_script` steps (opaque; users keep explicit overlays);
  - `on_failure` remediation agents (`launch_on_failure_agent`); an agent must not receive the user's push key or token because a teardown step failed;
  - main workflow agent steps or the dynamic leader.
- A token value delivered by any grant is added to the run's redaction set. Exact-match replacement with `***` in step stdout/stderr, captured `teardown-failure-*.txt` files, squad logs and API `StdoutLine`/`StderrLine`. Coordinate with WI 0140's redaction so there is one implementation.
- Container env values come from `SecretString::expose()` only at the runtime call site and go through the child process environment, never argv (the existing `-e NAME` pattern in `background.rs`).
- Every grant mount and env name shows in the printed runtime command like other overlays (`AWMAN_GIT_TOKEN=***`).

### 8. Dynamic workflows and the leader
- The prerequisite report is summarised in the leader prompt (`leader-prompt.md`): "Workflow prerequisites for this run: commit ✓, push over SSH ✓, create_pull_request ✗ (no GitHub token: do not add a create_pull_request step; mention it in your summary)".
- Replace the prompt's instruction to add `env(GITHUB_TOKEN)` / `ssh()` overlays to teardown steps with "awman supplies credentials to teardown steps automatically". Also fix the inaccurate "on the host" comment in `src/assets/dynamic/example-workflow.toml:116`.
- Grants are made once, before the leader starts, then narrowed after validation (§1). The leader cannot widen them: a generated step's own `overlays` keep working (unchanged behaviour), but they are user-visible in the saved workflow, and the post-validation preflight warns when a generated step requests `ssh()` or `env()` of a credential name the user declined.

### 9. Fixing today's identity check
Remove the host-global warning at `prepare.rs:469-495` and replace it with the `GitIdentity` prerequisite. Its probe checks what the container will actually see, and its remediation injects an identity. This fixes the false OK for users with only a global `~/.gitconfig`, which is today's most common `commit_changes` failure.

## Edge Case Considerations:
- **Dynamic, nothing ends up needing credentials**: all grants are dropped after validation, and the summary line reads "granted but unused: SSH key (not mounted)". The user is never charged for an assumption.
- **Dynamic leader needs a credential the user declined**: warn, and re-prompt if a human is present (the TUI is still attached in a yolo run, and the prompt doesn't block auto-advance for longer than the existing yolo countdown, after which it falls back to deny). Headless runs apply `unresolved`.
- **Passphrase-protected key**: the push in a headless container will fail. Detect it and say so. ssh-agent forwarding is out of scope: the builtin runtime refuses sockets by design (`SandboxSpec::check`), and Docker socket forwarding needs its own threat review. Note this as follow-up work.
- **Key file permissions / uid mismatch**: the container user may not be able to read a 0600 host key owned by another uid (Linux Docker without userns). The probe checks that the image's user is root or matches; otherwise warn and suggest the `dir()` overlay. Never chmod host files.
- **`~/.ssh/config` with `IdentityAgent`, `ProxyCommand` or `Include` pointing at host-only paths**: this is the reason the default is a single-key mount, not the whole directory.
- **`known_hosts` read-only**: with `accept-new` and a read-only `known_hosts`, ssh warns but continues. Use a writable temp copy so output stays clean.
- **Remote named in `push_branch.remote` doesn't exist**, or `origin` points at a local path or `file://`: `GitRemote` is `Missing` or `Other`, and no auth is needed for `file://`.
- **Non-GitHub HTTPS remote (GitLab, Gitea)**: awman can't supply a credential in this item. The report says so and suggests an explicit `env()`/`dir()` overlay. `create_pull_request` on a non-GitHub remote is flagged as unsupported.
- **GitHub Enterprise host**: classify by remote host. `gh auth token --hostname <host>` and `GET https://<host>/api/v3/user` are used for verification.
- **Token in both `GH_TOKEN` and `GITHUB_TOKEN` with different values**: use the first in `envVars` order and say which one.
- **Token expires mid-run** (GitHub App tokens last 1 h): `refresh: "beforeTeardown"` re-runs `acquire`. An env or pasted token can't be refreshed, and the pre-teardown check reports `Unknown` if verification fails.
- **Pasted token in TUI copy/scrollback**: the secret input is never echoed, never added to dialog history, and the buffer is zeroized on drop.
- **Host command hangs or prompts** (for example `op` waiting for biometrics): the timeout kills it. Interactive frontends give the command the TTY. Headless runs give it `/dev/null` stdin, so it fails fast.
- **Host command prints extra lines or a trailing newline**: only the first non-empty line is used. Multiple lines produce a warning.
- **Repo `.awman/config.json` sets `"overlays": ["ssh()"]`** (possible today, silent): this item doesn't change overlay semantics. The prerequisite summary lists credential-bearing overlays that came from repo config ("repo config mounts ~/.ssh into every step"), so the user can see them. Recommend a follow-up work item to put repo-sourced `ssh()` and secret `env()` overlays behind consent.
- **Worktree runs**: the identity probe must read the worktree's common git dir (`worktree_git_overlay`), not only the checkout. Running `git config --local` from the worktree writes the shared config, and the prompt says so.
- **Remote API sessions** (server-side clone): the remote URL comes from the session's `repo_url`. Host SSH keys belong to the server operator, so `ssh.use` defaults to `deny` for remote sessions unless global config allows it.
- **Image probe on builtin runtime (import-only)**: the probe must not trigger a build (`BuildPolicy::ImportOnly`). If the image isn't imported yet, the result is `Unknown`.
- **`--check-prerequisites` with `--dynamic`**: the leader isn't run. The report is for `WorkflowPrerequisites::assume_all()`.
- **Concurrent runs writing `prerequisite-grants.json`**: atomic rename plus last-writer-wins per key. Reads are bounded in size, as for `TrustStore`.
- **Squad task created before this item**: no `prerequisites` policy means `ask`, which headless resolves to deny. On the first run after upgrade, the daemon log and `squad show` say "workflow prerequisite policy not set; run `awman squad edit <task>` to configure". Today's behaviour, where explicit step overlays still work, is preserved.

## Test Considerations:
- **Unit (engine/data)**:
  - `required_for`: each teardown kind maps to the right prerequisites. `run_shell` gives only advisories. Dynamic gives `all()`. Narrowing after generation drops unused grants.
  - Remote URL classification: scp-style, `ssh://`, `https://` with and without embedded credentials (stripped in display), `file://`, GHE hosts.
  - SSH key header detection: unencrypted, PEM encrypted, OpenSSH bcrypt, unreadable, symlink escaping `~/.ssh` (refused).
  - Policy merge: a repo `allow` is ignored with a warning. A repo `deny` beats a global `allow`. A repo identity is allowed. Repo `acquire` needs approval. API request widening is refused unless `allowClientGrants`.
  - `HostSecretCommand`: argv-only (a shell metacharacter is passed literally), timeout kills the process group, agent binaries refused, output validation (empty, whitespace, over 8 KiB, multi-line), stderr scrubbed, minimal env asserted.
  - `PrerequisiteGrants` / `SecretString` `Debug` never contains the token.
  - Grant application: `commit_changes` gets identity env only, `push_branch` gets key mounts only, `create_pull_request` gets `GH_TOKEN` only, `run_shell` and remediation agents get nothing.
- **Integration**:
  - With a fake runtime, a static workflow with commit+push+PR and a fake env without a token: preflight reports `GitHubToken: Missing`. `unresolved: fail` stops before any container launch (assert zero `start_background` calls).
  - A token from a fake `acquire` command is present in the PR container's env, absent from the commit container's env, and redacted from captured output and the failure file.
  - SSH push to a local `sshd` test fixture (gated like other container tests by `AWMAN_TEST_DOCKER`) with a single-key mount works. Without a grant it fails with the preflight warning emitted earlier.
  - Identity: a repo with only a global identity yields `GitIdentity: NeedsConsent`. Choosing "host identity" makes `git commit` succeed in the container.
  - Dynamic: the preflight runs before the leader. The generated workflow without push means no SSH mount appears in any container argv.
- **Frontend / parity**:
  - Every new prompt's labels and defaults come from `prompts.rs`, with CLI vs TUI parity tests like the existing catalogue tests.
  - TUI render tests for the prerequisites modal and `SecretInput` (masked).
  - CLI tests: masked stdin read and the non-interactive flags.
  - `HeadlessDefaults` table tests for the new rows (api, squad, cli).
- **E2E**:
  - `awman exec workflow wf.toml --check-prerequisites` exit codes and output.
  - `--non-interactive --prerequisite-token env:GH_TOKEN`.
  - The API: `WorkflowPrerequisites` event comes first in SSE, a `workflow_prerequisites` failure stage, a usage error for `--prerequisite-token secret:NAME` before 0140, and refusal of a widening `--prerequisite-ssh` unless `workflows.prerequisites.allowApiClientGrants` is set. A parity test asserts the same `--prerequisite-*` flags parse identically from CLI args, the TUI command box and API `args`.
  - Architecture: the existing layer-dependency checks pass, and no `src/data` type contains a `SecretString`.
  - Squad: `squad add --prerequisite-ssh deny`, then `squad show` displays the policy, and the run log contains the summary.
- **Security regression**: a repo config with `workflows.prerequisites.ssh.use: "allow"` and an `acquire` argv never mounts keys or runs the command in API, squad or non-interactive CLI (assert no spawn). No token value appears in `events.jsonl`, daemon logs, `ps`-visible argv or `prerequisite-grants.json`.
- All tests run under `AWMAN_TEST_ISOLATION`. Network verification and real `gh` are faked.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Read `aspec/architecture/2026-grand-architecture.md` first. **Layering** is as tabulated in §0 and must not be changed without asking the developer:
  - no Layer 0 type may hold a `SecretString`;
  - no frontend computes prerequisites, prompt choices, defaults or policy;
  - grants become mounts only in `OverlayEngine`;
  - git calls happen only in `GitEngine`, and token sources only in `AuthEngine`;
  - engines never prompt; they return `WorkflowPrerequisiteReport` and accept `PrerequisiteGrants`.
- **Hook points**:
  - `exec_workflow/execute.rs::run_with_frontend` (static, before worktree prepare), `exec_workflow/dynamic.rs::run_dynamic` (before the leader) and `execute_generated_workflow` (narrowing).
  - `prepare.rs::execute_prepared`: replace the identity warning at :469; re-check before teardown at :662; apply grants in `collect_single_entry_overlays` / `resolve_phase_overlays`.
  - `engine/workflow/step_commands.rs`: `push_branch` `GIT_SSH_COMMAND` and the HTTPS helper; `create_pull_request` `GH_PROMPT_DISABLED`.
  - `engine/workflow/phases.rs::run_poll_ci_phase_step`: take the token from grants.
  - `launch_on_failure_agent`: assert grants are excluded.
- **Reuse rather than reinvent**:
  - `SecretString` (`engine/auth/credential.rs`), `TrustStore` and the `AgentTrustFrontend`/`HostCommandReview` approval UX (`command/commands/agent_trust.rs`).
  - `host_command.rs` for host git, `EnvSnapshot` for env lookup, `poll_ci::gh_is_available` (moved to a shared helper), `HeadlessDefaults` for headless answers.
  - The squad `env_sync` and `overlay_summary` machinery for daemon-side values and leader-prompt gaps.
  - `fields.rs` `CONFIG_FIELDS` for scope rules, and the `builtin_network.rs` "repo may only narrow" merge pattern.
- **Frontends**:
  - CLI `per_command/workflow_prerequisites.rs`.
  - TUI `per_command/workflow_prerequisites.rs`, a new `DialogRequest::SecretInput`, and routing in `dialog_router.rs`.
  - API `ApiDispatchFrontend`: renders the `WorkflowPrerequisites` event (`data/execution_event.rs`). It has no prompts and no policy logic, and `CreateCommandRequest` is unchanged (flags travel in `args`).
  - Squad `UnattendedFrontend`, the `task_store.rs` `Task.prerequisites` field, and `squad add/edit` flags in the catalogue.
- New flags go in the command catalogue with visibility data (cli.md "Frontend visibility and parity"). Failures map to existing exit-code classes: 2 for an unresolved required decision or a preflight `fail` policy, 130 for an abort from the prompt.
- **Specs to update** alongside the code:
  - `aspec/architecture/security.md`: add the `HostSecretCommand` host-execution exception, the agent-binary refusal, the grant scoping rules and the repo-cannot-grant rule.
  - `aspec/uxui/cli.md`: the new flags.
- **Coordinate with**:
  - WI 0140 (per-session secrets, git credential helper, redaction): share one redaction implementation and switch the HTTPS push helper to 0140's socket helper when it lands.
  - WI 0146/0147 (daemon modes, worker): `PrerequisitePolicy` is mode-agnostic and reusable.
- Update `templates/Dockerfile.project` to install `openssh-client` and `gh` by default (developer decision), so a new project satisfies the `ContainerTool` checks out of the box. Existing projects are not modified; they get the preflight warning with the exact line to add.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.

Specifically:
- `docs/05-workflows.md`: workflow prerequisite checks, what each teardown step needs, `--check-prerequisites`, and the updated identity behaviour. Remove the "add env(GITHUB_TOKEN)/ssh() yourself" guidance where awman now supplies credentials.
- `docs/06-dynamic-workflows.md`: checks run before the leader, assume-all, and narrowing after generation.
- `docs/07-configuration.md`: the `workflows.prerequisites` block, the repo-level safety rules, `acquire`, and `prerequisite-grants.json`.
- `docs/04-security-and-isolation.md`: grant scoping, the host token command, and how SSH keys are mounted. Correct the "no config option to enable it silently" sentence about `~/.ssh`.
- `docs/09-api-and-remote-mode.md`: the `--prerequisite-*` flags over the API, the `WorkflowPrerequisites` event, and the `workflow_prerequisites` error stage.
- `docs/12-squad.md`: task prerequisite policy and daemon-side token resolution.
- `docs/10-github-integration.md`: token sources for PRs and CI.
- `docs/14-command-reference.md`: the new flags.

## Security Review

### Plan review

Required model: **gpt-6-astra**. **Review not delivered; mandatory gate remains open.**
The supplied `plan-security-resolutions.md` records BLOCKED because
`plan-security-review.md` was absent. It contains no PSR-n identifiers or
resolutions. This is not a “no findings” review, and no model provenance or
accepted-PSR implementation trace is claimed. The intended PSR ledger is:

| Finding | Resolution |
|---|---|
| No PSR-n findings delivered | Recover or rerun the mandated-class plan review, resolve each numbered finding and trace accepted requirements before sign-off. The pre-implementation timing requirement was missed and cannot be retroactively claimed. |

### Implementation review

Model: **gpt-6-astra**, with an additional adversarial review by
**claude-fable-5-1** (the reviewers identified by this workflow). Dispositions
below refer to the supplied ISR/AR artifacts and the review-fixes changes.
The detailed regression ledger and validation logs are retained in
`/awman/context/workflow/fixes-log.md`.

| Finding | Resolution |
|---|---|
| ISR-1 | Fixed in `src/engine/auth/host_secret_command.rs`, `github_token.rs`, command flow and CI polling: canonical/content identity refusal, interpreter/launcher refusal, repository exclusion and custom-agent propagation. Exact installed-executable approval is a trust decision; arbitrary transitive program semantics cannot be classified from a basename or digest. That limitation is explicit in the security specification. |
| ISR-2 | Fixed in `src/engine/auth/host_secret_command.rs` and command flow: approval binds argv, canonical path and executable content; execution uses a verified private snapshot. Symlink and file-replacement regressions added. |
| ISR-3 | Fixed in `src/engine/auth/host_secret_command.rs`: retain process-group identity after reaping, kill remaining descendants, restore foreground terminal ownership and clean up on cancellation. Unsupported host-command platforms fail closed. |
| ISR-4 | Fixed in `src/engine/container/{options,phase_step,background,docker,builtin/backend}.rs` and phase launch wiring: explicit secret-valued overrides survive every backend and take precedence over ambient values. Values stay out of argv. |
| ISR-5 | Fixed in phase-container selection and engine redaction: credentialed execution uses filtered streams, including CLI/TUI, instead of a raw PTY/attach stream. Stdout, stderr, returned output and failure files remain redacted. |
| ISR-6 | Fixed in grants, flow, probes, overlays and CI polling: exact destination and credential-kind scope, actual-remote remembered keys, generated-destination reauthorization, scoped CI token selection and per-poll destination checks. |
| ISR-7 | Fixed in `src/engine/git/remote_push.rs`, `src/data/fs/git_metadata.rs`, overlays, squad evaluation and step commands: private inert metadata, descriptor-relative link refusal, pre-agent metadata-path capture, pinned destination, disabled checkout hooks/helpers/rewrites and controlled SSH transport. Config parsing uses a private no-follow snapshot. Rewritten `.git`/`commondir` cannot redirect staging or publication; squad rejects a changed layout after every leader/repair attempt. PR repository/head are explicit. |
| ISR-8 | Fixed in squad command admission, route frontend identity, policy provenance and evaluation: the operator ceiling applies to remote requests and is rechecked for remotely stored task decisions. Real squad-route regression added. |
| ISR-9 | Fixed in `src/data/fs/{prerequisite_grants,agent_trust,private_files}.rs`: lock/reload/per-key mutation/atomic publication; approval reads validate the opened descriptor. Stale writers cannot resurrect revoked decisions. |
| ISR-10 | Fixed in host capture and shared redaction: bounded pipes, overflow termination, withheld unsafe error diagnostics and registration of every nonempty accepted secret. |
| ISR-11 | Fixed in launch carriers and CLI/TUI secret input: SecretString-valued overrides until runtime environment construction, redacted launch Debug, bounded input from the first keystroke and wiping on deletion/error/drop. |
| ISR-12 | Fixed in `src/data/config/workflows.rs`: repository unresolved warn cannot weaken the implicit or explicit headless fail policy. |
| ISR-13 | Fixed in the Layer 0 resolver and task-policy editing: sparse explicit credential overrides retain unrelated global policy and acquisition commands; daemon defaults apply without a stored task policy. |
| ISR-14 | Fixed in setup transport builders and controlled metadata: pull/clone use the selected key/helper and pinned destination; pull synchronizes only inert Git state. |
| ISR-15 | Fixed in `src/engine/workflow/step_commands.rs`: valid printf credential records, correct shell quoting and exact protocol/host/path restrictions, tested with real `git credential fill`. |
| ISR-16 | Fixed in TUI key routing, prompt handling and bounded yolo presentation: Ctrl-C discards secrets, summary Ctrl-C aborts, and safe defaults remain command-owned. Ignored regression enabled. |
| ISR-17 | Fixed in host derivation, consent, overlays and polling: GitHub Enterprise uses host-specific gh/API endpoints. General github.com allow policy never sends tokens to a new repo-selected host before explicit or destination-remembered authorization. |
| ISR-18 | Documentation fixed here; required plan review remains OPEN. No missing PSR trace or historical pre-code review is represented as completed. The real-diff review also remains necessary because this checkout's Git pointer references unavailable host metadata. |
| AR-1 | Fixed: squad/generated runs use the assumed-needs unresolved default, while explicit fail policies remain effective. |
| AR-2 | Fixed: repo/task layers retain the trusted global acquire command; regression covers sparse SSH and acquire task choices. |
| AR-3 | Required review record added; missing plan-review gate explicitly remains open, as ISR-18. |
| AR-4 | Fixed: command-dialog Ctrl-C is routed before global tab closing; secret input is discarded and summary input interrupts. |
| AR-5 | Fixed: terminal release uses an unwind guard and tracing follows TUI_ACTIVE during release/restore. |
| AR-6 | Fixed: yolo questions render and expire with the existing countdown, instead of the frontend immediately choosing an answer. |
| AR-7 | Fixed with ISR-17's Enterprise authorization and host support. |
| AR-8 | Fixed: explicit squad context applies daemon defaults, and task-directory workflow policy cannot become trusted global policy. |
| AR-9 | Fixed: Dispatch validates credential flags before API enqueue and supplies the resolved, secret-free policy for flags_applied. Both ignored API tests enabled. |
| AR-10 | Fixed: workflow JSON flag implies non-interactive mode and serializes the complete outcome/report. Command reference regenerated and ignored binary regression enabled. |
| AR-11 | Fixed: check-only exits 0 only for all Satisfied; consent, missing and unknown checks exit 1. |
| AR-12 | Fixed: rejected tokens require replacement or remain Missing, and cannot be silently regranted from the rejected source. |
| AR-13 | Fixed: the shared command report prompt supplies labels/hotkeys/defaults to CLI and TUI; summary Ctrl-C/EOF aborts consistently. |
| AR-14 | Fixed: every PR target disables gh prompts, including without a token grant; the unused command option was removed. |
| AR-15 | Concrete regressions fixed: all ignored bug tests enabled, non-vacuous dynamic grant narrowing, real squad route gate, stream/SSE ordering and redaction, native Docker SSH push/clone and identity commit fixture. Rejected the broader inference that every invariant needs another binary-level duplicate: the typed §0 boundaries and focused integration tests remain authoritative. Native Docker execution is a separate opt-in gate, not claimed by test-fast. |
| AR-16 | Docs/specs/examples corrected. Rejected inserting command-specific syntax into `aspec/uxui/cli.md`: developer decision Q9 and its projection test reserve that document for general UX standards; syntax belongs in the generated reference and user guides. |
| AR-17 | Fixed: requested probe tools reach CLI argv, incomplete cache entries miss, and only the runtime cache remains. |
| AR-18 | Fixed: missing configured keys and unusable remembered keys produce an explicit warning and grant nothing. |
| AR-19 | Fixed by locked per-key grant-store updates, with interleaved revoke/remember regression. |
| AR-20 | Fixed: planned worktree context satisfies attached-HEAD checks before creation. |
| AR-21 | Shared report redaction and duplicate warning behavior fixed. Rejected the broad structural rewrite as a separate correctness requirement: §0 places this flow in Layer 2 and aspec sets no numeric file/function limit; further restructuring would enlarge this security fix unnecessarily. |
| AR-22 | Fixed: bounded secret input is protected from the first keystroke; read errors decline instead of advertising an invalid flag. |
| AR-23 | Rejected: WI §1/§3 require checks before the untrusted leader and recomputation after generated-workflow validation. Reusing the earlier report would miss destination changes; tool metadata alone is safely cached. |

Recommended follow-up work items:

- Threat review for SSH-agent forwarding and encrypted-key interaction; no agent socket forwarding is introduced here.
- Consent for repo-sourced `ssh()` and secret `env()` overlays, which remain a separate broader authorization surface under the existing developer decision.
- WI 0140 coordination: switch the HTTPS helper to its socket credential helper and reuse the same per-run RedactionSet.

Review-fixes validation passed: `cargo build --workspace`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`,
`bash tools/architecture-lint.sh` and `make test-fast`. `make docs-reference`
also passed after the catalogue change. Native Docker fixtures remain opt-in;
their execution is not claimed by test-fast.

The remaining sign-off gates are the missing mandated-class plan review,
resolution of any recovered PSR findings, and a complete real-diff audit when
the original Git metadata is accessible. The verify-fixes step must independently
check the dispositions against the supplied reviews.
