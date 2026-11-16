---
name: security-review
description: A guided agent security review of awman, its CI/CD, and repo.
---

# Security Review

Conduct a systematic security review of awman across five domains: container security, network and API security, secrets and credentials, CI/CD supply chain, and codebase hygiene. Work through each section in order. For each finding, note the file path and line number, the severity (Critical / High / Medium / Low / Info), and a one-sentence description of the risk. Collect all findings, then print a summary table at the end.

---

## 1. Container Security

Containers are awman's primary security boundary. Verify that boundary is correctly enforced.

### 1a. Agent image Dockerfiles

Check every Dockerfile in `templates/` and `.awman/`:

```bash
find templates/ .awman/ -name 'Dockerfile.*' | sort
```

For each file:

- **Non-root user** — Verify the file contains `USER awman` (or another non-root user) before the final `CMD`/`ENTRYPOINT`. Flag any image that runs as root.
- **Install scripts via curl** — Search for `curl … | bash` or `curl … | sh` patterns. Each one is a supply-chain risk: the remote server can serve arbitrary code. Note the URL and whether a checksum or signature is verified.
  ```bash
  grep -n 'curl.*|.*sh\|curl.*sh.*bash' templates/Dockerfile.* .awman/Dockerfile.*
  ```
- **Package pinning** — Check whether apt packages and tool versions are pinned (`= x.y.z` or `--locked`). Unpinned installs allow silent version drift.
- **Secrets baked in** — Scan for any `ENV`, `ARG`, or `COPY` that brings a credential, token, or private key into the image layer.
  ```bash
  grep -niE 'ENV.*(KEY|TOKEN|SECRET|PASSWORD|PASS)\s*=' templates/Dockerfile.* .awman/Dockerfile.*
  ```

### 1b. Mount configuration

Read `src/runtime/docker.rs` (and `src/runtime/apple_containers.rs` if present). For each `--volume` / `-v` / `--mount` flag passed to `docker run`:

- **Repo root scope** — Verify awman mounts only the current directory or the Git repo root, never a parent directory above the repo root.
- **SSH key mount** — Locate the SSH mount (search for `.ssh`). Confirm it is mounted read-only (`:ro`). If it is read-write, flag it High.
  ```bash
  grep -n '\.ssh' src/runtime/docker.rs
  ```
- **Docker socket mount** — Locate the Docker socket mount (search for `/var/run/docker.sock`). This gives the container the ability to spawn sibling containers and is a known container-escape vector. Confirm whether the mount exists, whether it is documented, and whether it is required by any agent workflow.
  ```bash
  grep -n 'docker.sock' src/runtime/docker.rs
  ```
- **Overlay permission enforcement** — Find where `overlays.directories[].permission` is converted to a `:ro`/`:rw` flag. Verify that omitting the field defaults to read-only, not read-write.
  ```bash
  grep -n 'overlay\|permission\|":ro"\|":rw"' src/runtime/docker.rs
  ```

### 1c. envPassthrough handling

Find where `envPassthrough` values are injected into `docker run`:

```bash
grep -rn 'env_passthrough\|envPassthrough\|passthrough' src/ --include='*.rs'
```

Confirm that:
- Only the **variable names** listed in config are forwarded — awman reads the value from the host environment at runtime and passes `-e NAME=VALUE` to Docker. If the names are baked into the image or config file, flag it.
- No passthrough list is stored in a world-readable file (check the repo config at `.awman/config.json` for actual secrets embedded as values, not just names).
  ```bash
  cat .awman/config.json
  ```

### 1d. Privileged mode and capabilities

Check that no container is launched with `--privileged`, `--cap-add=ALL`, or dangerous individual capabilities (`SYS_ADMIN`, `NET_ADMIN`, `SYS_PTRACE`):

```bash
grep -n 'privileged\|cap-add\|cap_add\|SYS_ADMIN\|NET_ADMIN\|SYS_PTRACE' src/runtime/docker.rs
```

Flag any match as High unless accompanied by a documented justification.

---

## 2. Network and Headless API Security

### 2a. Bind address

Read `src/commands/headless/server.rs`. Find the `TcpListener::bind` call and record the address:

```bash
grep -n 'bind\|0\.0\.0\.0\|127\.0\.0\.1' src/commands/headless/server.rs
```

- If the server binds to `0.0.0.0`, it accepts connections from any network interface (LAN, VPN, etc.), not just localhost. This is Medium if auth is required, High if `--dangerously-skip-auth` is used without warnings.
- If the server binds to `127.0.0.1`, it is only accessible locally — flag this as Info (good practice).

### 2b. TLS / transport encryption

Check whether the headless server supports TLS. Search for `rustls`, `tls`, `https` in the server and config files:

```bash
grep -rn 'tls\|rustls\|https' src/commands/headless/ --include='*.rs'
```

If TLS is absent, the API key and all command payloads travel in plaintext. Flag this High for any deployment where the server is reachable over a non-loopback interface.

### 2c. Authentication middleware

Read `src/commands/headless/auth.rs`. Verify:

- **`--dangerously-skip-auth` flag** — Confirm it exists and check whether awman emits a prominent warning when it is used.
  ```bash
  grep -n 'skip.auth\|dangerously' src/commands/headless/server.rs src/commands/headless/auth.rs
  ```
- **Constant-time comparison** — The API key comparison must use `subtle::ConstantTimeEq` or equivalent. A standard `==` comparison leaks key length via timing. Flag any `==` on key bytes as High.
  ```bash
  grep -n 'ConstantTimeEq\|constant_time\|== key\|== hash' src/commands/headless/auth.rs
  ```
- **Key generation** — Verify the key is generated with `ring::rand::SecureRandom` (32+ bytes). A short or PRNG-derived key is High.
  ```bash
  grep -n 'SecureRandom\|rand\|generate' src/commands/headless/auth.rs
  ```

### 2d. API key storage

Locate the stored key hash file and its permissions:

```bash
grep -n 'api_key\|0o600\|mode(' src/commands/headless/auth.rs
```

Verify:
- The file is created with mode `0o600` on Unix (owner read/write only). Any looser mode (e.g., `0o644`) exposes the hash.
- On non-Unix platforms (Windows), confirm whether equivalent ACL restrictions are applied or document the gap.
- The plaintext key is printed **once** to stdout and never written to disk. If it is logged to a file or stored in plaintext, flag it Critical.

### 2e. workDirs allowlist

For the headless server, find how `headless.workDirs` is enforced:

```bash
grep -rn 'work_dirs\|workDirs\|allowlist\|allowed' src/commands/headless/ --include='*.rs'
```

Confirm that any path not in the allowlist is rejected before the container is started, not after. If the check is missing or happens post-launch, flag it High.

---

## 3. Secrets and Credential Handling

### 3a. Hardcoded secrets scan

Search the entire Rust source tree and config files for patterns that look like embedded credentials:

```bash
grep -rn --include='*.rs' -E \
  '(api[_-]?key|secret|token|password|passwd|credential)\s*=\s*"[^"]' \
  src/ tests/
```

```bash
grep -rn --include='*.json' -E \
  '"(key|secret|token|password)":\s*"[^{]' \
  .awman/ aspec/
```

Any match outside of test fixtures or documentation is Critical if it looks like a real credential.

### 3b. Git history scan for secrets

Check recent commits and any committed config files for accidentally included secrets:

```bash
git log --all --oneline | head -50
git diff HEAD~10..HEAD -- '*.json' '*.toml' '*.env' '*.yaml' '*.yml' | \
  grep -E '^\+.*(key|secret|token|password).*=' | grep -v '#'
```

If you find a secret in git history, flag it Critical — rotating the credential is required even after a history rewrite.

### 3c. OAuth token extraction (macOS keychain)

Read `src/commands/auth.rs`. The Claude agent auth reads from the macOS keychain:

```bash
grep -n 'security\|keychain\|accessToken\|CLAUDE_CODE_OAUTH_TOKEN' src/commands/auth.rs
```

Verify:
- The token is extracted from the keychain via `security find-generic-password` and injected only as an environment variable into the container — it is never written to disk or logged.
- If `auto_agent_auth_accepted` is `false`, the user is prompted before the token is passed. Confirm the prompt path exists.

### 3d. Cline secrets mount

Locate where Cline's `~/.cline/data/secrets.json` is copied and mounted:

```bash
grep -rn 'cline\|secrets.json' src/runtime/docker.rs
```

Verify:
- The file is copied to a **temp directory** (not mounted directly from `~/.cline`). Direct mounts of the home subtree are a blast-radius risk.
- The copy is cleaned up after the container exits. If it persists, flag it Medium.
- The `tasks/` and `workspace/` directories under `.cline/data/` are explicitly excluded from the mount. If they are included, flag it Medium.

---

## 4. CI/CD and Supply Chain Security

### 4a. Workflow trigger scope

Read `.github/workflows/test.yml` and `.github/workflows/release.yml`:

```bash
cat .github/workflows/test.yml
cat .github/workflows/release.yml
```

Check:
- **test.yml** — Should trigger on push/PR to all branches. Verify no secrets are injected into untrusted PR workflows (any `pull_request_target` trigger with `secrets.*` access is High).
- **release.yml** — Should trigger only on version tags (`v[0-9]+.[0-9]+.[0-9]+`). If it can be triggered on arbitrary branches or by external PRs, flag it High.

### 4b. Action pinning

For every `uses:` line in both workflows, check whether third-party actions are pinned to a full commit SHA or only to a mutable tag:

```bash
grep -n 'uses:' .github/workflows/*.yml
```

- `uses: actions/checkout@v4` — mutable tag; a compromise of that tag serves malicious code. Flag as Medium.
- `uses: actions/checkout@a81bbbf8298c0fa03ea29cdc473d45769f953675` — commit SHA; immutable. This is the correct form.

For each unpinned action, note the action name, the current tag, and whether it is from a trusted publisher (`actions/`, `docker/`, `softprops/` with high download counts).

### 4c. Secrets in workflow environment

Check that no workflow step echoes, prints, or exports secrets to workflow logs:

```bash
grep -n 'echo.*SECRET\|echo.*TOKEN\|echo.*KEY\|run.*env' .github/workflows/*.yml
```

Also check for `set-output` or `::set-output` used to forward secrets to downstream steps (deprecated but still dangerous).

### 4d. Dependency audit

Run a vulnerability scan against the locked dependency tree. If `cargo-audit` is not installed, note that it should be run:

```bash
cargo audit 2>&1 | head -80
```

If `cargo-audit` is unavailable:

```bash
# List all dependencies and their versions for manual review
cargo metadata --format-version 1 | \
  python3 -c "import json,sys; pkgs=json.load(sys.stdin)['packages']; \
  [print(f\"{p['name']}=={p['version']}\") for p in pkgs]" | sort
```

Flag any package with a known CVE as Critical (if a fix is available) or High (if no fix exists).

### 4e. Release script safety

Read `scripts/release.sh`:

```bash
head -60 scripts/release.sh
```

Verify:
- The script begins with `set -euo pipefail` (or equivalent). Missing `pipefail` means silent failures in pipelines.
- Interactive prompts use `/dev/tty`, not stdin — this prevents the script from running non-interactively in CI.
- The script does not accept a `--force` or `--yes-all` flag that bypasses all confirmations. If it does, flag it Medium.
- No credentials are hardcoded; the script relies on `gh auth status` and environment-provided tokens.

### 4f. Cargo.lock committed

Confirm `Cargo.lock` is committed to the repository (it should be for a binary crate):

```bash
git ls-files Cargo.lock
```

A missing or gitignored `Cargo.lock` for a binary means CI builds can silently pick up newer (potentially vulnerable or breaking) dependency versions. Flag a missing lock as High.

---

## 5. Codebase Hygiene

### 5a. File permissions on committed files

Check for executable bits set on files that should not be executable (e.g., `.rs`, `.toml`, `.json`, `.md`):

```bash
git ls-files --stage | awk '$1 ~ /755|744/' | head -30
```

Scripts (`scripts/release.sh`) are expected to be executable. Source files, configs, and docs should not be. Flag unexpected `755` files as Low.

### 5b. Sensitive files not in .gitignore

Check that common secret-bearing files are covered by `.gitignore`:

```bash
cat .gitignore
```

The following patterns should be present (or the files should not exist in the repo):
- `.env`, `.env.*`
- `*.pem`, `*.key`, `*.p12`, `*.pfx`
- `secrets.json`, `credentials.json`
- `~/.awman/config.json` (global config — should not be in this repo)

If any of these files are tracked by git:
```bash
git ls-files | grep -E '\.(env|pem|key|p12|pfx)$|secrets\.json|credentials\.json'
```

Flag any match as Critical.

### 5c. Unsafe Rust

Search for `unsafe` blocks in the source:

```bash
grep -rn 'unsafe' src/ --include='*.rs' | grep -v '^\s*//'
```

Each `unsafe` block is not automatically a vulnerability, but each one must be justified. For every match: read the surrounding context, determine whether the invariant required for safety is clearly upheld (documented with a `// SAFETY:` comment), and flag any undocumented `unsafe` as Medium.

### 5d. Command injection in shell invocations

Search for places where user-controlled data is passed to shell commands:

```bash
grep -rn 'Command::new\|std::process::Command\|shell\|exec' src/ --include='*.rs' | \
  grep -v test | grep -v '#\[cfg(test'
```

For each match, check whether arguments are passed as separate `arg()` calls (safe) or concatenated into a single shell string with `sh -c "..."` (dangerous — command injection). Flag any `sh -c` that incorporates user input as High.

### 5e. Path traversal in config-driven paths

Find all places where user-supplied paths from config (overlays, workDirs, etc.) are used to construct file system paths:

```bash
grep -rn 'PathBuf\|path::Path\|canonicalize\|join(' src/ --include='*.rs' | head -40
```

Verify that paths from config are canonicalized and validated against an allowlist before use. A path like `../../etc/passwd` in `overlays.directories[].host` should be rejected, not passed to `docker run -v`. Flag missing canonicalization as Medium.

---

## 6. Final Report

After completing all sections, print a consolidated findings table in this format:

```
| # | Severity | Domain           | File / Location                        | Finding                                           |
|---|----------|------------------|----------------------------------------|---------------------------------------------------|
| 1 | High     | Network          | src/commands/headless/server.rs:NN     | Headless server binds 0.0.0.0; no TLS in transit |
| 2 | Medium   | Container        | src/runtime/docker.rs:NN               | Docker socket mounted; enables container escape   |
| … | …        | …                | …                                      | …                                                 |
```

Severity levels: **Critical** (exploit likely, immediate action), **High** (significant risk, fix before next release), **Medium** (risk with mitigating factors), **Low** (defence-in-depth gap), **Info** (good practice observed).

After the table, list:
1. **Immediate actions** — Critical and High findings that should be fixed before the next release.
2. **Recommended improvements** — Medium and Low findings worth tracking.
3. **Confirmed secure** — Areas where the implementation is correct and intentional, so reviewers know these were checked.
