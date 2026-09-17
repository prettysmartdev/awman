# awman 0.12: tackle more with your personal squad of agents

Much of my day building software isn't the work I'd describe if you asked me what I do. It's the triage, the dependency and pipeline management, the PR that went red on a merge conflict, the issue that needs a rough plan before anyone can pick it up. Each thing takes only a few minutes, but together they eat up significant chunks of my day. Even with the multiplicative effort of parallel agents, the thing I actually wanted to build often gets pushed down the stack while I ensure the "glue work" gets done. 

Agents have taken on huge amounts of complex software development for a while now, but every one of them requires that I kick things off with a prompt or a spec (which interrupts some other line of thinking and forces a context switch). My goal with awman v0.12 was to drastically reduce the amount of time I need to spend thinking about procedural glue work throughout my day.

v0.12 introduces **awman squad**: a group of agents that are always looking out for you, taking on the repetitive work as it shows up so your attention stays on the things that matter.

---

```sh
# install or upgrade
curl -s https://prettysmart.dev/install/awman.sh | sh
```

---

## Helpers that watch so you don't have to

awman now allows you to hand your squad a task in the same words you'd use with a teammate:

```sh
awman squad start --background
awman squad add --name issue-triage \
  --description "When a new issue is opened in this repo, analyze it, draft an implementation plan, and comment the plan on the issue." \
  --interval 4h --overlay "env(GITHUB_TOKEN)"
```

From then on, one of your squad's agents runs regularly to check whether there is anything for it to do. Often there isn't, and it stands down after doing a thorough check. When there is, the task's leader agent plans an awman dynamic workflow for the work that needs doing and sees it through, with nobody at the keyboard.

Because an agent judges the real-world state of each task, you describe the chore instead of programming it. "When a PR has failing tests, fix them" covers the flaky one and the broken one without needing to be overly prescriptive. "When I comment `/squad` on an issue, do some research on my question and reply with an answer" means a quick comment from my phone is enough to hand something off. Nothing to script, no webhook to configure.

The squad I run today handles new-issue triage, fixing failing CI runs, weekly dependency bumps, and weekly security and architecture audits. None of it is the work I'm excited about doing, but it is all important and it used to be work I had to do myself. Now I sign on in the morning and the repetitive stuff has already been tackled, which leaves the day for the parts I actually care about.

<!-- SCREENSHOT PLACEHOLDER: the squad TUI tab with a few task cards in different states.
     Save it as docs/blog/images/squad-tab.png and replace this comment with:
     ![squad tab](./images/squad-tab.png) -->

Handing work off only feels good if you can monitor everything that happens. `awman squad` opens a TUI tab showing all of the tasks your squad is handling: full history, last run, next run, and the reason the leader gave for acting or standing down each time. From there you can run a task immediately, cancel one heading somewhere strange, or attach to the live container and watch it work.

awman squad is not meant to take on your entire workflow, but rather take the repetitive and menial tasks off your plate so you can spend more time on important design, implementation, and product work.

## Curate a skills library for you and your squad

A squad agent working unattended can't ask you for help mid-task. Whatever skills it starts with are what it has, so equipping your squad well matters more than it ever did for a session you were sitting in front of.

Until now every skill in `awman` was one you wrote by hand. v0.12 lets you pull published libraries of skills straight into your local collection and mount them to agents with overlays:

```sh
awman new skill --pull obra/superpowers
awman chat --overlay "skill(superpowers/brainstorming)"
```

You now have the ability to pick and choose the most useful collection of skills you write along with those published by others in the community without adding anything to the repos you work on. Everything is managed by awman centrally and then dynamically mounted to agent contaienrs at runtime. Nothing is added until you explicitly request it using an overlay, so you can pull a library to try it out without it polluting the context window of every agent you run. If you come to rely on a skill regularly, you can add it to your repo's `.awman/config.json` and every agent on the project gets it automatically. `--pull-all` keeps the whole collection current.

## Also in 0.12

Previously, long workflow runs could die if the Claude OAuth token expired. Containers managed by awman now get an auto-refreshed credentials file that stays fresh for the life of the session, while the refresh token never leaves the host. In addition, a workflow step that fails no longer ends the entire run; the workflow control board opens so you can retry, step back, or step over and carry on. Finally, a new experimental ACP launch mode renders any compatible agent using awman's own UI instead of its raw terminal stream. Full details in the [release notes](https://github.com/prettysmartdev/awman/blob/main/docs/releases/v0.12.0.md).

---

Using awman squad is the first time I have trusted agents with real work while I was not watching, and I am still learning how far I can stretch that trust. I'd reccomend starting with chores whose worst outcome is an unhelpful comment rather than an unwanted push, and widen from there. Use overlays and squad's leader guidance settings to control exactly how your squad behaves.

---

Source and issues at [github.com/prettysmartdev/awman](https://github.com/prettysmartdev/awman). More at [prettysmart.dev](https://prettysmart.dev). Feedback, issues, and contributions all welcome.
