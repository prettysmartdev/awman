# awman 0.14: one daemon, many ways to work

I've wanted awman to be useful wherever development work starts: at my keyboard, in CI, on a machine watching recurring chores, or on a worker taking jobs from a queue. Each of those needs more than a way to launch an agent. It needs a place to keep the job, a way to find out what happened, and clear rules for who can ask it to do what.

v0.14 gives awman that foundation: one daemon for long-running work, with a versioned API that other tools can build on. The CLI, TUI, API, your squad, and workers are five ways to use the same command engine today. The daemon makes room for more experiences without asking each one to own the work itself.

---

```sh
# install or upgrade
curl -s https://prettysmart.dev/install/awman.sh | sh
```

---

## Give long-running work a home

I want to be able to start a workflow, leave it running, and get an honest answer later about its state. The daemon now hosts the engine, worker, and squad features in one process. It owns accepted work, the associated state, and the capacity those features share. I can see which features are on, which jobs are running, and what needs attention with one set of commands:

```sh
awman daemon start --background
awman daemon status
awman daemon logs --follow
```

A TUI can use the daemon's engine, so an admitted command keeps running if a frontend connection drops. The worker accepts jobs sent over the API or claimed from a linked work server. Your squad is a group of agents you own for recurring tasks; its scheduled work runs through the same daemon. I no longer have to manage separate background services for those jobs. The daemon also applies one capacity limit across its enabled features, so a busy queue cannot silently compete with squad work outside my view.

The CLI still works for a command I want to run and finish in my shell. That matters too: a persistent daemon is available when the job needs one, while a one-shot command remains simple to use in a script.

## An API other experiences can rely on

The API is what makes this more than a new process name. Its routes live under `/v1`; status reports the API protocol and capabilities, so a client can check what this daemon supports. Requests and results have defined shapes, resource IDs are opaque, and errors carry stable codes. A client can follow a run through resumable events, cancel it explicitly, and use an idempotency key when it must retry a request.

Those details save real work. A CI job can submit a workflow and retrieve its result without parsing terminal output. A remote client can reconnect to the event stream after a network interruption instead of launching another copy of the task. A new interface can ask the daemon for a session, a terminal, or a job's status rather than trying to manage an agent process on its own.

With a remote target configured, the CLI client uses that API too:

```sh
awman remote exec workflow ./workflow.toml --work-item github:84 --follow
```

The daemon keeps access deliberate. Its local listener is for the owner. A configured network listener uses TLS and named API keys, and it exposes only the features selected for that listener. The same API can support a small local tool or a larger integration without making every daemon capability public.

## Five ways to put the engine to work

The **CLI** is for a one-off command or a script that needs an exit status. The **TUI** is for watching agents, reviewing progress, and controlling a workflow while it runs. The **API** is for CI and other programs that need structured results. **Your squad** takes recurring tasks you describe and evaluates them on a schedule. A **worker** accepts work offered by a server, either automatically or when you pick an item from the queue.

These are different experiences for different moments of the same development workflow. I can create a work item and run it from the shell, watch another run in the TUI, or let a worker claim one from a team queue:

```sh
awman new spec
awman exec workflow --dynamic --work-item 0042
awman link https://work.example.com --name team
awman queue
```

What I care about is that each job still goes through awman's command engine. Workflow steps, results, and setup requirements have one meaning, whether a person typed the command or another system submitted it. That makes an unattended run easier to understand when I inspect it later.

## Keep the same boundaries as awman grows

Every agent still runs in an isolated container or microVM. Before a workflow starts, awman checks the images, Git identity, credentials, and overlays it will need. A compatible local builder can refresh stale images, and unchanged ones can be reused. The worker reviews secrets and overlays for each item instead of handing every incoming job the machine's whole environment. Linux x86_64 with KVM can now use the experimental builtin microVM runtime, alongside Apple Silicon Macs.

I want future interfaces to inherit these boundaries from the engine and daemon. The prerelease web app and a Kubernetes operator are two more modalities coming soon. Both can use the stable API to offer a different experience while awman continues to own execution, state, and isolation.

---

See the [release notes](../releases/v0.14.0.md) for command and checkout changes before upgrading. Source and issues at [github.com/prettysmartdev/awman](https://github.com/prettysmartdev/awman). More at [prettysmart.dev](https://prettysmart.dev). Feedback, issues, and contributions are welcome.
