---
name: blog-post
description: Write a blog post in the preferred style related to amux
---

# Skill: blog-post

## Description

Write a new blog post for amux in the established first-person, problem-driven style. Posts live in `docs/blog/` and are numbered sequentially.

## Body

### Step 1: Understand the topic

If the user has not specified a topic, ask them: "What should the blog post be about?" Accept a feature name, a release version, a theme, or a free-form description. Do not invent a topic.

If the user says "the latest release" or similar, run:

```bash
git log --oneline -10
ls aspec/work-items/ | sort -r | head -10
cat Cargo.toml | grep '^version'
```

Read the relevant work items in `aspec/work-items/` to understand what changed.

### Step 2: Read existing blog posts for style calibration

Read the two most recent posts in `docs/blog/` (highest-numbered files):

```bash
ls docs/blog/*.md | sort -r | head -3
```

Read at least two of them fully. Do not skim. You are calibrating voice, length, and structure — not extracting facts.

Also read `docs/blog/0001-announcement.md` if the topic is introductory or architectural.

### Step 3: Determine the next post number and slug

```bash
ls docs/blog/*.md | sort -r | head -1
```

Increment the four-digit prefix by one. Choose a short, lowercase, hyphenated slug that names the theme, not the release version (e.g. `grand-refactor`, `headless-remote`, `specs-and-status`). Do not use version numbers as slugs unless the post is a general announcement.

The filename is: `docs/blog/NNNN-slug.md`

### Step 4: Draft the post

Follow this structure exactly:

```markdown
# amux X.Y: Short title     ← or a plain title if not release-tied

<one or two sentences — the problem or itch, not the solution>

---

```sh
# install or upgrade
curl -s https://prettysmart.dev/install/amux.sh | sh
```

---

<main body: 2–4 named sections using ## headers>

---

<one short paragraph: where things stand, what's next, or a frank admission>

---

Source and issues at [github.com/prettysmartdev/amux](https://github.com/prettysmartdev/amux). More at [prettysmart.dev](https://prettysmart.dev). Feedback and contributions welcome.
```

### Step 5: Apply the style rules

**Voice and tone**
- Write in first person ("I built this...", "I've been wanting...", "I decided to...").
- Open with the problem or friction point — never open with the solution.
- Explain *why* the feature or change matters before explaining *what* it does.
- Be direct. Use plain language. No hedging, no marketing copy.

**What to avoid**
- No buzzwords: "revolutionary", "game-changing", "seamless", "robust", "powerful", "exciting"
- No fluff openers: "In this post I will...", "I'm excited to announce...", "Today we're launching..."
- No long calls to action at the end — just the two-line pointer to GitHub and prettysmart.dev
- No passive voice when active voice works

**What to include**
- Shell examples with `sh` code blocks for any commands a reader would run
- Screenshot placeholders (e.g. `![TUI showing the new dialog](images/NNNN-slug-01.png)`) when a visual would help — do not attempt ASCII art
- The install snippet (`curl -s https://prettysmart.dev/install/amux.sh | sh`) in the first third of the post, inside a `---` fenced section
- Concrete "before vs. after" framing when the post is about a fix or refactor

**Length**
- Target 400–600 words in the body (not counting headers, code blocks, or the install snippet).
- If you exceed 600 words, you are over-explaining. Cut the section that reads most like documentation and link to `docs/usage.md` instead.

### Step 6: Write the file

Write the post to `docs/blog/NNNN-slug.md`. Do not create any other files.

Do not summarize what you wrote or list what sections you included. The user will read the file.

### Decision tree

```
Did the user specify a topic?
  Yes → use it
  No  → ask before writing anything

Is the topic tied to a specific release version?
  Yes → include "amux X.Y:" in the title
  No  → use a plain descriptive title

Does the topic involve new commands or flags?
  Yes → include at least one shell code block demonstrating them
  No  → skip the install blurb if no new behavior exists for users to try
        (but still include the install line if it's a release post)

Is the post longer than 600 words?
  Yes → find the most documentation-like section and cut or shorten it
```
