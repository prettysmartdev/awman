# Work Item: Feature

Title: Squad leader prompt states the task's exact overlay inventory
Issue: n/a

## Summary:
- The squad evaluation leader is told its task name, description, workspace
  mount and agent pool, but nothing about the **overlays** the task runs with.
  It therefore has to guess whether `GITHUB_TOKEN` is in its environment,
  whether `~/.ssh` is mounted, whether a data directory exists, or which
  skills it may call.
- Both failure directions are real. A leader that *assumes* a resource is
  present writes a workflow whose steps fail at runtime on a missing key or a
  missing path. A leader that *assumes* nothing is present writes a weaker
  workflow than the task could support — it will not push a branch, open a PR
  or read a mounted dataset because it never knew it could.
- `env()` overlays make this sharper than the other kinds: a directory overlay
  whose host path is missing fails fast before any container launches, and a
  named skill that cannot be resolved is reported before launch, but an
  `env(VAR)` the daemon has no value for is **silently absent** from the
  container. Only the daemon can tell the leader which names actually resolved.
- Fix: render the task's fully-merged overlay set into the leader prompt as an
  explicit inventory — directories with their container paths and permissions,
  environment variables split into *set* and *declared but not set*, skills,
  context directories, and the structural mounts that are always present.

## User Stories

### User Story 1:
As a: user

I want to: have the squad leader agent be told exactly which directories,
environment variables and skills its containers can reach

So I can: get generated workflows that use everything the task was given and
never depend on something the task does not have.

### User Story 2:
As a: user

I want to: see a declared `env(VAR)` that the daemon has no value for called
out to the leader as missing

So I can: have the leader route around it (or report it in the verdict reason)
instead of writing a step that dies on an unset variable, and so I know to run
`awman squad env --push`.

## Implementation Details:
- `overlay_inventory(collected)` — new Layer-2 helper in
  `src/command/commands/squad/overlay_summary.rs` — turns a `CollectedOverlays`
  into an `OverlayInventory`: five bullet lists (directories, env-set,
  env-unset, skills, context), and nothing else.
- Env presence is resolved with `crate::data::config::env::host_var`, the same
  function `resolve_env_passthrough` uses to build the `docker run` argv, so the
  prompt cannot disagree with what the container receives. In the daemon that
  reads the in-memory payload overlay, which is why only the daemon can decide
  it.
- **Names, never values.** The inventory prints names and their set/unset state,
  matching the rule the daemon's env state already holds itself to.
- `build_squad_leader_prompt` gains an `&OverlayInventory` parameter feeding five
  placeholders in `src/assets/dynamic/squad-leader-prompt.md`. The section always
  renders — "this task has no overlays" is exactly the fact the leader is
  missing today.
- `LocalTaskEvaluator::evaluate` collects the task's overlays once, before the
  prompt is built, and hands the same `CollectedOverlays` to
  `leader_run_options` for every repair attempt. Previously the collection ran
  once per attempt; now the prompt is built from the very set the leader's
  container is launched with.
- The inventory states that the same overlays reach every step of the generated
  workflow — already true: the task's overlays are passed to `exec workflow`
  through the `--overlay` slot.
- The stale-draft review checklist gains an overlay line: a `workflow.toml`
  carried over from an earlier run may name an `env()` value or directory the
  task no longer has — the failure this inventory exists to prevent, in the one
  place a reused draft hides it.

### Prose in the template, data in the code

Every heading and sentence lives in the template; the code contributes bullet
lists. This is a rule for the prompt assets generally:

- A prompt is prose and is edited as a whole. Wording split between a Markdown
  file and Rust string literals — whose `\`-continuations silently eat leading
  whitespace, a trap this work item hit while drafting — cannot be tuned in one
  pass.
- Placeholders get blank lines around them, so the rendered shape is visible in
  the template. Previously `{{max_concurrent_steps_note}}` and
  `{{developer_guidance}}` sat on adjacent lines, and the output was correct
  only because each rendered string carried a compensating leading `\n` — an
  invariant expressed nowhere and checked by nothing.
- A conditional block therefore renders `(none)` rather than an empty string.
  That keeps the spacing static, and here it is also the better content:
  absence is decision-relevant, and "no directories are mounted" is information
  where a vanished section is silence.

The rule reaches `leader-prompt.md` too, since `build_developer_guidance` is
shared with the WI-0092 work-item leader. Its `{{max_concurrent_steps_note}}` —
a whole sentence built in Rust, or an empty string — becomes
`{{max_concurrent_steps}}`, substituting `3` or `no limit` into a sentence the
template owns.

### Removing the dead `SQUAD_TASK:` marker

The prompt opened by requiring the leader to declare
`SQUAD_TASK: triggered|not_triggered` on the first line of its output. Nothing
has ever read that marker — it appears in the template and nowhere else in the
tree. It is a leftover from WI 0101, where the verdict *was* signalled out of
band (by writing or not writing `workflow.toml`); WI 0106 replaced that with the
run-scoped verdict file and renamed the marker from `AMIE_CONDITION:` without
re-examining whether it still had a job.

Leaving it was not neutral. It gave the most prominent position in the prompt to
the one signal that decides nothing, and it asked for the verdict *first*,
before the leader had investigated anything — so a leader that later concluded
otherwise had to contradict its own opening line, leaving a run log with two
conflicting statements and nothing marking which one was authoritative.

The marker is removed. The two rules that were riding along inside it — default
to not-triggered when the evidence is ambiguous, and do not produce a workflow
unless confident — are load-bearing and move into the verdict section, restated
in terms of the verdict file. That section now also says outright that nothing
else the leader writes or says is read as a verdict, and that the verdict is
written once, at the end, from the evidence actually gathered.

## Edge Case Considerations:
- No overlays at all: each list renders `(none)` rather than being omitted, so
  absence is stated rather than implied.
- `context(global)` / `context(repo)`: these reach the generated workflow's
  steps but **not** the leader's own container, which mounts only the durable
  task workspace. The template says so rather than over-promising.
- `context(workflow)`: listed as the durable task workspace, because that is
  what `with_task_workspace` retargets it to for both the leader and the steps.
- `/awman/squad/run` is leader-only — the steps of the generated workflow never
  see it — and is labelled as such.
- An `env(VAR)` set to the empty string counts as set, matching
  `resolve_env_passthrough`'s gate exactly.
- Values are never rendered, so a leader transcript or run log cannot leak one.

## Test Considerations:
- Unit tests on the renderer: directories with both permissions, set vs unset
  env split, `skill(*)` vs named skills, context scopes, the all-empty case,
  and the invariant that no env value ever appears in the output.
- A test that the rendered inventory survives substitution into the prompt
  template, that the template supplies the headings, and that the template
  retains no unreplaced placeholder.
- A test that the lists carry no headings and no prose — every line is a bullet
  or `(none)` — so the prose cannot drift back into the code without failing.
- The WI-0099 guidance tests change from asserting the section is *omitted* when
  guidance is absent to asserting it *states* its absence; likewise for the
  concurrency advisory.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture
  patterns from the project's aspec.

## Documentation
- `docs/12-squad.md`: note in the overlays/task-environment sections that the
  leader is told its overlay inventory, and that an unset `env()` name is
  reported to the leader as missing.
- `docs/08-overlays.md`: cross-reference from the `env()` "silently absent"
  paragraph.
