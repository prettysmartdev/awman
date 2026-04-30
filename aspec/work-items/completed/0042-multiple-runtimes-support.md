# Work Item: Feature

Title: Multiple Runtimes Support
Issue: issuelink

## Summary:
- Define an `AgentRuntime` Rust trait that abstracts all container operations currently hardcoded to Docker.
- Refactor `src/docker/mod.rs` into a `DockerRuntime` struct that implements the trait — no behavior change, just structural.
- Investigate and implement an `AppleContainersRuntime` as a second implementation (macOS-only, using Apple's `container` CLI introduced in macOS 26).
- Expose a runtime selector in global/repo config so users can choose their preferred runtime.

## User Stories

### User Story 1:
As a: user on macOS with Apple Containers installed

I want to: configure amux to use Apple Containers as my agent runtime instead of Docker

So I can: run agents without Docker Desktop overhead, using a native macOS container runtime that integrates with the system.

### User Story 2:
As a: user

I want to: see a clear error message when the configured runtime is unavailable (e.g., Docker daemon not running, Apple Containers not installed)

So I can: quickly diagnose why `amux` failed to launch an agent and know which runtime to fix or switch to.

### User Story 3:
As a: contributor

I want to: add a new container runtime by implementing a single `AgentRuntime` trait

So I can: extend amux to support additional runtimes (e.g., Podman, Lima) without touching the existing agent-launching or TUI code.


## Implementation Details:

### Phase 1 — Define the `AgentRuntime` trait (`src/runtime/mod.rs`)

Extract the contract from `src/docker/mod.rs`. The trait must cover every operation consumed by the rest of the codebase:

```rust
// src/runtime/mod.rs
pub trait AgentRuntime: Send + Sync {
    // Availability
    fn is_available(&self) -> bool;

    // Image lifecycle
    fn build_image(&self, tag: &str, dockerfile: &Path, context: &Path, no_cache: bool) -> Result<String>;
    fn build_image_streaming<F>(&self, tag: &str, dockerfile: &Path, context: &Path, no_cache: bool, on_line: F) -> Result<String>
    where F: FnMut(&str) + Send;
    fn image_exists(&self, tag: &str) -> bool;

    // Container run variants
    fn run_container(&self, image: &str, host_path: &str, entrypoint: &[&str],
        env_vars: &[(String, String)], host_settings: Option<&HostSettings>,
        allow_docker: bool, container_name: Option<&str>, ssh_dir: Option<&Path>) -> Result<()>;
    fn run_container_captured(&self, image: &str, host_path: &str, entrypoint: &[&str],
        env_vars: &[(String, String)], host_settings: Option<&HostSettings>,
        allow_docker: bool, container_name: Option<&str>, ssh_dir: Option<&Path>) -> Result<(String, String)>;
    fn run_container_at_path(&self, image: &str, host_path: &str, container_path: &str,
        working_dir: &str, entrypoint: &[&str], env_vars: &[(String, String)],
        host_settings: Option<&HostSettings>, allow_docker: bool,
        container_name: Option<&str>) -> Result<()>;
    fn run_container_captured_at_path(&self, image: &str, host_path: &str, container_path: &str,
        working_dir: &str, entrypoint: &[&str], env_vars: &[(String, String)],
        host_settings: Option<&HostSettings>, allow_docker: bool) -> Result<(String, String)>;
    fn run_container_detached(&self, image: &str, host_path: &str, container_path: &str,
        working_dir: &str, container_name: Option<&str>, env_vars: Vec<(String, String)>,
        allow_docker: bool, host_settings: Option<&HostSettings>) -> Result<String>;

    // Container lifecycle
    fn start_container(&self, container_id: &str) -> Result<()>;
    fn stop_container(&self, container_id: &str) -> Result<()>;
    fn remove_container(&self, container_id: &str) -> Result<()>;
    fn is_container_running(&self, container_id: &str) -> bool;
    fn find_stopped_container(&self, name: &str, image: &str) -> Option<StoppedContainerInfo>;

    // Discovery & stats
    fn list_running_containers_by_prefix(&self, prefix: &str) -> Vec<String>;
    fn list_running_containers_with_ids_by_prefix(&self, prefix: &str) -> Vec<(String, String)>;
    fn get_container_workspace_mount(&self, container_name: &str) -> Option<String>;
    fn query_container_stats(&self, name: &str) -> Option<ContainerStats>;

    // PTY argument builders (for TUI interactive sessions)
    fn build_run_args_pty(&self, image: &str, host_path: &str, entrypoint: &[&str],
        env_vars: &[(String, String)], host_settings: Option<&HostSettings>,
        allow_docker: bool, container_name: Option<&str>, ssh_dir: Option<&Path>) -> Vec<String>;
    fn build_run_args_pty_display(&self, image: &str, host_path: &str, entrypoint: &[&str],
        env_vars: &[(String, String)], host_settings: Option<&HostSettings>,
        allow_docker: bool, container_name: Option<&str>, ssh_dir: Option<&Path>) -> Vec<String>;
    fn build_run_args_pty_at_path(&self, image: &str, host_path: &str, container_path: &str,
        working_dir: &str, entrypoint: &[&str], env_vars: &[(String, String)],
        host_settings: Option<&HostSettings>, allow_docker: bool,
        container_name: Option<&str>) -> Vec<String>;
    fn build_exec_args_pty(&self, container_id: &str, working_dir: &str,
        entrypoint: &[&str], env_vars: &[(String, String)]) -> Vec<String>;

    // Display helpers (masked env vars, shortened paths)
    fn build_run_args_display(&self, image: &str, host_path: &str, entrypoint: &[&str],
        env_vars: &[(String, String)], host_settings: Option<&HostSettings>,
        allow_docker: bool, container_name: Option<&str>, ssh_dir: Option<&Path>) -> Vec<String>;

    // Runtime name for display/logging
    fn name(&self) -> &'static str;

    // CLI binary used (e.g. "docker", "container") — used for display-only command strings
    fn cli_binary(&self) -> &'static str;
}
```

Keep pure utility functions (`generate_container_name`, `project_image_tag`, `parse_cpu_percent`, `parse_memory_mb`) as free functions in `src/runtime/mod.rs` — they are not runtime-specific.

### Phase 2 — Refactor Docker into `DockerRuntime`

Create `src/runtime/docker.rs` (or keep as `src/docker/mod.rs` and re-export behind the trait):

- Move all existing functions from `src/docker/mod.rs` into `impl AgentRuntime for DockerRuntime`.
- `DockerRuntime` holds no state beyond what the current free functions use (Docker socket path is resolved lazily).
- Keep `HostSettings` struct in `src/runtime/mod.rs` (it is runtime-adjacent but not Docker-specific; Apple Containers will need a similar credential injection mechanism).
- All call sites in `commands/`, `tui/`, etc. change from `docker::fn()` to `runtime.fn()`, where `runtime: &dyn AgentRuntime` is threaded through from the top-level dispatch.
- `src/lib.rs` / `src/commands/mod.rs`: resolve the runtime once at startup (from config) and pass it as `Arc<dyn AgentRuntime>` or `&dyn AgentRuntime` into every command handler.

### Phase 3 — Runtime selection via config

Extend `GlobalConfig` and `RepoConfig` in `src/config/mod.rs`:

```rust
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct GlobalConfig {
    pub default_agent: Option<String>,
    pub terminal_scrollback_lines: Option<usize>,
    pub runtime: Option<String>,  // "docker" (default) | "apple-containers"
}
```

Add a factory function in `src/runtime/mod.rs`:

```rust
pub fn resolve_runtime(config: &GlobalConfig) -> Arc<dyn AgentRuntime> {
    match config.runtime.as_deref().unwrap_or("docker") {
        "apple-containers" => Arc::new(AppleContainersRuntime::new()),
        _ => Arc::new(DockerRuntime::new()),
    }
}
```

Propagate `Arc<dyn AgentRuntime>` from `main.rs` → `commands::dispatch()` → individual command handlers.

### Phase 4 — Investigate and implement `AppleContainersRuntime`

Apple Containers (`container` CLI, macOS 26+) is an OCI-compatible container runtime. Research and implement the following:

**Investigation checklist:**
**IF ANYTHING IS NOT FEASIBLE OR WILL GREATLY DEGRADE THE USER EXPERIENCE, ASK THE USER THEIR OPINION BEFORE CONTINUING IMPLEMENTATION**
- Confirm `container` CLI availability and command surface: `container run`, `container build`, `container ps`, `container stop`, `container rm`, `container images`, `container stats` (or equivalent).
- Determine if Dockerfiles are supported natively or if a conversion/pre-build step is needed.
- Determine how env vars, volume mounts (`-v`), and working directories map to `container` flags.
- Determine whether `docker exec` equivalent (`container exec`) is supported for PTY sessions (TUI attach mode).
- Determine if Docker socket passthrough (`--allow-docker`) is meaningful or should be blocked/warned for this runtime.
- Determine the output format of `container ps` and `container stats` to update parsing logic.
- Identify any restrictions on running Linux containers vs. native macOS containers.

**Implementation:**
- Create `src/runtime/apple.rs` with `AppleContainersRuntime` struct.
- Implement `AgentRuntime` for `AppleContainersRuntime` mapping each method to the `container` CLI equivalents found above.
- For unsupported operations (e.g., if `exec` PTY is unavailable), return a descriptive `Err` with a clear message. **BUT ASK THE USER PER DEFFICIENT ITEM BEFORE IMPLEMENTING**
- Gate compilation of `src/runtime/apple.rs` behind `#[cfg(target_os = "macos")]`.
- `resolve_runtime()` should return `Err` (or fall back to Docker with a warning) if `"apple-containers"` is requested on non-macOS.

### Phase 5 — `amux ready` runtime check

Update `commands/ready.rs` to:
- Validate the configured runtime is available (`runtime.is_available()`).
- Print which runtime is active.
- Report a clear error if the runtime binary is missing or the daemon is not running.


## Edge Case Considerations:

- **Runtime not installed**: If the user sets `runtime: "apple-containers"` but the `container` CLI is not in PATH, fail at startup with a clear message rather than failing mid-launch.
- **macOS-only runtime on Linux/Windows**: `resolve_runtime()` must reject `"apple-containers"` on non-macOS at startup, not silently fall back, so users know their config is invalid.
- **`--allow-docker` flag with Apple Containers**: Mounting the Docker socket into an Apple Containers container may be meaningless or unsupported. Warn the user and document the behavior.
- **Detached containers and `find_stopped_container`**: Apple Containers may not persist stopped container state the same way Docker does. The nanoclaw restart flow (`find_stopped_container` → `start_container`) may need a different recovery path.
- **HostSettings / credential injection**: `HostSettings` currently uses temp directories and file mounts. Verify Apple Containers supports bind mounts with the same `-v host:container` syntax; if not, implement an alternative injection path (**verify with user before choosing a fallback implrmentation**).
- **PTY / TUI interactive sessions**: The TUI uses docker-specific arg builders (`build_run_args_pty`, `build_exec_args_pty`) to spawn PTY processes. If `container exec` supports `-it`, map these directly; otherwise, the TUI must degrade gracefully or disable interactive mode for this runtime. (**verify with user before choosing a fallback implrmentation**)
- **`docker stats` output format**: `ContainerStats` parsing is tuned to Docker's `--format` output. Apple Containers stats output may differ — implement separate parsers guarded by the runtime type.
- **Image tag naming**: `project_image_tag()` produces `amux-{project}:latest`. Verify Apple Containers accepts this tag format.
- **Unknown `runtime` value in config**: `resolve_runtime()` should warn and fall back to Docker rather than panic, to avoid breaking existing users after a typo.
- **Concurrent runtime access**: `Arc<dyn AgentRuntime>` must be `Send + Sync`. Ensure no interior mutability in runtime structs that is not protected.


## Test Considerations:

- **Unit tests for `DockerRuntime`**: Each refactored method must have the same coverage as the original free functions in `docker/mod.rs` — inputs, outputs, and error paths. Use a mock `Command` executor or test with `--dry-run` style flags where possible.
- **Unit tests for `resolve_runtime()`**: Test that each config string resolves to the correct runtime type; test fallback behavior for unknown strings; test macOS-only guard.
- **Unit tests for `AppleContainersRuntime`**: Mock the `container` CLI binary (or use a test double) to verify arg construction and output parsing without requiring the runtime to be installed in CI.
- **Integration tests for `DockerRuntime`**: Existing integration tests that exercise the Docker code path should pass without modification after the refactor — this is the primary correctness check for Phase 2.
- **Integration tests for `AppleContainersRuntime`**: Run on a macOS CI runner with Apple Containers installed. Gate behind `#[cfg(target_os = "macos")]` and an env var guard (e.g., `AMUX_TEST_APPLE_CONTAINERS=1`) so they are opt-in.
- **End-to-end tests**: `amux ready` should report the correct runtime name. `amux chat --non-interactive` should work with both runtimes (on their respective platforms).
- **Stats parsing tests**: Add table-driven tests for both Docker and Apple Containers stat output formats, including edge cases like missing fields or unusual units.
- **Error path tests**: Verify that requesting an unavailable runtime produces the expected user-facing error, not a panic or opaque OS error.


## Codebase Integration:

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- The `src/docker/mod.rs` module is the single largest file in the codebase (1,376 lines). Moving its content into `src/runtime/docker.rs` and the trait into `src/runtime/mod.rs` keeps module sizes manageable.
- `HostSettings` is consumed by Docker-specific mount logic but conceptually belongs to the runtime layer — move it to `src/runtime/mod.rs` so `AppleContainersRuntime` can reuse it.
- Pure utilities (`generate_container_name`, `project_image_tag`, `parse_cpu_percent`, `parse_memory_mb`, `format_build_cmd`, `format_run_cmd`) belong in `src/runtime/mod.rs` as free functions — they are not runtime-specific.
- `commands/agent.rs` is the central orchestrator (`run_agent_with_sink`). Update it to accept `runtime: &dyn AgentRuntime` in place of direct `docker::` calls — this is the highest-leverage integration point.
- `tui/mod.rs` uses `docker::query_container_stats` and `docker::list_running_containers_with_ids_by_prefix` for status polling. Both must be routed through the runtime interface.
- `commands/claws.rs` uses `docker::run_container_detached`, `docker::build_exec_args_pty`, and `docker::start_container` for nanoclaw. These call sites must be updated; nanoclaw may be Docker-only initially — document that limitation clearly if `AppleContainersRuntime` does not implement detached mode.
- `CLAUDE.md` security constraint — never execute agents outside containers — remains unchanged. The trait enforces this by design: all execution paths go through `run_container*` methods.
- Keep `Cargo.toml` changes minimal: no new dependencies are needed for Phase 1–3. Phase 4 may require OS detection (`#[cfg(target_os = "macos")]`) but no new crates.
- Adhere to the `aspec/architecture/security.md` mount constraints in every `AgentRuntime` implementation — never mount parent directories beyond the Git root without user confirmation.
