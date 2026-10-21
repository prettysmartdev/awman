---
name: release-prep
description: Prepare awman for a new release. Updates version numbers, docs, getting-started, release notes, and blog post. Does NOT run the release script.
---

# awman Release Prep Skill

Use this skill when the user asks to "prepare for release vX.Y.Z", "bump the version", or "get ready to release".

## What this skill does

1. Determine the new version from the user's request (e.g. `v0.3.0`)
2. Identify what changed since the last release (git log, new work items, new features)
3. Update all version numbers
4. Update documentation to reflect new features
5. Review and refresh the status-dashboard tips
6. Write release notes
7. Write a blog post

Do **not** run `make release` or push any tags. Stop before the release script.

---

## Step 1: Gather context

Run these in parallel:

```bash
# What changed since the last tag?
git log $(git describe --tags --abbrev=0)..HEAD --oneline

# What is the current version?
grep '^version' Cargo.toml

# What work items were implemented? (high-numbered ones are recent)
ls aspec/work-items/ | sort -r | head -10
```

Also read:
- `docs/releases/` — understand the format of prior release notes
- `docs/blog/` — understand the blog post style (first-person, problem-driven, no buzzwords)
- `README.md` — check if new commands or features need to be added
- `docs/00-getting-started.md` — check if new workflow steps need to be covered
- the relevant numbered doc under `docs/` (e.g. `docs/07-configuration.md`) — verify new subcommands/config fields are already documented (usually they are; added alongside the code)

---

## Step 2: Bump the version

**`Cargo.toml`** — the only place the version number lives:

```toml
version = "X.Y.Z"   # was the old version
```

Update to the new version. There is no `Cargo.lock` to worry about in this project (binary crate, not published to crates.io).

---

## Step 3: Update README.md

**Writing about squad:** squad is a *group of agents the reader owns*, not a daemon or a scheduler. Write "your squad is a group of agents that…", "create a squad to automate your work", "create a new task for your squad to tackle" — never "squad is a background daemon" or "automate work with squad". "squad daemon" stays only in lifecycle, auth, and API-conflict material (`squad start`/`stop`/`logs`, `AWMAN_SQUAD_KEY`), where it names the literal process and matches the literal error text a reader will hit.

Check for any new top-level commands or major features that aren't mentioned. The README has a "Commands" section — add new subcommands there. Keep it brief: one line per command.

If the feature is significant enough to have its own section (like `awman claws` got in v0.2), add one. Otherwise, a line in the commands table is enough.

---

## Step 4: Update docs/00-getting-started.md

The getting-started guide covers the first-time user workflow. Add any new commands that a new user would encounter in their first session. Skip internal implementation details; focus on workflow.

Common additions:
- New subcommands with a brief example
- New flags on existing commands that change the workflow
- New monitoring or status tools

---

## Step 5: Verify the numbered docs under docs/

The numbered guides (`docs/01-concepts.md` through `docs/15-parallel-workflows.md`) are usually kept up to date alongside the code. Verify that new subcommands or config fields are documented in the relevant one. If anything is missing, add it following the existing pattern:

```markdown
### `awman <command> [flags]`

Brief description of what it does.

**Flags**

| Flag | Values | Default |
...

**Examples**

```sh
awman <command>
```
```

---

## Step 6: Review and refresh the tips

The status dashboard shows one random tip from `TIPS` in `src/command/commands/status_tips.rs`. If that file has moved (for example to `src/command/commands/agents/tips.rs`), find it with `grep -rn "pub const TIPS" src`. Tips are a cheap way for users to discover features, so review the whole list every release:

1. **Remove or fix stale tips.** Check each tip against the current command surface (`docs/14-command-reference.md`), TUI keybindings (`docs/02-using-the-tui.md`), and the other numbered docs. Pay particular attention to keybindings, defaults (intervals, timeouts, ports), supported agents and runtimes, and anything renamed or removed since the last release.
2. **Add tips for what's new.** Each user-visible feature, command, flag or keybinding in this release (from Step 1) should get at least one tip if a user could benefit from discovering it. Put each tip in the matching topic group in the list.
3. **Keep the list comprehensive.** Every top-level command, and the main TUI keys, should be covered by at least one tip.
4. **Follow the tip style:**
   - One sentence describing what the user can do, not how it works internally.
   - Wrap commands, flags and paths in backticks, starting with `awman` (for example `` `awman clean --dry-run` ``).
   - 100 characters or fewer.
   - No duplicates.
   - Follow the squad wording rules in Step 3.
5. **Run the tips tests:**

```bash
cargo test --lib tips
```

The tests check every `` `awman …` `` command and flag a tip names against the `CommandCatalogue`, and reject duplicates and tips over 100 characters. A failure means a tip is stale: fix the tip, never the test.

---

## Step 7: Write release notes

Create `docs/releases/vX.Y.Z.md`:

```markdown
# Release vX.Y.Z

## Features

- **Feature name**: one or two sentences explaining what it does and why it matters.
  - Sub-bullet for sub-commands or options if relevant.

## Improvements

- **Improvement name**: what changed and why it's better.

## Fixes

- Fixed <symptom> in <context>.
```

Keep it factual and brief. No marketing language. Focus on what a developer using awman would care about.

---

## Step 8: Write a blog post

Create `docs/blog/NNNN-slug.md` where NNNN is the next number after the last post.

**Style guide** (derived from existing posts):
- First-person narrative ("I built this...", "I've been wanting...")
- Open with the problem or itch, not the solution
- Explain *why* the feature matters before explaining *what* it does
- Focus on the improved workflows, problems solved, and benefits to security that the tool brings rather than how it works internally
- Show examples with code/shell blocks (or screenshot placeholders for the human to fill in later, don't try to do ASCII art). Keep placeholders inside an HTML comment — see the blog-post skill — so the docs link check does not flag an image that does not exist yet
- No buzzwords ("revolutionary", "game-changing", "seamless", "robust")
- No fluff ("In this post I will...", "I'm excited to announce...")
- Inlcude a quick blurb on how to install the tool in the first 1/3 of the post (the curl|sh version)
- End with a pointer to the GitHub repo and prettysmart.dev website, not a long call-to-action paragraph. Mention that feedback, issues, contributions, etc are welcome.
- Length: 400–600 words. If it's longer, you're over-explaining.

Header format:
```markdown
# awman X.Y: Short title

<short intro>

---

<quick install>

---

<main body>

---

<short outro and links>

```

---

## Checklist

Before finishing, verify:

- [ ] `Cargo.toml` version updated
- [ ] `README.md` reflects new commands/features
- [ ] `docs/00-getting-started.md` covers new workflow steps
- [ ] relevant numbered doc(s) under `docs/` document new subcommands/config fields (check, don't assume)
- [ ] Status-dashboard tips reviewed: stale tips fixed, new features covered, `cargo test --lib tips` passes
- [ ] `docs/releases/vX.Y.Z.md` created
- [ ] `docs/blog/NNNN-slug.md` created
- [ ] Release script NOT run (user does that separately)
