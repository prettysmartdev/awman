# new-amux observed issues

### TUI

TUI-1: For the THIRD time now, container stats in the top-right title bar of the container window are not showing any data, only `...`. This is unacceptable, it has been "fixed" several times and still does not work. Think hard, do not take shortcuts, look at the codepaths end-to-end to ensure that container stats in the container window title bar work for every container backend in every scenario and update at a regular interval. Review old-amux and make it work EXACTLY THE SAME WAY. No more fake fixes.

TUI-2: The `status --watch` command run in a new tab that is launched in a non-git directory only outputs two lines of status text, does not show the entire status output, and does not continuously update. Look at how this behaved in old-amux and replicate it EXACTLY using the new grand architecture patterns.

TUI-3: The `config show` dialog window only shows some titles but no content, no controls, no anything. Port it over identically from old-amux and ensure it's wired correctly into the new grand architecture.

### Engines

ENG-1: When producing the status table during `ready`, all of the non-default agents can be reported on in a single table row, like `Other agents: done` instead of having a table row per other-agent. If all non-default agents have valid images, just include one row for all of them. If any of the non-default agents have missing images, each agent with a missing image can get a row in the table, like `Maki: missing`. Non-default agents with missing images are NEVER a fatal error and should only produce warnings and a row in the status table. Ensure this is all handled in the ready engine and that both frontend traits render the output correctly.

### Commands

COM-1: Whenever a git/worktree pre/post workflow detects a dirty worktree and/or requires a commit message, ensure the engine and/or command code produces BOTH the list of dirty files AND a suggested commit message to the frontend and that the frontends render these correctly so that the user knows which files are dirty and can choose to accept the suggested commit message or delete it anwrite their own. Ensure all the git logic is at the engine/command layers and the frontends are rendering and returning chosen commit messages only via their frontend trait implementations.
