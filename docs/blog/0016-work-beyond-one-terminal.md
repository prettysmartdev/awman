# awman 0.14: work beyond one terminal

I've had a recurring annoyance with agent work: the task is running, but the terminal where I started it becomes the place I have to watch it. A terminal closes, I move to another screen, or a job arrives while I'm doing something else. Keeping track should be easier.

v0.14 brings the TUI, browser, API, squad, and workers onto a shared command engine and daemon. For me, that means a simpler routine: start work, check on it from another view, and decide what needs attention next.

---

```sh
# install or upgrade
curl -s https://prettysmart.dev/install/awman.sh | sh
```

---

## One place to see what is happening

I want a clear answer to a basic question: what is awman doing right now? Previously, background features had separate lifecycles. In v0.14 one daemon can host the engine, worker, and squad features. `awman daemon status` reports the features, listeners, and running jobs; `awman daemon logs --follow` shows what happened.

```sh
awman daemon status
awman daemon logs --follow
```

The TUI can use that engine too. If its frontend disconnects, an admitted command stays with the engine, and an authorized frontend can observe it after reconnecting. During a long workflow, a lost connection does not have to mean starting over or guessing whether it finished. Workflow controls still let me retry, step back, or resume when a step needs judgment.

Agent activity appears in tabs and container windows, and the same commands drive the CLI, TUI, and API. I can choose the view that fits the moment.

## A browser view of a local workspace

The new web app is for the times I want to look at an agent terminal, a squad task, or a work queue without living in the TUI. It is hosted at [awman.prettysmart.dev](https://awman.prettysmart.dev), but the work stays on my computer. The browser connects to the daemon's loopback listener on that same machine. It does not send my repo through the hosted site.

I enable the engine and local listener, register the repo as a workspace, then approve the code the app shows me:

```sh
awman config set daemon.features.engine.enabled true
awman config set daemon.network.listeners.loopback.enabled true
awman daemon start --background
awman daemon api launch_contexts.create default=true
awman link web-<code>
```

The approval matters. A browser cannot choose an arbitrary directory or quietly bring its own environment into a run. I register the workspace and approve the browser from that computer. The app then gives me live terminals, squad status, and a queue view. Reloading reconnects while authorization remains valid.

<!-- SCREENSHOT PLACEHOLDER: the web app showing a running agent terminal beside the work queue.
     Save it as docs/blog/images/0016-work-beyond-one-terminal-01.png and replace this comment with:
     ![Web workspace with an agent terminal and work queue](./images/0016-work-beyond-one-terminal-01.png) -->

## Let work come to this machine

I don't always want to start a task by copying an issue into a prompt. A linked work server can offer items to this machine. The worker can claim eligible items automatically within the limits I set, or I can use `awman queue` to see what is waiting and `awman queue pick` to start one. The TUI has a queue panel on **Ctrl+Q** too.

```sh
awman link https://work.example.com --name team
awman queue
awman queue pick
```

Your squad is still a group of agents you own for recurring work. The worker handles jobs offered by a server. Both use the daemon's execution capacity. Each server can have its own labels, allowed inputs, and job limit. Server work does not get a blanket copy of my environment; secrets and overlays are handled for the specific item.

## Spend time on the task, not its setup

The new engine is useful only if starting a task is dependable. `awman new` and `awman edit` now let an agent create or revise specs, workflows, skills, and agent definitions. I can turn a short description into a work item, hand it to a dynamic workflow, then amend the spec after implementation:

```sh
awman new spec
awman exec workflow --dynamic --work-item 0042
awman edit spec 0042 --amend
```

Before a workflow starts, awman checks its images, Git identity, credentials, and other prerequisites. A compatible local builder can refresh an image when its inputs change; unchanged images are reused. This cuts down failures that otherwise appear after several agents have already done their part.

The builtin microVM runtime reaches Linux x86_64 with KVM in this release, alongside Apple Silicon Macs. More people can run isolated agents with the runtime shipped in awman. This is still a beta; check the [release notes](../releases/v0.14.0.md) for command and checkout changes before upgrading an existing setup.

---

Source and issues at [github.com/prettysmartdev/awman](https://github.com/prettysmartdev/awman). More at [prettysmart.dev](https://prettysmart.dev). Feedback, issues, and contributions are welcome.
