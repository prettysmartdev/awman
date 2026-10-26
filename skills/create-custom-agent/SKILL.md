---
name: create-custom-agent
description: >
  Use when asked to add support for <agent> to awman, create an awman custom agent,
  or write an awman agent TOML definition.
---

# Create a custom agent definition

## 1. Context inside the container

You are running inside an awman container. The repository is at `/workspace`. You cannot run `awman` here, and you cannot write to the user's `~/.awman`. Write the definition to `/workspace/.awman/agents/<name>.toml`.

## 2. Research the agent CLI

If the agent can be installed in this container, inspect `<bin> --help` and its subcommands. Otherwise, use the agent's documentation or README. Verify the actual CLI behavior and fill in this checklist:

- Install method (npm, curl script, binary release, or another method)
- Binary name
- Interactive entrypoint
- Non-interactive or print mode
- How an initial prompt is passed
- YOLO, auto, and plan flags
- Model flag form
- System-prompt mechanism (maps to one `delivery`)
- Host config directory
- Skills or commands directory
- Authentication environment variables
- ACP support

## 3. Pick a mode

Use install steps by default. Choose a Dockerfile escape hatch for multi-stage builds or unusual base-image needs. For an escape-hatch Dockerfile, use `FROM {{AWMAN_BASE_IMAGE}}` when the agent image should be layered on awman's project image.

## 4. Write the TOML

Read [`reference.md`](reference.md) and start from the closest example in [`examples/`](examples/). Write `schema = 1` explicitly. Leave `[ping]` out unless the user asks for a host check: it is the optional command that runs on the user's host, and without it `awman ready` skips that check. If the user wants a host check, use the agent's cheapest one-shot print mode (for example, `command = ["pi", "-p"]`) and explain that the approval prompt will show the command.

## 5. Self-validate

Check the definition against the validation table in [`reference.md`](reference.md):

- The filename stem matches `^[a-z0-9][a-z0-9_-]{0,63}$`.
- Every install step is a single line.
- `settings.host_dir` is not on the host-directory deny-list.
- The selected system-prompt `delivery` has exactly the required fields.
- argv fields use explicit tokens and do not rely on shell metacharacters or shell parsing.
- The file contains `schema = 1` and no `[sandbox]` table.

## 6. Hand off

Tell the user to run `awman ready --agent <name>` on the host. That step reviews and approves the definition and builds the image. They can then run `awman chat --agent <name>`. They can move the file to `~/.awman/agents/` to use it in every repository. Do not claim the image was built or tested.
