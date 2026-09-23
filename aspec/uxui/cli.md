# CLI & TUI UX Standards

Binary name: `awman`
Install path: `/usr/local/bin/`
Storage location: `$HOME/.awman/`

This document is the authoritative specification of `awman`'s interaction
design: the conventions every command, flag, prompt, and dialog follows,
across every frontend. It does not enumerate commands or flags — that
surface is generated from `CommandCatalogue`
(`src/command/dispatch/catalogue.rs`) into `docs/14-command-reference.md`,
which can never drift from the code because it is the code, rendered.
Anything below applies to the whole surface; nothing below names a specific
command or flag.

## Design principles

- **Single binary, two modes.** `awman` with no arguments launches a
  Ratatui TUI. `awman <subcommand> …` runs one command and exits, with
  output on stdout/stderr.
- **Catalogue-driven.** Every command, subcommand, flag, default, and
  argument lives in `CommandCatalogue`. Frontends (CLI, TUI, API) project
  from it; none of them hard-codes a command name, flag name, or default —
  see the module-level doc on `src/command/dispatch/projections/`. A
  convention that can't be expressed as catalogue data (a flag's kind,
  default, `implies`/`conflicts_with`, or frontend visibility) doesn't
  belong in a frontend either; it belongs in a new catalogue field.
- **Container isolation.** Every agentic operation runs inside a container
  built from `Dockerfile.dev`. The host never executes agent code directly
  — see `aspec/architecture/security.md`.

## Naming and casing

- Long flag names are lowercase, kebab-case, and describe the value or
  toggle, not the implementation (`--skip-verification`, not `--no-tls-chk`).
- A single-character short alias is reserved for flags common enough to
  type constantly (non-interactive mode, skip-confirmation, follow-logs).
  A short alias is never reused for a different meaning on a different
  command.
- A boolean flag is presence-only: `--foo` sets it, its absence doesn't;
  there is no `--foo=true`/`--foo=false` form.
- A value that repeats is a repeatable flag (`--foo a --foo b`), never a
  single flag with a delimited list — delimiters force the frontend to own
  parsing rules the catalogue can't express as a value kind.
- An enum-valued flag's accepted values live in the catalogue as data (its
  `FlagKind::Enum` payload) and are rendered wherever the flag is
  documented or completed; they are never duplicated as a string in a
  frontend or in prose here.

## Positional vs. flag

- A required, identifying value (a path, a name, a number the command
  can't run without) is a positional argument.
- Everything else — toggles, overrides, output shaping — is a flag, always
  optional at the catalogue level even when a command's own validation
  requires it in practice (so the same flag can be optional for one
  subcommand and required for another without two catalogue entries).

## Cross-cutting flag behavior

- `--json` always implies non-interactive mode: a machine-readable output
  mode can never block on a prompt. Any flag that changes output shape for
  scripting implies the same.
- Non-interactive mode suppresses every prompt. Where a prompt would have
  supplied a required decision and none was given, the command refuses
  with a usage error instead of guessing or blocking on stdin.
- A flag that forces one outcome and thereby makes another flag's request
  impossible to honor is declared as a conflict (`conflicts_with`) rather
  than silently overridden; a flag that forces a prerequisite on for
  correctness is declared as an implication (`implies`) rather than
  silently required.

## Exit codes

Exit codes are classes of outcome, not per-command codes:

| Exit code | Class |
|---|---|
| 0 | Success. |
| 1 | Runtime failure inside a lower layer (engine, data, transport). |
| 2 | Invalid usage: bad flag value, missing required input, a conflict, or required interactive input unavailable in a non-interactive context. |
| 3 | Container runtime unavailable. |
| 4 | A referenced resource (file, work item, template) does not exist. |
| 130 | Aborted by the user (Esc in the TUI, Ctrl-C on the CLI). |

A command that introduces a new failure mode maps it to the class it
belongs to rather than inventing a new code.

## Prompt and dialog conventions

- Every interactive choice shown to the user — a CLI stdin prompt or a TUI
  modal alike — is described by one `Prompt<D>`-shaped value that Layer 2
  owns: a title, an optional body, an ordered list of typed choices (each
  with a key, a label, and a value), and an explicit default returned when
  the user dismisses the prompt without choosing (or no default, which
  makes dismissal an abort). A frontend renders that shape — mapping a
  keystroke or a button to a choice's value — and holds no label, hotkey,
  or default of its own.
- A confirmation for a destructive or irreversible action always has a
  flag that skips it for scripting, and refuses instead of guessing when
  neither a TTY nor that flag is available.
- Dismissing a prompt (Esc, or a "No"/"Cancel" choice) never partially
  applies the action it was confirming.

## Hint and help-text style

- A command or flag's help text is one sentence: capitalized start,
  trailing period, describing the effect, not the implementation.
- A side effect that isn't obvious from the flag's name — an implied flag,
  a value it forces, a frontend it's hidden from — is stated inline in
  that same sentence, not left for the reader to infer from behavior.
- Long-form help (a second paragraph of context, only where the one-line
  summary isn't enough) is separate catalogue data from the summary, never
  a concatenation the frontend builds itself.

## Command-surface hygiene

- An unrecognized command path is corrected against its actual siblings
  (not a hardcoded top-level list) within a small edit-distance threshold,
  and reported as unknown — never silently guessed — outside that
  threshold. The catalogue owns this lookup so every frontend suggests the
  same correction from the same source.
- A retired flag is recognized before parsing and answered with a
  migration hint, not clap's generic unrecognized-argument error.

## Frontend visibility and parity

- Three frontend kinds exist: CLI, TUI, and API. A command or flag's
  visibility across them is catalogue data, not a frontend-side branch —
  see the visibility field on each catalogue entry and its projection into
  `docs/14-command-reference.md`.
- An interactive or PTY-bound command is excluded from the API frontend as
  long-term policy: an HTTP request cannot safely own a PTY's terminal
  lifecycle for the duration of the command. This exclusion is expressed
  once, at the catalogue entry, never re-derived per frontend.
- Where the same decision is available from more than one frontend, its
  label, default, and behavior are identical — verified by parity tests
  that assert against the catalogue and the shared prompt/dialog data, not
  against a second copy of the literals.

## Output and configuration

- Human-readable output goes to stdout, diagnostics to stderr. A
  machine-readable output mode replaces the human renderer entirely rather
  than appending structured data alongside it.
- The TUI takes over the terminal via Ratatui; a container's own PTY
  output is forwarded through it, ANSI escapes and all.
- Configuration resolves in one order, highest precedence first: an
  explicit flag, then an environment override, then repo-scoped config,
  then global config, then the catalogue's built-in default. A command
  never reads configuration through a path that skips a level of this
  order.
