# Custom agent definition reference — schema 1

A custom agent definition is a TOML file named `<name>.toml` in `.awman/agents/` (repo scope) or `<data_home>/agents/` (global scope). The file stem is the agent name; there is no `name` key. Names must match `^[a-z0-9][a-z0-9_-]{0,63}$`. A repo definition takes precedence over a global definition, while first-party names are reserved and cannot be overridden. Schema 1 rejects unknown keys. A top-level `[sandbox]` table is reserved and rejected; custom agents currently use Docker, Apple Containers, or builtin-experimental runtimes.

## Top-level fields

| Key | Type | Required | Meaning |
|---|---|---:|---|
| `schema` | integer | yes | Must be `1`. |
| `display_name` | string | yes | Name shown in ready, status, and the TUI. |
| `description` | string | no | Short description. |
| `homepage` | string | no | Project URL surfaced in errors and the trust review. |
| `[run]` | table | yes | Interactive command and launch behavior. |
| `[modes]` | table | no | Optional YOLO, auto, and plan flag arrays. Missing modes are silent no-ops. |
| `[tools]` | table | no | Flags for allowed and disallowed tool restrictions. |
| `[system_prompt]` | table | no | System-prompt delivery configuration. Defaults to `delivery = "none"`. |
| `[acp]` | table | no | Presence enables ACP with its own argv command. |
| `[ping]` | table | no | Optional argv command run on the host only by `awman ready`, after approval. |
| `[settings]` | table | no | Host settings directory and in-container skills directory. |
| `[auth]` | table | no | Host environment variable names passed through when set. |
| `[image]` | table | yes | Install steps or a user Dockerfile escape hatch. |

## Field details

### `[run]`

| Key | Type | Default / rule |
|---|---|---|
| `command` | string array | Required interactive entrypoint argv. `command[0]` is a bare binary name or an absolute container path. No shell is used. |
| `non_interactive` | string | Optional token appended for non-interactive `-n`, exec, and workflow steps. |
| `interactive_only_flags` | string array | Optional flags appended only to interactive launches. |
| `seed_prompt` | string or table | Defaults to `"positional"`; alternatively `{ flag = "--prompt" }`. |
| `model` | string | `"space"` (default, `--model NAME`), `"equals"` (`--model=NAME`), or `"unsupported"`. |
| `env` | string-to-string table | Optional literal environment values always set in the container. |

### `[modes]`, `[tools]`, and system prompt delivery

`[modes]` keys are optional arrays of argv tokens: `yolo`, `auto`, and `plan`. An absent or empty `plan` rejects `--plan` as unsupported; absent `yolo` and `auto` flags are silent no-ops.

`[tools]` has optional string keys `allowed_flag` and `disallowed_flag`.

`[system_prompt]` supports these keys:

| Key | Type | Meaning |
|---|---|---|
| `delivery` | string | One of `append-file`, `append-inline`, `replace`, `agents-md`, `env-file`, `add-dir`, or `none`; default `none`. |
| `flag` | string | Required for `append-file`, `append-inline`, `replace`, and `add-dir`. |
| `key` | string | Required for `append-inline`. |
| `env` | string | Required for `env-file`. |

Each delivery accepts only its required field(s): `append-file`, `replace`, and `add-dir` require `flag`; `append-inline` requires `flag` and `key`; `env-file` requires `env`; `agents-md` and `none` accept no extra fields.

### `[acp]`, `[ping]`, `[settings]`, and `[auth]`

- `[acp] command`: required argv array; its presence means the agent supports ACP.
- `[ping] command`: optional host argv array. It runs only during `awman ready`, in an empty temporary directory, and the fixed greeting is appended as the final argument. It does not receive `run.env` or `auth.env`. A definition with `[ping]` requires trust approval in either scope; the approval review shows the host command.
- `[settings] host_dir`: optional normalized path relative to the host `$HOME`, mounted read-write at the same relative path under the container home when it exists.
- `[settings] skills_dir`: optional relative path under container `$HOME`; when absent, the skills overlay is skipped.
- `[auth] env`: optional array of POSIX environment variable names. Set host variables are passed through; unset names are skipped.

### `[image]`

| Key | Type | Default / rule |
|---|---|---|
| `needs_node` | boolean | Optional, default `false`; adds the Node 20 setup block. |
| `apt` | string array | Optional extra Debian packages. |
| `install` | string array | Required unless `dockerfile` is set. Each item becomes one root `RUN` step in the hardened awman Dockerfile wrapper. |
| `binary` | string | Optional smoke-test binary; defaults to `run.command[0]`. |
| `dockerfile` | string | Optional path relative to this TOML, for a hand-written Dockerfile escape hatch. Mutually exclusive with `needs_node`, `apt`, and `install`. |

Install steps are shell commands executed only during image build. Keep each step on one line. The escape-hatch Dockerfile should start with `FROM {{AWMAN_BASE_IMAGE}}` to layer on awman's project image. awman copies it into the repo build context and substitutes the placeholder.

## Validation table

Validation reports every violation found. TOML tables reject unknown keys. The file is capped at 64 KiB and every array at 64 entries.

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
