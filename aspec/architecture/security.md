# Security

## Guidance:

- Never directly execute any code assistant on the developer host machine. Every single code agent action should be executed by running a Docker image that has the agent tool installed and passing an entrypoing command to direct the agentic tool.
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
    - (a) during `awman ready` (`ReadyPhase::CheckingLocalAgent`);
    - (b) periodically by the credential-refresh monitor's tick loop while credentialed agent sessions are live (`CredentialRefreshMonitor::tick`, which reaches the host through the `HostAgentPinger` it holds), bounded by the per-agent backoff and a per-tick timeout;
    - (c) once, synchronously and time-bounded, as the workflow pre-step guard when a step's file-delivered credential is within the refresh threshold of expiry (`refresh_credential_blocking` from `exec_workflow.rs`); and
    - (d) at most once per workflow step, as auth-failure recovery after a step exits with an authentication error the descriptor recognizes (`recover_auth_failure`).

    Every trigger runs the identical, unchanged ping: the prompt is hardcoded (from the `GREETINGS` table), there is no user input, no repo content, the process runs in a dedicated empty directory outside the repository (never the repo working directory), and the cheapest available model is pinned. Triggers (c) and (d) route through the same monitor/ping path as (a) and (b) — a single type, `HostAgentPinger`, now handles all four; they widen *when* the ping runs, never *what* it does. No other code path may execute an agent on the host. Any new host-side agent invocation beyond this check is a security violation.

    Do not conflate `HostAgentPinger` with `engine::daemon::DaemonSupervisor`,
    which is the only type permitted to spawn the *daemon* binary
    (`awman api start` / `awman squad start`'s detached child) — a distinct
    exception for a distinct purpose.
- **Credential delivery** — For Claude on container-class runtimes, credentials are delivered as an awman-authored, refresh-token-free credential file inside the staged settings overlay (mode `0600`, atomically replaced via `rename`); the host's own credential files and refresh tokens are never mounted into a container.
- Never mount any directory to any Docker container other than the current directory. If any parent directories are a Git repo root, the aspec CLI will prompt the user if the mounted directory should be limited to the current CWD or can be expanded to the Git repo root. Follow this instruction for every single container launched.
  - **Sanctioned exception — the Docker socket.** `--allow-docker` mounts the
    host's `docker.sock` into the container (`/var/run/docker.sock` on
    Unix, an `npipe` on Windows) alongside the current-directory-only bind
    mount above, so a container that needs to drive Docker itself (e.g. an
    agent building or running images as part of its task) can reach the host
    daemon. This is a deliberate, opt-in widening of what the container can
    reach beyond its filesystem mount: a socket, not a directory, and
    off by default. `docker.rs`'s `allow_docker` argv builder is the only
    place this mount is constructed.
