# awman 0.14: one engine, five ways to work

As awman grew, I kept running into the same uncomfortable question: would a task behave differently depending on where I started it? A workflow launched from a terminal, a scheduled squad task, and a job sent to a worker should agree on what the task means, what it needs, and when it has actually succeeded. When each path grows its own logic, those answers drift.

v0.14 puts one command engine behind five awman experiences: the CLI, TUI, API, your squad, and workers. The point is practical. You can choose how work starts without learning five sets of rules for how it runs.

---

```sh
# install or upgrade
curl -s https://prettysmart.dev/install/awman.sh | sh
```

---

## At the keyboard: CLI and TUI

Some days I want one command I can put in a script. Other days I want to watch agents think, inspect a diff, and decide whether to retry a failed step. The CLI and TUI serve those different habits, but they now send work through the same command path. A work item, agent choice, workflow definition, and set of overlays mean the same thing in both places.

```sh
awman new spec
awman exec workflow --dynamic --work-item 0042
```

I can run that workflow from my shell or type the same command in the TUI. The CLI gives scripts an exit status and structured output where supported. The TUI gives me live terminals, the workflow overview, and controls for retrying or resuming a step. Neither needs its own version of the workflow rules. If a command needs a decision, an interactive run can ask me; an unattended run has to use an explicit answer or report why it cannot continue.

This also changes how I add features. `awman new` and `awman edit` can help author specs, workflows, skills, and agent definitions. Those commands do not need a special implementation for every screen that exposes them. I can start with a short work item description, run the task, then use `awman edit spec 0042 --amend` to reconcile the spec with what was built.

## For scripts and integrations: the API

An API is useful only if it can be trusted to do the same work I tested locally. In v0.14, API requests reach the same command dispatch used by the terminal experiences. A remote workflow still gets the same setup checks, isolated agent execution, workflow state, and result handling. API clients get typed responses, resumable events, cancellation, and idempotency keys instead of having to scrape a terminal transcript or guess whether a retry started duplicate work.

The long-running pieces now live behind `awman daemon`. One process manages the enabled engine, worker, and squad features, with status and logs in one place:

```sh
awman daemon start --background
awman daemon status
awman daemon logs --follow
```

That gives me a straightforward local path for a script or integration and an explicit network listener when I choose to configure one. The local listener is owner-authenticated; network access uses TLS and named API keys. A client can submit work without gaining an unlimited view of my machine.

## For recurring work: your squad

Your squad is a group of agents you own for tasks that keep coming back: issue triage, a failing PR, or a dependency update. I describe the condition and the work I want done, and the squad evaluates it on its schedule. When it acts, its agents use the same workflow engine and runtime as a task I launched by hand.

That consistency matters most when I am away. A scheduled run still has to check images, Git identity, credentials, and overlays before it starts. It cannot quietly skip a prerequisite because there is no person at the keyboard. I can inspect the task history, trigger one now, pause it, or attach to a running agent with the squad commands. The daemon handles its lifecycle, while `awman squad` remains the place to manage my group of agents and their tasks.

## For offered work: workers

A worker is for jobs that arrive from outside this machine. I can link awman to a work server, look at the queue, pick an item myself, or allow eligible items to be claimed automatically:

```sh
awman link https://work.example.com --name team
awman queue
awman queue pick
```

The worker does not invent a second way to run a workflow. It uses the same command engine, with capacity shared among pushed jobs, pulled jobs, and other daemon work. Each server can have its own job limit and labels. Secrets and overlays are reviewed for the particular item, and the resulting agents still run in isolated containers or microVMs. The experimental builtin microVM runtime now works on Linux x86_64 with KVM as well as Apple Silicon Macs, giving more machines that isolation option.

This release is a beta and changes some commands and checkout names; check the [release notes](../releases/v0.14.0.md) before upgrading an existing setup. The web app is still prerelease and is coming soon as another experience built on this engine.

---

Source and issues at [github.com/prettysmartdev/awman](https://github.com/prettysmartdev/awman). More at [prettysmart.dev](https://prettysmart.dev). Feedback, issues, and contributions are welcome.
