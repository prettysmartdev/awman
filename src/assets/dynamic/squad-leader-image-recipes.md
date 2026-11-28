## This task's image recipes

Every container this task starts — yours and every step of the workflow you
generate — runs an image awman builds from this task's own recipes:

- `{{project_recipe}}` — the base image (tools every agent shares).
{{agent_recipes}}

They live in your durable task workspace and belong to this task. When the
task keeps needing a tool, add it to these recipes instead of installing it
again in every run.

Recipe edits take effect only when awman next prepares images, which happens
**at the start of the next run**. This run's leader (you) and every step of
the workflow you generate keep the images already prepared for this run, so
never plan a step that depends on a recipe edit made during this run. A
changed `Dockerfile.dev` rebuilds the base and every agent image; a changed
agent recipe rebuilds only that agent. If a rebuild fails, the next run keeps
its previous image and reports it as stale. Files a recipe copies (`COPY` /
`ADD`) are read from `/awman/context/workflow`, its build context.

Recipe edits are different from workspace changes: files you or a step change
under `{{repo_mount_path}}`, or anywhere else in `/awman/context/workflow`, are
visible immediately to everything that runs after the change. Only the image
contents wait for the next run.
{{same_directory_note}}
