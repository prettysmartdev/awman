# amux grand architecture refactor april 2026

## Purpose
amux has become a spaghetti implementation of 3 different modes: CLI, TUI, and Headless. Each of these 3 modes is meant to provide an identical set of functionality (core business logic) with 3 unique presentation layers (CLI for scripting, TUI for human interaction, Headless for API operation). What has instead arisen is a codebase where each of the 3 frontends provide a patchwork of function calls into a smattering of internal crates with a vague similarity in what they do without any guarantees of functional parity.

This grand refactor will aim to solve that once and for all. amux will be re-designed into a layered architecture that will guarantee codebase health for the forseeable future and allow for simpler implementation of future frontend modes such as desktop apps, code editor extensions, kubernetes operators, and more. The health and viability of the amux project depends on this refactor being successful, as without it there will be no way to offer users a consistent and high-quality experieince using the tool.

## Concept
The new layered architecture will ensure that core business logic, data structures, storage, and container runtime operations are completely seperated from the frontend modalities. Each of the 3 frontends (CLI, TUI, Headless) will become simple presentation layers atop a robust set of core engines. Each presentation layer will be only responsible for output (in the form of stdout/stderr for CLI, rendered TUI, and API responses for Headless) and input (stdin for CLI, keyboard input for TUI, and API requests for Headless). This refactor will make it impossible for the core behaviour of amux to drift between the 3 modalities, and will guarantee that any future frontends will not need to re-implement any of the core business logic.

## Tenets
There are several tenets of this new architecture that MUST be upheld.

1) Every layer must expose its public API ONLY to the layer ABOVE itself. If ever a lower layer needs to interact with a higher layer, it MUST accept an object that implements a trait which the higher layer provides to delegate operations to a higher layer. Lower layers must NEVER directly call functions or access structs from higher layers. 

2) NONE of the frontend packages (TUI, CLI, or Headless) may EVER implement any business logic. ALL business logic MUST be consumed via the layer below, the `Command` layer. The frontend layer's sole responsibility is displaying outputs and recieving inputs. It should NEVER attempt to control the flow of business logic EXCEPT where the command layer requires a frontend trait to manage inputs and outputs. 

3) At EVERY possible opportunity, a typed object should be used over a raw exported pub function. Building a struct with well-understood options that itself exposes public methods should ALWAYS be preferable to exposing a simple pub func. The main exceptions are constructors for said structs, one-off helper functions, and a limited number of stateless functions that take simple inputs and provide simple outputs with no data persistence or OS interaction. An example: instead of exposing a `pub func run_container_with_sink`, instead provide a `pub ContainerRuntime::new_with_options(vec<options>)` which then creates a `ContainerRuntime` object that exposes a `ContainerRuntime::run_with_frontend(some_frontend_trait)`. This decouples internal ContainerRuntime concerns with the frontend that will provide inputs and outputs to the executed containers. Follow this pattern wherever possible.

## Layers
A reminder that LOWER LAYERS (with smaller numbers) must NEVER call functions or use types from HIGHER LAYERS (with higher numbers). This is a key tenet (see above).

### Layer 0: data
Layer 0 is where all definitions of exchangable data live, along with any external storage, api types, filesystem interaction, etc. 

Anything that can be serialized for external use or stored on the filsystem or in a database must live here.

Some key data structs and functions that must live at this layer:

- A new KEY, CORE data concept: `Session`. This will replace both `TabState` (as a TUI tab will just become a frontend representation of a `Session`) and the session concept in the headless server.
  - `Session` and one of its internal structures, `SessionState` will be the new RULING TYPE for all amux operations. `Session` includes the working directory and git root, which EVERY command must respect, it includes information about the default agent, all available agents, all relevant configuration (repo- and global-config), and stores `SessionState`. `SessionState` will store all information related to the command being executed, any workflow state that is currently being run, the current container being used, any relevant error states, and anything else essential to the ongoing execution of commands and workflows in the session.
  - Every single command must be instantiated with a `Session`, and it will be the core guiding light for all command and workflow execution. Even the CLI, which will now be "just another frontend", which can only support a single session at at time, will use `Session` to guide its operations whereas before it would infer state from whichever directory it was launched from. CLI will be a single-Session frontend whereas TUI and Headless will be multi-session frontends. 
  - The management of multiple `Session` will be handled by `SessionManager`, a new type which will be responsible for collecting multple sessions, controlling their CRUD operations, ensuring concurrency-safe access to their state, and handling persistence where needed related to a session.
- All configuration concerns live in this layer, such as repo and global config files (plus reading, writing, merging/resolving them), env var definitions, and flag defintions. Any and all reconciliation or resolution of config that requires merging config files, env vars, and flag values happens at this layer.
- All filesystem concerns, including (but not limited to):
  - Reading and writing config files
  - Handling the headless mode sqlite database
  - Handling the headless mode directories
  - Persisting and retrieving workflow state
  - Handling global workflow/skill directories
  - Resolving filepaths for container overlays and agent settings/auth passthroughs
- Environment variable fetching, definition, parsing, and merging with other config sources
- Any and all other interactions between amux and the filesystem or databases it uses

Things that DO NOT belong in this layer:
- Layer-specific types such as traits or business logic concerns
- Container management OTHER THAN resolution of filepaths and directories for anything mounted to a container

### Layer 1: engine
Layer 1 is where the core primitives needed to execute business logic lives. Any common functionality that spans different commands live in this layer, along with very important components such as the container runtime and workflow execution packages.

Kay components of layer 1:

- The `ContainerRuntime`, responsible for any and all interactions with containers (either Docker or AppleContainers)
  - `ContainerRuntime` will now be responsible for defining traits, types, etc. related to the operation of containers within amux. It will be completely disaggregated from any consumer and is totally agnostic to whatever UX it put in front of it. As discussed in the tenets above, it will be re-designed to mainly provide "container contructors" which take in a set of "options" to build a `ContainerInstance` which can then be executed by a higher layer and provided with one of several `Frontend::*` traits to handle inputs and outputs.
  - It is imperative that the container runtime move away from a huge library of `run_container_with...` pub fns and instead move to a builder/factory design which allows passing options instead of calling dense functions with a dozen parameters. Things like overlays, seeded prompts, interactivity, entrypoint commands, images to be used, etc. should be passed as `Option` objects to a SMALL NUMBER of container builders which return objects implementing the `ContainerInstance` trait. That trait can then be used by a higher-level package to, for example, call `ContainerInstance::run_with_frontend(some_frontend_trait)` which executes the configured container using the provided frontend (which could be a pty, a stdin/stdout binding, etc.)
  - This package should include a `ContainerExecution` type which allows a fully-prepared container run method (such as `run_with_frontend` above) to be provided to another package to run a container with a configured frontend, without leaking the details of the frontend itself. This way, `ContainerExecution::run` can be called and a lower package like `WorkflowEngine` below does not need to concern itself with how the container's execution was prepared.

- The `WorkflowEngine`, responsible for any and all execution of amux workflows:
  - `WorkflowEngine` will be responsible for all state, execution flow, etc related to the execution of any workflow
  - Things like yolo-mode auto-advance, agent and model resolution for each step, executing next steps, etc. must all reside within `WorkflowEngine`
  - `WorkflowEngine` should NOT create `ContainerInstance` itself, but CAN be given a `ContainerExecution` by a higher-level caller to execute a workflow step with a given pre-configured container.
  - `WorkflowEngine` DOES need to concern itself with exit codes etc. from containers, so things like `ContainerExecution` will need to expose whatever outputs are relevant to the workflow, and still allow those outputs to flow appropriately upwards to higher-level callers with error wrapping.
  - `WorkflowEngine` MUST delegate ALL user input to higher-level packages using traits. Things like the workflow control dialog in the CLI, or workflow procedure prompts in the CLI must NOT be included in `WorkflowEngine`, but the ability to request those things from higher-level packages can be achieved by accepting a frontend trait at workflow instantiation, such as `WorkflowEngine::new(workflow... some_frontend_trait)` where `some_frontend_trait` exposes things like `UserChooseNextAction` to trigger the workflow control dialog, etc.

- The `GitEngine`, responsible for any and all Git operations that amux requires, including (but not limited to):
  - Git root resolution
  - Clean vs dirty worktree detection
  - Git worktree management (creation, merging, removal)
  - Adding and committing files
  - (Eventually, not yet) pulling and pushing branches

- The `OverlayEngine`, responsible for constructing and managing all types of overlays that are granted to agent containers:
  - Agent settings / config passthrough
  - User-defined overlays (directories, env vars, secrets, skills, etc.)
  - Any and all other "thing that needs to be mounted from the host or the user into an agent container"

- The `AuthEngine`, responsible for any and all auth-related concerns including (but not limited to):
  - Resolving host-side agent credentials
  - Handling authentication logic for the headless server
  - Any and all other authentication concerns across amux

### Layer 2: command
Layer 2 is responsible for the higher-level business logic of operating amux's various commands.

Each command that amux provides, such as `chat`, `exec prompt`, `init`, etc. should all be implemented, in their entirety, within this package. No command or flag definitions, busines logic, flows, error handling, etc. should leak into higher layers.

Key components:

- A new `Dispatch` package which is used to route inputs (such as command strings from the TUI, or command execution requests received from the headless API) to appropriate command types and methods.
  - `Dispatch` is responsible for ensuring that any higher-level caller is able to provide all of the reqired parameters, flags, etc. for each and every command. For example, `Dispatch::new(some_frontend_trait)` must accept a trait-implemented object which, for each of the possible commands, includes functions to read the appropriate flgs for said command. As an example, the CLI frontend may run `Dispatch::new(cli_command_frontend)` and then `dispatch::run_command("exec", "prompt")` and the `cli_command_frontend` object passed to `new` would have `get_model`, `get_agent`, `get_yolo`, `get_auto`, etc methods allowing the dispatcher to retrieve relevant flag options from the frontend. This guarantees that every frontend will implement methods for all required flags and cannot drift in which flags are supported.
  - The full avilable list of commands avilable resides within the Dispatch package, NEVER any of the frontend packages. The dispatcher will, as needed, provide frontend-specific command definitions (such as clap command objects, TUI hint strings, etc.) which will power the frontend's construction, rather than any of the frontends retaining ANY lists whatsoever of commands, flags, etc. Any and all frontend-specific data provided by `Dispatch` MUST be constructed from master lists of commands, subcommands, flags, etc. and NEVER be generated on a per-frontend basis. For specificity, internal Dispatch:: methods like `clap_commands_from_global(...)` and `tui_hints_for_subcommand_from_global` should be used to generate frontend-specific data based on a core, dispatch-internal canonical list of commands, subcommands, flags, etc.

- Command-specific types
  - For example, a `ChatCommand` object can be created by `Dispatch` which implements a `Command` trait, exposing a `run_with_frontend(some_frontend_trait)` method. Each command object will require a different frontend trait, each exposing the methods that a frontend will need to implement in order to handle user input.
    - For example, the `init_frontend` trait will need to provide methods to collect input about running the refresh agent, among others, whereas `ready_frontend` will need to provide methods to collect input about migrating legacy dockerfiles, etc.
  - Each command-specific object must be instantiated with all of the required flags, config, databse access etc. meaning that the command package must collect EVERYTHING it needs (calling into lower layers or recieving traits from higher layers) in the command object's struct at instantiation time so that command execution goes smoothly.
  - To be specific, for something complex like workflow execution, the Command package is responsible for constructing the workflow object instance and flossing any frontend traits required down from higher levels into the lower levels. Layer 2 is the main go-between that wires frontends, via command-specific business logic, through to the lower levels.

### Layer 3: frontend
As stated above, and re-stated here, any frontend package is a PRESENTATION AND USER INPUT VEHICLE ONLY. Absolutely NO COMMAND SPECIFIC LOGIC OR BUSINESS LOGIC may reside at this layer.

Components of this layer:

- CLI frontend:
  - Uses `Dispatch` to build `clap` commands which sets up their subcommands, flags, arguments, etc. based on information provided by the Dispatch layer
  - When executed by the user, provides all relevant information to `Dispatch` and then calls `run...` on whatever `Command` that `Dispatch` provides to it, only passing in input/output frontend traits as needed to `run...` methods to provide the `Command` with what it needs.
  - Any container execution resources must be provided via a trait (such as for binding stdin/stdout or writing command output to stdout/stderr) to the `Dispatch` or `Command` as they require

- TUI frontend:
  - Launched with bare `amux` command, as always
  - User-perceptible functionality, UX, design, and keyboard operations should all remain identical to pre-refactor, but powered by the layered architecture instead of any TUI package business logic.
  - Responsible for rendering all tabs, execution and container windows, command text box, dialogs, hints, etc. as needed.
  - Use `SessionManager` to manage a set of `Session` objects, each bound to a created tab (replacing the current `TabState`).
  - All command text box input should be routed directly to a method in the `Dispatch` package, no parsing or anything else should be done by the TUI itself
  - Any and all hint text for commands, subcommands, and flags should come from methods in the `Dispatch` package
  - The TUI frontend handles all keyboard input, keyboard shortcuts, PTY creation/rendering, rendering of any and all UI.
  - All data to be displayed in any kind of dialog or the execution or container window should come from packages in lower levels. Dialog layouts can be complex and should be defined within the TUI package itself, but any and all data structures, prompts, user input options, etc. should flow to and from lower level packages. Very few strings should be defined within the TUI package, most should come via frontend traits that the TUI package implements. Things like `*Action` objects should be returned to lower level packages when things like dialog selections are made via frontend trait methods.

- Headless frontend
  - Responsible for all headless server operations including binding ports, handling auth and TLS, etc.
  - All routes and request/response schemas or data structures should be sourced from and provided by lower-level packages such as `Dispatch`
  - No validation of requests should reside in the headless frontend package, all of that logic must be handled by lower-level packages like Dispatch
  - The server MUST NOT "just call the CLI", it should instead directly call into lower level packages like SessionManager, Dispatch, etc. to perform all business logic on its behalf.
  - No specific persistence logic may reside in the headless frontend package, all should be delegated to objects and functions in lower-level packages
  - The server/headless frontend's only job is to translate the lower-level package's functionality into an HTTP-powered API.
  - Server endpoint handler should be nearly identical to their CLI and TUI counterparts, using `Dispatch` to parse inputs and then execute the resolved `Command` and providing `frontend_...` trait implementations. 

### Layer 4: binary
The binary layer is responsible for the main method, whose sole responsibility is to set up the available frontends and make them available to the user. 

- The actual setup of `clap` should happen here (even if the clap commands and subcommands themselves are provided by the frontends in lower-level packages)
- The `amux` binary entrypoint lands here, builds the `clap` command structure, then executes it. That's it. Frontends and other lower-level packages handle everything else.

## Summary
To conclude, this architecture re-work is THE MOST IMPORTANT undertaking in the history of the amux project. It is extremely important that this refactor goes well to ensure the longevity and success of the project.

Note that this document and each of its layers is not exhaustive. There will be corners of the application not covered in this document. For some portions, it may be obvious which layer they should reside at, but for any piece of functionality that is not obvious, the implementing agent should ASK FOR CLARIFICATION and ASK ANY QUESTIONS THAT ARISE FROM UNCERTAINTY. Do not write work items with ambiguous "you could try this or try this", instead ASK THE DEVELOPER to weigh in and choose which option makes more sense.

This document will be a starting point for a series of work items that will define the actual work needed to make this new architecture a reality. It is imperative that the agent writing the work items and implementing the refactor take into strong consideration the design-level thinking this document is meant to outline and adhere to the spirit of the redesign and its tenets before making any decision. And whenever in doubt, ASK THE DEVELOPER, do not make assumptions. Any work item created from this document should reference back to this file and inform implementing agents to read this document and consider the architectural basis before implementing.

Go forth, leave no package unturned. Refactor this entire project and do it PROPERLY with NO SHORTCUTS. amux will be stronger and more successful because of it. MODE PARITY, CODE HEALTH, SECURUTY, SCALABILITY, and PERFORMANCE are key. Do not sacrifice any of them. Keep code clean, modular, and object-oriented. DO NOT CONCERN YOURSELF WITH LEGACY CODE. There is NO REASON to be lazy and leave legacy cruft for the sake of simplicity or expediency. This is a fallacy that will ruin this refactor. Make the RIGHT CHOICE for the new architecure, NOT the easy choice.

Be brave. Be bold. Build for a long future.
