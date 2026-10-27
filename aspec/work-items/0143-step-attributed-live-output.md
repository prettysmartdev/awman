# Work Item: Feature

Title: Step-attributed, live output for API clients
Issue: issuelink

## Summary:
- Over the API, a workflow command's output is one undifferentiated stream. `ExecutionEvent::StdoutLine(String)` and `StderrLine(String)` carry no step, container or attempt, so a client cannot tell which agent of a parallel group printed a line. The TUI does not have this problem because it reads each container's PTY directly (`container_view.rs`, `Ctrl-S` to cycle the focused slot).
- Agents also run non-interactively in API mode (for example, `claude --print`). Their output therefore arrives as one final message just before the container exits, not live. In a real infact run, the `docs` step's whole output (22 lines) arrived after its work was done, while `tests` was still running.
- Tag every output event with its origin, and give non-interactive agents a live activity stream, so an API client can build the equivalent of the TUI's Workflow Overview and container panes.

## User Stories

### User Story 1:
As a: API client rendering a running workflow (e.g. infact's workflow view, infact work item 0002)

I want to:
receive `StepOutput { step_name, step_index, container, stream, line }` for every line, and `StepActivity` events while an agent works

So I can:
show each agent's own output live, and switch between the parallel agents of a group the way the TUI does

## Implementation Details:
- New `ExecutionEvent` variant:

  ```
  StepOutput {
    step_name: Option<String>,   // None for leader / setup / teardown shell output
    step_index: Option<u32>,
    phase: String,               // leader | setup | main | teardown
    container: Option<String>,   // runtime container name
    attempt: u32,                // on_failure retries and restarts
    stream: "stdout" | "stderr",
    line: String,
  }
  ```

  Emit it from the same place `StdoutLine` is emitted today, with the context the workflow engine already has when it launches a step (`WorkflowParallelStepLaunched` knows the step and agent). Keep `StdoutLine`/`StderrLine` for one release, then drop them behind the API's additive-versioning rule.
- Live activity for non-interactive agents. Where the agent supports a streaming output format (claude `--output-format stream-json --verbose`, codex JSON events), run it in that mode under the API and translate its events into:

  ```
  StepActivity { step_name, step_index, kind: "message" | "tool_use" | "tool_result" | "thinking", summary: String }
  ```

  Keep the final message as `StepOutput` lines as today. Agents without a streaming mode emit only `StepOutput`. This is a capability in the agent table (`AgentDefinition`), not a name match.
- `GET /v1/workflows/{command_id}` already returns `steps[]` with `depends_on`, `agent` and `model`. Add `stages: [[step_name…]…]`, computed by the same grouping the TUI's Workflow Overview uses, so clients do not re-implement it. Also add each step's `container` and `attempt`.
- Optionally, `GET /v1/commands/{id}/logs?step=<name>` filters the replay to one step.

## Edge Case Considerations:
- Output from the leader and from shell setup/teardown steps: `step_name: None`, `phase` set.
- An `on_failure` retry of a step: same `step_name`, with `attempt` incremented.
- A command re-run after an awman restart: covered by `attempt`, together with 0136's attempt numbering.
- Very chatty streaming agents: `StepActivity.summary` is a short one-line summary, not the full tool payload, to bound event volume.

## Test Considerations:
- A parallel group of two fake agents writing interleaved lines: every `StepOutput` carries the right `step_name` and `container`.
- The streaming translation for claude's `stream-json`, against recorded fixtures.
- `stages` in `/v1/workflows` matches the TUI overview's grouping for linear, fan-out/fan-in and setup/teardown workflows.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation
- `docs/09-api-and-remote-mode.md`: event reference (`StepOutput`, `StepActivity`) and `stages` in `/v1/workflows`.
