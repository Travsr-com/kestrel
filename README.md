# Kestrel

Run fleets of AI agents on your own machine or server: headless, sandboxed, parallel, and cheaper.

> Status: milestone M1 (headless core). You can run agents from the command line on Linux. See the [quickstart](docs/quickstart.md).

## What works today (M1)

* **Runs agents headless** inside gVisor sandboxes on Linux, each in its own workspace (an empty
  folder or a git worktree on its own branch).
* **Meters every model call.** All model traffic goes through one gateway that counts tokens and
  cost per run, and your budget suspends a run when it is used up.
* **Controls from the command line:** `kestrel serve`, `run`, `watch`, `ps`, `pause`, `resume`,
  `cancel`, `budget`, `approve`.
* **Survives restarts:** runs, events and spend are stored in SQLite.

## Planned

* Agents that start sub-agents, each with a slice of the parent's budget and permissions.
* Cheaper runs through prompt caching and model routing.
* A web UI to watch and configure runs, schedules and triggers, and Google Chat and meeting agents.
* macOS support through Apple `container` (the driver exists but is not verified on the real tool).

The full plan is in [`docs/architecture.md`](docs/architecture.md).

## Learn more

* Get started: [`docs/quickstart.md`](docs/quickstart.md)
* Architecture and roadmap: [`docs/architecture.md`](docs/architecture.md)
* Contributor rules: [`.claude/CLAUDE.md`](.claude/CLAUDE.md)

## License

Apache-2.0. See [`LICENSE`](LICENSE).
