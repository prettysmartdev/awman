# Parity Review Notes: 0063 Overlays Part 1

Date: 2026-04-28
Reviewer: Claude Code (automated parity review)
Status: Minor/trivial issues for future cleanup

These issues were identified during the parity review but are **not critical, high, or medium** priority.
Critical, high, and medium issues were fixed directly during the review.

---

## Minor Issues

### M1: `resolve_overlays_once` is now redundant

**Location:** `src/tui/state.rs` — `TabState::resolve_overlays_once`

**Description:** `resolve_overlays_once` was the original method for populating
`resolved_overlays` in `TabState`. During the 0063 parity review, a new
`resolve_and_cache_overlays` method was added that properly handles per-command
`--overlay` flags and returns `Result`. The original `resolve_overlays_once` is
now only used in `TuiContainerLauncher::run_audit` (the audit container launch),
where it is called with an empty flags slice.

`resolve_overlays_once` still works correctly for its remaining use case, but
it's worth consolidating to avoid two parallel methods that do similar things.

**Suggestion:** Replace the remaining `resolve_overlays_once` call in
`run_audit` with a direct `if let Ok(v) = resolve_overlays(git_root, &[])` and
then remove `resolve_overlays_once` from `TabState`. The `resolve_and_cache_overlays`
method covers all non-audit callsites already.

---

### M2: Single `--overlay` value in TUI (no multi-flag repeat)

**Location:** `src/tui/flag_parser.rs`, `src/tui/mod.rs` usage strings

**Description:** The TUI's flag parser stores one value per flag in a `HashMap`.
CLI mode allows `--overlay` to be repeated (`--overlay A --overlay B`). In the
TUI, repeating `--overlay` would overwrite the first value with the second.

The correct TUI workaround is comma-separated syntax in a single flag value:
`--overlay "dir(/a:/b:ro),dir(/c:/d:rw)"` — this is parsed correctly because
`resolve_overlays` joins the raw flags with comma before parsing.

However, the TUI usage string (`[--overlay=<SPEC>]`) does not explain this
limitation or the comma-separated workaround. Users who try to repeat `--overlay`
in the TUI will silently get only the last value.

**Suggestion:** Update the TUI usage strings for `implement`, `chat`, `exec
prompt`, and `exec workflow` to mention the comma-separated syntax, e.g.:
`[--overlay=<SPEC[,SPEC...]>]` and/or add a note to the TUI help output.

---

### M3: `AMUX_OVERLAYS` env var not visible in TUI help

**Location:** TUI `help` command output

**Description:** The `AMUX_OVERLAYS` environment variable is automatically read
and applied by `resolve_overlays`, but it is not mentioned in any TUI help text.
Power users who set `AMUX_OVERLAYS` in their shell profile will get the benefit
automatically, but there is no discovery path for users who read TUI help.

**Suggestion:** Add a line about `AMUX_OVERLAYS` to the TUI `help` output (the
same place that documents `AMUX_AUTH_*` and similar env vars, if such a section
exists). Low priority since the env var works correctly without any documentation.

---

### M4: Apple Containers runtime overlay support not verified

**Location:** `src/runtime/apple.rs`

**Description:** The spec (section 5, "Edge Case Considerations") states that
overlays must work equally on the Apple Containers runtime. The `DockerRuntime`
calls `append_overlay_mounts` via `append_settings_mounts`. It was not verified
during this review whether `AppleContainersRuntime` (if implemented) also calls
`append_settings_mounts` or otherwise propagates `HostSettings.overlays`.

**Suggestion:** When Apple Containers support is active, verify that
`run_container_pty` and `run_container_text` in `apple.rs` pass overlay mounts
to the container. Add a parity test analogous to `overlay_flag_present_in_implement_spec`
that checks `AppleContainersRuntime` includes overlay mounts in its run args.

---

### M5: `Option<String>` vs `Vec<String>` overlay type in TUI `PendingCommand`

**Location:** `src/tui/state.rs` — `PendingCommand` overlay field

**Description:** CLI uses `Vec<String>` for `--overlay` (repeatable flag via
clap). TUI stores `overlay: Option<String>` — a single comma-separated string.
This is a deliberate simplification (the TUI flag parser can only hold one value
per flag), but it means the types diverge between modes. When TUI wraps the value
in `vec![s.to_string()]` before calling `resolve_and_cache_overlays`, the behaviour
is functionally correct, but the type mismatch could confuse a future developer
adding new overlay handling.

**Suggestion:** Consider either (a) upgrading the TUI flag parser to collect
repeated flags into a `Vec`, or (b) document the intentional simplification with
a comment at the `overlay: Option<String>` field declaration.

---

## Trivial Issues

### T1: `resolve_overlays` re-reads `AMUX_OVERLAYS` on every call

**Location:** `src/overlays/mod.rs` — `resolve_overlays`

**Description:** Each call to `resolve_overlays` reads `std::env::var("AMUX_OVERLAYS")`.
In the TUI, this means the env var is re-read for every command launch. This is
correct (env vars can technically change between calls) but redundant in practice.

**Suggestion:** No action needed. If profiling ever shows env-var reading to be a
hotspot, cache the result in `TabState::resolved_overlays` for the env component
only. Not worth doing preemptively.

---

### T2: `HostSettings::overlays_only` construction is untested

**Location:** `src/runtime/mod.rs` — `HostSettings::overlays_only`

**Description:** The `HostSettings::overlays_only` constructor is used in
`src/commands/exec.rs` and `src/commands/implement.rs` when `host_settings` is
`None` but overlays need to be added. This code path is correct but has no
dedicated unit test. The broader integration test for overlay flag propagation
(see `0063-overlays-part-1.md` integration test descriptions) would cover it
implicitly.

**Suggestion:** Add a unit test for `HostSettings::overlays_only` confirming that
`host_settings.overlays` equals the input when other fields are at their defaults.
