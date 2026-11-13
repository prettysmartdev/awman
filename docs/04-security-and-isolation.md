# Security & Isolation

awman is built around a simple principle: agents never run on your host machine. Every agent session runs inside an isolated environment — a container or microVM — that only sees what you explicitly give it. This section explains the isolation mechanisms and when to opt into elevated access.

---

## The containment model

The browser frontend is intended for same-machine access through the daemon's
loopback listener. The v0 web app cannot connect to a daemon on another
machine. Browser origins are configured per listener with
`daemon.network.listeners.<name>.allowedOrigins`; this is an origin allowlist,
not itself authentication. A browser must also be approved with `awman link
web-<code>`; the resulting grant can be listed or revoked through the local
daemon API. Grants last twelve hours; a reload within that time reconnects
without a new approval. See the [Web App guide](20-webapp.md) for setup,
when relinking is needed, and usage reporting.

The owner decides how much the loopback listener exposes. Browser grants are
scoped to the engine on that one listener; they can never create API keys,
change configuration or reach worker routes, and the browser cannot supply its
own shell environment, executable paths or mount roots. If you suspect a browser
or the hosted site has been compromised, revoke its grants with
`awman daemon api browsers.delete id=<browser-id>`, then disable the listener
(`awman config set daemon.network.listeners.loopback.enabled false`) or remove
`engine` from its `features`. Both changes apply live and close that listener's
browser connections.

By default, an agent environment:

- Sees only your current Git repository (mounted via bind mount or virtiofs)
- Receives your credentials through a secure per-session channel (never exposed to other sessions)
- Has no access to your home directory, SSH keys, or host Docker daemon
- Is stopped or removed when the session ends

This means a misbehaving agent can't access your SSH keys, can't run arbitrary containers on your behalf, and can't touch files outside the project. The worst case is that it makes bad edits inside the repo — which git can undo.

**Docker / Apple Containers:** agents run in a Linux container or lightweight VM respectively. The container is removed (`--rm`) when the session ends. Most agents receive credentials as environment variables. Claude Code is the exception: it receives a live-updated credential file (access token, expiry, and scopes only — never the refresh token) written into its staged settings directory and kept fresh for as long as the session runs. See [Live credential refresh](#live-credential-refresh) below.

**Docker Sandboxes (`sbx`):** agents run in a dedicated microVM with its own kernel, private Docker daemon, and private filesystem. Host escape requires a hypervisor exploit rather than a container escape. Sandboxes persist between sessions (state survives `sbx stop`); awman runs `sbx rm` only on explicit teardown. Credentials are registered at agent launch with sandbox-scoped `sbx secret set` calls (never global), so removing a sandbox removes its secrets with it. sbx is unaffected by the live credential-file refresh described below — it continues to authenticate Claude only through `ANTHROPIC_API_KEY` or an in-sandbox login. See [Runtimes](11-runtimes.md#docker-sandboxes-experimental) for setup and limitations.

**Builtin microVM (`builtin`, macOS on Apple Silicon only):** agents run in a per-session microVM whose hypervisor, kernel, and guest agent are embedded in the awman executable. The builtin runtime does not use a host runtime helper to start or manage VMs; the separate `docker` or `container` CLI is used only for an explicitly enabled, source-matched image build during `awman ready` or when a launch finds its agent image missing. That build uses the configured local image source and repository Dockerfiles/build context; it does not move agent execution out of the microVM. Environment overrides that could replace the builtin runtime are rejected; the error identifies the variable to unset. The VM is started by a separate awman process running the same executable, which is what holds the hypervisor access (the `com.apple.security.hypervisor` entitlement carried by awman's ad-hoc code signature); the internal `awman machine` entry point that process uses is reserved and not a public command. Sessions are removed when they end, and if the awman process that launched a session exits, the session's VM stops with it. Its private state directory must be owned by you (mode `0700`, no symlinks). Every mount into a session, including any overlay, is checked before creation and refused if it isn't a regular file or a directory — a Unix socket (such as the Docker socket), FIFO, or device node can never be bridged into the guest that way. Images are imported only from a source you configure, verified against their digests, and rejected if their layers contain path traversal, writes through symlinks, device nodes, or unsafe hard links; awman never runs Docker credential helpers, and registry credentials go only to the registry itself. Claude credentials use the same staged, refresh-token-free, mode-`0600` file as the container runtimes, delivered as a single-file mount. Agents run as the image's non-root user, which reads that file inside the guest, and a live credential refresh reaches every running session atomically.

Guest network policy is configured through `runtime.builtin.network`; the repo may only narrow the global ceiling. The policy is resolved per launch from the repo the session runs in: the repo block narrows the policy for every launch in that repo (TUI, API and squad sessions, and workflow worktrees, which keep the narrowing of the checkout you launched from even before you commit it), and no launch can widen the global ceiling. Every session reads the global ceiling from the same global config the awman process loaded, including one relocated with `AWMAN_CONFIG_HOME`. The host-side network stack enforces it without TLS interception or ambient proxies: allowlisted HTTPS is permitted only for an allowed, DNS-bound server name, hostname rules deny UDP/QUIC, and `none` gives the guest no network device. These rules are tested in real guests; see [Runtimes: Network](11-runtimes.md#network) for exactly what is and isn't inspected (encrypted HTTP `Host` and ECH inner names are not).

See [Runtimes](11-runtimes.md#builtin-microvm-experimental).

### Terminal output and host terminal variables

When agent output is attached to a host terminal, awman filters terminal
control sequences before writing it to the host. Printable text, ordinary
cursor and colour controls (CSI), standard escape sequences, and OSC titles,
hyperlinks, and icon names (OSC 0, 1, 2, and 8) pass through. Other OSC
sequences are removed, including clipboard access (OSC 52), file transfer,
palette changes, and terminal notifications (OSC 9, 99, and 777). OSC 7501
program-status reports are removed or parsed and re-encoded from sanitised
fields; raw reports and terminal queries are never passed through. DCS, SOS,
PM, and APC strings are removed in full, including Kitty graphics. tmux
passthrough wrappers are removed and their contents are scanned again by the
same filter. Incomplete control strings are dropped at detach. This
filter applies to TTY attach output; the TUI's in-process terminal display is
separate.

Containers do not receive these host-terminal variables, even when an overlay
asks to pass one through: `REX_*`, `TMUX`, `TMUX_*`, `ZELLIJ`, `ZELLIJ_*`,
`STY`, `WINDOW`, `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`, `TERM_SESSION_ID`,
`WEZTERM_*`, `KITTY_*`, `ITERM_*`, `LC_TERMINAL*`, `ALACRITTY_*`, `GHOSTTY_*`,
`VTE_VERSION`, `WT_SESSION`, and `WT_PROFILE_ID`. awman warns when it removes
requested variables. Inside every container, `TERM` is `xterm-256color`;
`COLORTERM` is `truecolor` only when the host had `COLORTERM` set. If an agent
needs one of the blocked values, copy it into a differently named host
variable before launching awman, then pass that new variable with an `env()`
overlay.

### Live credential refresh

Claude Code's OAuth refresh token — the credential that could mint new access tokens indefinitely — never leaves your host and is never given to a container. Instead, for each session awman writes a sanitized, awman-authored `.credentials.json` (access token, expiry, and scopes only, mode `0600`) into the staged `~/.claude` directory that's bind-mounted into the container; the host's own `~/.claude/.credentials.json` is excluded from that mount entirely.

While a Claude session is running, awman's credential-refresh monitor watches the access token's expiry. Shortly before it would expire, the monitor triggers a host-side refresh: it invokes the same sanctioned, hardcoded ready-check ping that `awman ready` uses to verify your local agent is authenticated (fixed prompt, no user input, no repository content, no working directory) — this causes your host's Claude Code installation to rotate its own Keychain entry. The monitor then atomically replaces the staged credential file for every live session with the new token, so a long-running container observes it on its next request without a restart.

If the host can't refresh (asleep, logged out, offline), awman keeps the last-known-good token in place, warns loudly, and retries with backoff; it never leaves a container with no credential at all. This behavior is on by default and can be tuned or disabled — see [Control credential refresh](07-configuration.md#control-credential-refresh-agentsauthrefresh).

Every runtime command awman runs is printed before it executes; see [Command transparency](#command-transparency).

---

## Custom agent definitions

[Custom agents](15-custom-agents.md) are defined by TOML files, so awman treats them as untrusted input until you approve them:

- **Approval per content hash.** Definitions in a repo's `.awman/agents/` must be approved before awman builds or launches them. The approval covers the TOML and any Dockerfile it references, and is stored in `~/.awman/trusted-agents.json`; changing either file asks again. Global definitions in `~/.awman/agents/` are your own and need no approval unless they declare `[ping]`. Unattended callers (`-n`, `--json`, the API, the squad daemon) never prompt and fail instead — see [The approval prompt](15-custom-agents.md#the-approval-prompt).
- **Containers only.** Agents run in the same containers and microVMs as built-in agents. The one host-side command a definition can request is the optional `[ping]` check during `awman ready`; it is shown in the approval prompt, runs only after you approve, from an empty temporary directory, with an awman-chosen greeting as the last argument. Without `[ping]`, `ready` skips the host check. See [The optional host check](15-custom-agents.md#the-optional-host-check-ping).
- **No shell.** Commands are argv arrays and install steps are written into the image build in a form that cannot run outside their own `RUN` step.
- **Host-path deny-list.** A definition cannot mount `.ssh`, `.gnupg`, `.aws`, `.azure`, `.config/gcloud`, `.kube`, `.docker`, `.awman`, `.netrc`, `.git-credentials`, `.config/gh`, `.password-store`, `.local/share/keyrings` or `Library/Keychains` — or any directory above, inside, or symlinked onto them — as its settings directory. The same holds for awman's actual data directory and the real targets of these directories when they are themselves relocated by a symlink.
- **Your Dockerfile, your responsibility.** If you [take ownership](15-custom-agents.md#taking-ownership-of-a-rendered-dockerfile) of a rendered Dockerfile by removing its header line, awman builds it as written and no longer checks it against the approved definition.

---

## Worktree isolation

The `--worktree` flag on `exec workflow` runs agent sessions in an isolated Git worktree rather than your main working directory. `chat` and `exec prompt` can use worktrees too; see [Session worktrees](#session-worktrees). The agent's changes land on a separate branch, completely isolated from your current work until you decide what to do with them.

```sh
awman exec workflow path/to/workflow.toml --worktree
```

### Why use it

- The agent can make sweeping changes without your working branch becoming unstable mid-implementation
- You can review the full diff as a coherent unit before it touches your main tree
- If the output isn't useful, discard it with a single keypress — no `git reset` needed
- Works with `exec workflow`: all steps in the workflow share the same isolated worktree

### How it works

1. awman creates a branch from your current `HEAD`: `awman/work-item-NNNN` for a work item, `awman/workflow-<name>` for a named workflow
2. A worktree is checked out under `~/.awman/worktrees/<repo-name>-<hash>/` (`<NNNN>/` or `wf-<name>/`; `<hash>` is 8 hex characters derived from the repository's full path, so two checkouts with the same folder name never share a worktree)
3. The agent container mounts the worktree instead of your repo root
4. When the run ends, you choose what to do with the branch

### After the run

When a worktree run completes or is aborted, the worktree is kept on disk and awman asks what to do with it:

| Key | Action |
|-----|--------|
| `m` | Merge into the branch you started from, then pick a mode: `m` merge (no squash), `s` squash into one `Implement <branch>` commit, or `l` leave the branch alone |
| `d` | Discard — remove the worktree and delete the branch |
| `k` | Keep the worktree and branch for manual review |

An aborted run is never deleted automatically. If the merge fails, the worktree is kept and awman can launch a merge-fix agent. An interrupted run's worktree can be resumed or recreated (recreate deletes the branch and its commits). Dirty merge targets, merge failures and existing worktrees each have a flag to answer up front; see [Worktrees](05-workflows.md#worktrees) and [Worktree merge-back](05-workflows.md#worktree-merge-back).

A `checkout_create_branch` setup step is skipped with a warning in a worktree run, since the run already has its own branch.

### Session worktrees

`chat` and `exec prompt` can also run in a worktree with `--branch`, `--pr` or `--worktree <name|path>` (see [Agent Sessions](03-agent-sessions.md#--branch---pr-and---worktree)). The agent's changes land in a linked worktree of your repository under `~/.awman/worktrees/`, not in your checkout. Unlike a workflow worktree, awman keeps it afterwards: there is no merge or discard prompt, and you remove it yourself with `git worktree remove`.

- **What is mounted.** The container gets the worktree plus the repository's main `.git` directory, because a linked worktree stores its history there. Nothing else from your checkout is mounted, and never a parent directory. A checkout that isn't a real linked worktree of this repository (for example a path you point `--worktree` at that belongs to another repository) is refused.
- **Your checkout's configuration applies.** Repo config, trust decisions for custom agents, network policy narrowing and overlays all come from the checkout you ran the command in, in full. The config inside the worktree is never read, so a branch or pull request cannot change how its own session is launched.
- **Local only.** These flags are accepted on the CLI and in the TUI. The HTTP API, including remote and worker routes, rejects them, so a remote caller cannot choose a host directory to mount.
- **Remote access from the host.** To check and fetch a branch or pull request, awman runs `git ls-remote` and `git fetch` on the host against your repository. Because an agent can write your repository's `.git/config`, awman refuses to contact the remote when that repository-level (local or worktree) config sets a program or credential source for transport: `core.sshCommand`, `core.gitProxy`, `core.askPass`, a `credential.helper`, `remote.<name>.vcs`, `uploadpack`, `receivepack` or `proxy`, or an `http` proxy, TLS certificate/key/CA, `sslVerify`, cookie file or resolve override. The error names the key; remove it, or move it to your global Git config, which awman trusts. SSH runs in batch mode so it can never prompt, and each network call is limited to five minutes.
- **Git content drivers.** Host-side Git never runs LFS or other configured filters, so files stored with Git LFS or a similar filter may be checked out as pointer files in a new worktree. Fetch the real content yourself (for example `git lfs pull` inside the worktree) before starting the session.

### Host-side Git

Git commands awman runs on the host (creating worktrees, merging back, committing) never execute repository-supplied code: hooks, fsmonitor, external diff/merge drivers and content filters are disabled, and so are commit, tag and push signing and automatic maintenance. Commits awman makes on the host are therefore unsigned; sign them yourself afterwards if your branch policy requires it. Git commands an agent runs inside its container use the repository's normal configuration.

### Edge cases

| Situation | Behaviour |
|-----------|-----------|
| `git` < 2.5 | Error before launch: "git >= 2.5 is required for --worktree support" |
| Detached HEAD | Warning printed; worktree created from current commit; continues |
| Branch exists, no worktree dir | Worktree created using the existing branch |
| Merge conflict | Error with manual resolution instructions; worktree kept |
| Combined with `exec workflow` | All workflow-step containers share the same worktree |
| Combined with `--overlay ssh()` | Both flags apply independently |

### Examples

```sh
awman exec workflow path/to/workflow.toml --worktree                              # isolated run; prompt to merge after
awman exec workflow path/to/workflow.toml --worktree --overlay "ssh()"            # worktree + SSH keys in container
```

---

## Overlay mounts

Overlays extend the base isolation model: they are the *only* supported way to give a container anything beyond the project mount. There are five kinds — `dir()`, `env()`, `skill()`, `ssh()`, and `context()` — and they can come from global config, per-repo config, `AWMAN_OVERLAYS`, `--overlay` flags, or an individual workflow step. [Overlays](08-overlays.md) is the full reference for the syntax, the five sources, and the merge and conflict rules; this section covers only what they mean for security.

**What an overlay can and cannot reach.** With overlays in play, the agent can access your Git repo **plus exactly the listed overlay directories, variables, and skills** — nothing more. There is no wildcard that exposes the host filesystem, and no overlay type that grants a shell on the host.

**Read-only by default.** `dir()` mounts default to `:ro` when no permission is given. Only use `:rw` when the agent genuinely has to write there, and only with agent images you trust. Skills overlays are **always** mounted read-only regardless of source, so an agent can never modify a skill file.

**Missing paths fail closed.** If an overlay's host path does not exist at launch, awman stops before starting any container, rather than letting the runtime create an empty mount source:

```
overlay host path '/data/reference' does not exist (resolved to '/data/reference')
```

**Everything is printed.** Like `ssh()` and `--allow-docker`, every overlay mount appears in the runtime command awman prints before it executes, so you can always see exactly what a container was given. Values of `env()` overlays are masked as `***`.

Two overlay types have host-access implications significant enough to document on their own: [SSH key access](#ssh-key-access) below, and [context overlays](08-overlays.md#context-overlays-in-depth), which mount a writable directory shared across sessions.

---

## Docker socket access

The `--allow-docker` flag mounts the host Docker daemon socket into the agent container. This lets the agent build and run Docker containers itself.

### When to use it

Use `--allow-docker` when the task requires the agent to:

- Build Docker images (e.g. testing your app's Dockerfile)
- Run Docker containers (e.g. starting a local database for testing)
- Interact with the Docker daemon in any other way

### What happens

The socket mount appears in the printed `docker run` command:

| Platform | Mount |
|----------|-------|
| Linux / macOS | `-v /var/run/docker.sock:/var/run/docker.sock`, plus `--group-add <gid>` of the host's `docker` group when it exists |
| Windows | `--mount type=npipe,source=\\.\pipe\docker_engine,target=\\.\pipe\docker_engine` |

Under Docker Sandboxes the flag is ignored: each sandbox already has its own private Docker daemon.

### Security note

Mounting the Docker socket gives the agent root-equivalent access to your host — it can start containers, delete images, and interact with any running container. Only use `--allow-docker` for tasks that genuinely require it and when you trust the agent and work item. awman will never mount the socket without this explicit flag.

**Not available under the builtin runtime.** The builtin runtime rejects `--allow-docker` (`Docker socket bridge is not supported on the builtin runtime`): a Unix socket is never mounted into the VM and there is no bridge to a host daemon. Agents that must build or run containers need the Docker runtime. As defense in depth, every mount the builtin runtime creates (including overlays) is checked before the VM starts: only regular files and directories can be shared into a guest, so a socket, FIFO, or device node can never be bridged that way even by mistake.

### Examples

```sh
awman exec workflow path/to/workflow.toml --allow-docker  # workflow that needs to build a Docker image
awman chat --allow-docker                               # freeform session with Docker access
awman ready --refresh --allow-docker                    # Dockerfile audit with Docker access
```

---

## SSH key access

Use the `ssh()` overlay to mount your host `~/.ssh` directory read-only into the container, so the agent can authenticate with remote Git servers using your existing SSH keys.

### When to use it

Use `--overlay ssh()` when the task requires the agent to:

- Clone private repositories over SSH
- Push branches or tags to a remote
- Run `git fetch` / `git pull` against SSH remotes

### What happens

awman checks that `~/.ssh` exists (launch fails if it doesn't) and mounts it read-only at `.ssh` in the image user's home, for example `-v /home/user/.ssh:/home/awman/.ssh:ro`, which appears in the printed command. The agent can use your keys but cannot modify them.

An `ssh()` overlay deliberately mounts the whole directory when you configure it in an overlay source. Workflow prerequisite checks also support a narrower, read-only mount of a selected key after you choose it, or the whole directory after you explicitly choose that option. A repository's `workflows.prerequisites.ssh.key` is only a suggested key; repository config cannot grant SSH access by itself. See [Workflow prerequisites](05-workflows.md#workflow-prerequisite-checks).

### Security notes

- SSH key permissions must be correct on the host (`600` for private keys); Docker bind mounts inherit host permissions
- Only use the `ssh()` overlay with agent images you trust

### Examples

```sh
awman exec workflow path/to/workflow.toml --overlay "ssh()"              # agent can push/pull over SSH
awman chat --overlay "ssh()"                                             # freeform session with SSH access
awman exec workflow path/to/workflow.toml --worktree --overlay "ssh()"   # combine with worktree isolation
```

When used with a workflow, the SSH directory is mounted into every workflow-step container. The workflow prerequisite flow instead scopes its selected key mount to the steps that need SSH access.

## Workflow credentials

Workflow credential grants are scoped to the current run, repository, remote, credential type, and the setup or teardown steps that need them. A selected SSH key is mounted read-only only for the relevant step containers. A granted GitHub token is passed only to the relevant PR, HTTPS-push, or API-polling step. awman redacts granted tokens from workflow output. Remembered choices store decisions only in `~/.awman/prerequisite-grants.json`; they do not store keys or token values, and each decision is limited to its repository, remote, and credential type.

Grants never reach `run_shell` or `run_script` steps, main agent steps, `on_failure` remediation agents, or dynamic and squad leaders. A selected SSH key is mounted read-only as `~/.ssh/awman_push_key` with `IdentitiesOnly=yes` and a private writable copy of `known_hosts`. Credentialed Git transport pins the approved URL and ignores checkout-supplied hooks, URL rewrites, credential helpers and SSH commands; HTTPS credentials are answered only for the approved host and path. A new destination needs a fresh decision.

If you configure `workflows.prerequisites.githubToken.acquire.argv`, awman may run that token command on the host. It runs an argv list without a shell, with a timeout and bounded output, and the process group is cleaned up on every exit. The tool must be an installed program outside any Git checkout; known agent binaries (including configured custom agents), shells, interpreters and launch wrappers are refused. Approval binds the argv, the resolved executable and its content, and awman runs a verified private copy, so changing the installed program requires a new approval. A repository-configured command is approved by its argument-list hash before use. Approving a tool also trusts whatever it runs, so never approve one that delegates to an agent. The token is held as a secret, never written to config or logs, and redacted from all workflow output. See [Workflow prerequisites](05-workflows.md#workflow-prerequisite-checks) and [Configuration](07-configuration.md#workflow-prerequisites).

### API command secrets

An API command may include a `secrets` map for values requested by an
`env(NAME)` overlay. The map is limited to 32 entries; each non-empty value is
at most 8192 bytes and contains no NUL. Names are at most 128 bytes and must
match `^[A-Z_][A-Z0-9_]*$`. A command secret overrides the daemon's host
environment for that matching overlay variable. It is delivered only when the
overlay asks for that name; repositories and workflows that request an
environment variable are trusted recipients of its value.

Command secret values are held in daemon memory, not command rows or
idempotency records. A daemon restart drops them. A queued command that still
has a names-only marker but no in-memory values fails before dispatch with
`secrets_lost`; submit it again with the values. awman redacts registered
values and common encodings from command events, logs, and structured results,
including JSON data and map keys. This protects awman-published text and
records; it does not sanitize arbitrary artifacts. Files in `/awman/output`
and terminal bytes are not redacted. Do not write secrets to output files or
print them to a terminal.

Values passed through `env(NAME)` become part of the receiving process or
container environment. The receiving runtime and its operators can inspect
that environment, so environment delivery is not a boundary against a trusted
recipient or runtime administrator. Exact-value redaction also cannot prevent
deliberate exfiltration or every novel encoding. Do not treat log redaction as
a substitute for limiting which command and workflow receives a secret.

Remote API sessions may also supply `git_auth` for Git clone, fetch and
push-back. Those credentials remain in daemon memory and are served to Git via
a private local credential channel; they are not placed in Git process arguments or
environment variables. Git workflow steps receive a temporary, memory-only
credential cache scoped to the session's repository. This works across container
and microVM boundaries without mounting a host socket. Matching push and pull
steps also use the session credential during preflight. PR creation uses it for
Git operations and requires a separate GitHub API token. A phase container that receives the credential is trusted
with the reusable credential. After daemon restart, the credential is lost and
the session fails closed with `credentials_lost`; create a new session and
provide credentials again.

---

## The daemon engine

When the TUI runs on the [daemon engine](19-engine-and-frontends.md), nothing about containment changes: agents still run only in containers, with the same mounts and checks.

- **Same-machine browser access.** The TUI engine uses the owner-only local
  socket or named pipe. Browser access is designed for a separate loopback
  listener and cannot connect to a daemon on another machine. Browser grants
  are approved by the owner on the daemon's local CLI and can be revoked
  through the local daemon API; see the [Web App guide](20-webapp.md).
- **Shell environment.** So that commands behave as they would in your own shell, the TUI sends its environment to the engine when it connects. The engine keeps it in memory for that instance only; it is never written to disk, config, the database or logs, and it is dropped when the instance ends. It is not passed wholesale to containers: containers receive only what they would receive from a builtin TUI.
- **Secrets.** Secret dialog answers (such as a token you type) are sent only over the local socket and are redacted everywhere else.

---

## Command transparency

Every command awman issues to the underlying runtime is printed before it executes — in command mode to stdout, in TUI mode as the first line of the execution window.

```
$ docker build -t awman-myapp:latest -f Dockerfile.dev /path/to/repo
$ docker run --rm -it \
    -v /path/to/repo:/workspace \
    -w /workspace \
    -v /tmp/awman-claude-dir-a1b2c3/.claude:/home/awman/.claude \
    -e GEMINI_API_KEY \
    awman-myapp-claude:latest claude "Implement work item 0001..."
```

With the Apple Containers runtime, the same commands are shown with `container` instead of `docker`. With the Docker Sandboxes runtime, every `sbx` invocation is announced — `sbx create`, `sbx run`, `sbx exec`, `sbx stop`, `sbx rm`, `sbx secret set`, `sbx kit validate` — without sensitive values:

```
Running: sbx create --kit ~/.awman/kits/claude --name awman-ab12-claude claude /home/user/myproject
Running: sbx secret set awman-ab12-claude anthropic (value piped via stdin)
Running: sbx run awman-ab12-claude
```

Credential values are never printed. Env-delivered agent credentials render as a bare `-e VAR_NAME`: the value is passed from awman's own environment to the container CLI process, never through argv. Claude's OAuth credential is a file (see [Live credential refresh](#live-credential-refresh)), so only the staged directory's mount path appears. Variables you configure yourself (`env()` overlays, `envPassthrough`) render as `VAR_NAME=***`. Everything else is shown in full.

---

[← Agent Sessions](03-agent-sessions.md) · [Next: Workflows →](05-workflows.md)
