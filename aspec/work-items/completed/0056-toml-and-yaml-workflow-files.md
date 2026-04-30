# Work Item: Enhancement

Title: TOML and YAML workflow files
Issue: issuelink

## Summary:
- Extend the workflow file format to support TOML and YAML in addition to Markdown
- All workflow fields (`Step`, `Depends-on`, `Agent`, `Model`, `Prompt`) must be representable in each format
- Format is detected by file extension (`.md`, `.toml`, `.yml`/`.yaml`)
- Add example TOML and YAML versions of `implement-preplanned.md` in `aspec/workflows/`


## User Stories

### User Story 1:
As a: user

I want to: define workflow files in TOML or YAML format

So I can: use structured, machine-editable config syntax for workflows instead of a custom Markdown dialect, making them easier to generate, lint, and integrate with other tooling.

### User Story 2:
As a: user

I want to: pass a `.toml` or `.yaml` workflow file to the `--workflow` flag exactly as I would a `.md` file

So I can: use any supported format interchangeably with no behavioral differences in execution, resumption, or validation.

### User Story 3:
As a: user

I want to: have clear example workflow files in TOML and YAML beside the existing Markdown examples

So I can: quickly learn the syntax of each format and use them as templates for my own workflows.


## Implementation Details:

### Workflow data model (no changes needed)
The `WorkflowStep` struct (`src/workflow/parser.rs`) and `WorkflowState` (`src/workflow/mod.rs`) are format-agnostic and do not need to change.

### New parser module strategy
Introduce a top-level dispatch function in `src/workflow/parser.rs` (or a new `src/workflow/format.rs`) that selects a parser based on the file extension:

```
.md            → existing parse_workflow() markdown parser (unchanged)
.toml          → new parse_workflow_toml()
.yml / .yaml   → new parse_workflow_yaml()
(no extension / unknown) → return an explicit error
```

The dispatch should live in `load_workflow_file()` (`src/workflow/mod.rs`, currently calls `parse_workflow()` directly) so all callers transparently get multi-format support.

### TOML schema
Use the `toml` crate (already a common Cargo dependency; add if absent).

```toml
title = "Implement Feature Workflow"   # optional

[[step]]
name = "implement"
prompt = """
Implement work item {{work_item_number}}...
"""

[[step]]
name = "tests"
depends_on = ["implement"]
prompt = """
Implement tests...
"""

[[step]]
name = "review"
depends_on = ["docs", "tests"]
agent = "codex"
model = "claude-opus-4-6"
prompt = """
Review...
"""
```

- `title` — optional string at top level
- `[[step]]` — array of tables, preserves ordering
- Each step: `name` (required string), `prompt` (required string), `depends_on` (optional array of strings), `agent` (optional string), `model` (optional string)
- Deserialize into an intermediate serde struct, then convert to `Vec<WorkflowStep>`

### YAML schema
Use the `serde_yaml` crate (add if absent).

```yaml
title: "Implement Feature Workflow"   # optional

steps:
  - name: implement
    prompt: |
      Implement work item {{work_item_number}}...

  - name: tests
    depends_on: [implement]
    prompt: |
      Implement tests...

  - name: review
    depends_on: [docs, tests]
    agent: codex
    model: claude-opus-4-6
    prompt: |
      Review...
```

- `title` — optional string
- `steps` — sequence, preserves ordering
- Each item: same fields as TOML above
- `depends_on` may be a YAML sequence or omitted entirely

### Parsing implementation steps
1. Add `toml` and `serde_yaml` to `Cargo.toml` dependencies.
2. Create serde-deserializable structs (e.g., `TomlWorkflow`, `YamlWorkflow`, `RawStep`) that mirror the schema above. These are internal; they convert into the existing `WorkflowStep` type.
3. Implement `parse_workflow_toml(content: &str) -> Result<(Option<String>, Vec<WorkflowStep>)>` in `parser.rs`.
4. Implement `parse_workflow_yaml(content: &str) -> Result<(Option<String>, Vec<WorkflowStep>)>` in `parser.rs`.
5. Add `detect_format(path: &Path) -> WorkflowFormat` (enum `Markdown | Toml | Yaml`) using `.extension()`.
6. Update `load_workflow_file()` in `mod.rs` to call `detect_format()` and dispatch to the right parser. Everything downstream (hashing, validation, DAG checks, state persistence) remains unchanged.

### Example files
Create two new files in `aspec/workflows/`:
- `implement-preplanned.toml` — TOML translation of `implement-preplanned.md`
- `implement-preplanned.yaml` — YAML translation of `implement-preplanned.md`

Both must contain all four steps (`implement`, `tests`, `docs`, `review`) with identical prompt content, `depends_on`, and `agent` fields as the original Markdown version.


## Edge Case Considerations:
- **Unknown extension**: if the file extension is not `.md`, `.toml`, `.yml`, or `.yaml`, return a clear error: `"unsupported workflow format: expected .md, .toml, .yml, or .yaml"`.
- **Missing `name` field**: TOML/YAML steps without a `name` key should produce a descriptive parse error including the step index.
- **Missing `prompt` field**: same — error with step name if available, or index if not.
- **Empty `steps` array**: mirror the existing Markdown behaviour and return `"workflow file contains no steps"`.
- **`depends_on` as a string instead of array**: YAML allows `depends_on: implement` (bare string) — either accept it as a single-element list or emit a clear type error; do not silently ignore it.
- **Multiline prompts**: TOML triple-quoted strings and YAML literal blocks (`|`) must preserve newlines and not collapse whitespace; verify `{{work_item_section:...}}` template variables survive round-trip through both parsers unchanged.
- **Hash stability**: the SHA-256 hash used for resume/restart detection is computed over the raw file bytes, so TOML/YAML files are treated identically to Markdown — no special handling needed.
- **State file naming**: the workflow file stem (used in `.amux/workflows/<hash>-<witem>-<stem>.json`) is derived from `Path::file_stem()`, which is format-agnostic; no change needed.
- **Case sensitivity**: field names in TOML/YAML are lowercase (`name`, `depends_on`, `agent`, `model`, `prompt`); document clearly that uppercase variants are not accepted.
- **Extra unknown fields**: use `#[serde(deny_unknown_fields)]` for strict validation so typos like `dependson` surface as errors rather than being silently dropped.
- **BOM / encoding**: `std::fs::read_to_string` handles UTF-8; explicitly reject or strip a UTF-8 BOM if present to avoid parse failures.


## Test Considerations:
- **Unit tests in `src/workflow/parser.rs`**:
  - `parse_workflow_toml` happy path: all fields present, multiline prompt, multiple steps with dependencies.
  - `parse_workflow_yaml` happy path: same.
  - `parse_workflow_toml` with no `title` field (optional): steps still parse correctly.
  - `parse_workflow_yaml` with `depends_on` as YAML sequence and as omitted key.
  - Missing `name` field → error.
  - Missing `prompt` field → error.
  - Empty `steps` / `[[step]]` array → error matching Markdown equivalent.
  - Unknown field → error (deny_unknown_fields).
  - TOML triple-quoted prompt preserves embedded newlines and `{{template_vars}}`.
  - YAML literal block prompt preserves embedded newlines and `{{template_vars}}`.

- **Unit tests in `src/workflow/parser.rs` for format detection**:
  - `.md` → `WorkflowFormat::Markdown`
  - `.toml` → `WorkflowFormat::Toml`
  - `.yml` → `WorkflowFormat::Yaml`
  - `.yaml` → `WorkflowFormat::Yaml`
  - `.json` (unsupported) → error

- **Integration tests** (follow patterns in existing test suite):
  - `load_workflow_file()` on a `.toml` path parses successfully and produces the same `WorkflowStep` list as the equivalent `.md` file.
  - `load_workflow_file()` on a `.yaml` path: same.
  - DAG validation (`validate_references`, `detect_cycle`) works identically after TOML/YAML parse.
  - Template variable substitution (`substitute_prompt`) works on steps loaded from TOML and YAML files.

- **Example file smoke tests**:
  - Parse `aspec/workflows/implement-preplanned.toml` without error; assert step count, names, and dependency structure match the Markdown original.
  - Parse `aspec/workflows/implement-preplanned.yaml` without error; same assertions.


## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- The existing `parse_workflow()` function in `src/workflow/parser.rs` must not be modified; new parsers are additive.
- Dispatch logic belongs in `load_workflow_file()` (`src/workflow/mod.rs` lines ~199-214), the single choke-point through which all workflow files pass.
- Use `serde` derive macros (`Deserialize`) on the intermediate structs; keep them private to `parser.rs` (or `format.rs`) — callers only see `WorkflowStep`.
- Add `toml` and `serde_yaml` to `[dependencies]` in `Cargo.toml`; pin to versions compatible with the existing Rust edition and `serde` version already in use.
- Do not add a new sub-crate; keep all workflow parsing within the existing `src/workflow/` module.
- Ensure `make test` (i.e., `cargo test`) passes with no regressions after the change.
