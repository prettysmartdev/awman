# Code agents are bad at Software Architecture - for now.

Hello from paternity leave, week two. There won't be an amux release this week because I've gotten so fed up with the patchwork design of the amux codebase I've decided to burn it all to the ground, more or less. Even top-end foundation models like Opus are not (yet) truly good at software architecture. I know that's rich coming from someone whose job title is "Software Architect", and I don't claim that my role won't be overtaken soon - probably in the next 12 months - but as of right now, it's true.

---

## Agents and architecture

The first 7 major releases of amux were about ~85% written by Claude and the rest by me. I didn't create any massive components but I was going into specific modules that Claude created and verifying or re-writing the security-sensitive portions like container management, API token auth, etc. I come from a security background and so doing proper human validation of those parts was important to me (and I wanted to continue building my Rust capabilities).

The thing is, while agents are genuinely good at writing code, they suck at seeing the big picture. Given a proper spec and a well-understood scope, an agent in 2026 can produce solid Rust, pass tests, handle edge cases, and keep things idiomatic. What agents are *not* good at is doing things the "right" way for long-term codebase health. The higher-order thinking about how pieces of a codebase fit together over time is completely lost on them. Reasoning about abstractions, modularity, and structural guarantees never seems to be what they focus on. 

That shortcoming may or may not matter for what you're doing. If your agent is working in a codebase where architectural patterns are already well established and documented, it will likely do fine most of the time. Fully agent-driven architecture however quietly accumulates little sins: a `pub fn` here that should have been a method on a struct, a piece of business logic that ended up in the wrong module because it was convenient, a config value resolved in two different places in slightly different ways. None of these things individually are a crisis, but together, over sixty-plus work items, they become a rotten crumbly foundation.

---

## The specific problem

amux has three frontends: the TUI (the interactive terminal interface), the CLI (single-shot commands for scripting), and the API (the headless HTTP server for remote/cluster use). All three are supposed to do the same things; start an agent container, run a workflow, manage sessions, etc. Each of these frontends are supposed to be "just different interfaces on top of amux's core". I explained this to my gaggle of agent ducklings several times.

What actually happened is that each frontend grew little stalagmites of business logic over time. The TUI knew how to start a container by calling into a particular set of internal crates in a particular order. The CLI did roughly the same thing but through slightly different code paths, with slightly different flag resolution. Headless did it a third way ("just shell out to the CLI"). When I added new features like the `--model` flag or multi-agent workflow steps, I had to be sure they got wired through all three frontends, and there was no structural guarantee they'd behave the same. Sometimes they did. Sometimes they didn't.

Config became a spaghetti nightmare. `config/mod.rs` grew to over 1,600 lines of scattered `effective_*` free functions, each resolving a different config value through a tangle of merge rules that lived in no single authoritative place. Want to know what the effective `envPassthrough` value is? You call a free function. Want to set it? You call a different one. Want to display it? Something in the CLI reads it another way. The three frontends would each call their preferred subset of these functions, with no guarantee they were calling the right ones.

---

## Putting my foot down with the 'grand architecture'

The grand architecture is a manifesto I wrote berating my agents for their decision making and forcing a reorganization of amux into four strict layers:

- **Layer 0: Data** — everything that can be stored on disk or passed between components. Config, sessions, workflow state, filesystem paths. No business logic. No container calls. Just typed data.
- **Layer 1: Engine** — the core capabilities: container runtime, workflow execution, git operations, auth passthrough, overlay management. No UI. No CLI parsing. Just engines.
- **Layer 2: Command** — the business logic for every amux command (`chat`, `exec`, `init`, `ready`, etc.). Each command is a typed object built from lower layers. A `Dispatch` type routes requests to commands and generates frontend-specific data (like `clap` definitions or TUI hint strings) from a single canonical source.
- **Layer 3: Frontend** — the TUI, CLI, and Headless modes each become pure presentation layers. They receive inputs and emit outputs. They are structurally *forbidden* from containing business logic.

The key tenet: lower layers never call upward. The CLI, TUI, and Headless are just three different faces of the same Layer 2 machinery. Any command you can run from the TUI, you can run from the CLI or the API — guaranteed by construction, not by "we tried to make them match."

The other tenet that the old code violated constantly: prefer typed objects over free functions. Instead of `pub fn run_container_with_these_twelve_params(...)`, you get a `ContainerRuntime::builder()` that takes typed options and produces a `ContainerExecution` that any frontend can run with its own I/O sink. Session state is a `Session` struct with typed methods, not a `TabState` in the TUI and a loose struct in headless and an inferred `current_dir` in the CLI.

---

## Why this is worth burning a pile of Opus tokens

The refactor is running as a multi-agent, multi-stage workflow across eight work items (0066–0073). Old-amux is actively building new-amux. Each work item is a multi-hour agent workflow: thousands of lines of carefully specified Rust, comprehensive unit tests, strict layer-boundary enforcement, compatibility inventories that validate on-disk JSON schemas byte-for-byte so existing user configs don't break. I think that skipping a release week is worth it given the long-term benefits this will create.

The alternative is continuing to build features on a foundation that I *know* is going to cause problems. Every new capability I add to a codebase with this kind of architectural debt costs more than the one before, because the surface area of the bad abstractions keeps growing. This is the kind of thing that an agent will happily do, and will tell you it believes everything is correct despite the rot.

More importantly: the amux frontends I want to build next don't fit in the current architecture at all. Things like a desktop app, VS Code and Zed extensions, and a Kubernetes operator that can schedule agent workloads across a cluster. Each of these is a new "frontend" over the same amux core — and every one of them would require re-implementing the business logic stalagmites to make them work under the old structure, with all the same parity drift problems that made this refactor necessary in the first place.

The grand architecture is the thing that makes those futures possible without rebuilding amux N more times.

---

## Where things stand

Layers 0, 1, and 2 are complete and Layer 3 is in progress. After each work item I have a "holistic review" step which pits two agents (Opus and GPT) against each other to find any structural issues, functionality gaps, or architecture violations in the current implementation or future plans, which forces the agents to evolve the plan based on its findings during implementation. It's working well thus far but is essentially a ground-up rewrite so it's taking time.

That's the plan. I'll see you next week with a v0.8 that ships all of this and probably a handful of new things on top.

---

Source and issues at [github.com/prettysmartdev/amux](https://github.com/prettysmartdev/amux). More at [prettysmart.dev](https://prettysmart.dev). Feedback and contributions welcome.
