<h1 align="center">Kestrel</h1>

<p align="center"><b>Run fleets of AI agents on your own machine or server.<br/>Sandboxed, metered, and under a budget.</b></p>

<p align="center">
  <a href="https://github.com/Travsr-com/kestrel/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/Travsr-com/kestrel/ci.yml?branch=main&style=flat-square&logo=githubactions&logoColor=white&label=CI" alt="CI"/></a>
  <a href="#build-from-source"><img src="https://img.shields.io/badge/rust-1.89%2B-B7410E?style=flat-square&logo=rust&logoColor=white" alt="Rust 1.89+"/></a>
  <a href="https://gvisor.dev"><img src="https://img.shields.io/badge/sandbox-gVisor-4285F4?style=flat-square" alt="gVisor sandbox"/></a>
  <a href="#status"><img src="https://img.shields.io/badge/status-M1%20done%2C%20M2%20in%20progress-orange?style=flat-square" alt="Status"/></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue?style=flat-square" alt="Apache 2.0"/></a>
</p>

<p align="center">
  <a href="#quickstart">Quickstart</a> ·
  <a href="#why-kestrel">Why Kestrel</a> ·
  <a href="#cli">CLI</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#status">Status</a> ·
  <a href="#contributing">Contributing</a>
</p>

> Point an agent at a repo, give it a budget, and walk away. Kestrel runs it headless in a
> sandbox, meters every model call, and stops it when the money is gone.

## Why Kestrel

- **Sandboxed by default.** Each run gets its own gVisor sandbox and its own workspace: an empty
  folder, or a git clone on a branch of its own. Agents never touch your working tree.
- **Every call metered.** Model traffic goes through one gateway that counts tokens and cost per
  run. A run that spends its budget is suspended, not left running.
- **Self-hosted.** One binary and a SQLite file. Your code and keys stay on your machine.
- **Survives restarts.** Runs, events and spend are stored. Stop the server and start it again, and
  suspended runs are still there.
- **Bounded shutdown.** Ctrl-C or SIGTERM stops every sandbox within a grace period you set, and
  the next start sweeps up anything left over.

## Quickstart

You need Linux with Docker and gVisor (`runsc`), Rust 1.89 or newer, an Anthropic API key and an
agent image that contains `claude`. The [quickstart](docs/quickstart.md) has the full setup.

```sh
# 1. Build
git clone https://github.com/Travsr-com/kestrel && cd kestrel
cargo build --release -p kestrel-cli
export PATH="$PWD/target/release:$PATH"

# 2. Start the engine and gateway (config: ~/.config/kestrel/config.toml)
kestrel serve

# 3. In another terminal, inside a git repo, submit a run (agent.yaml is in the quickstart)
cd your-project
kestrel run agent.yaml --input "Fix the failing test in src/lib.rs" --watch
```

Prefer a browser? `kestrel up` starts the same engine plus the API server and opens the web UI.

## CLI

| Command | What it does |
|---|---|
| `kestrel serve` | Run the engine and gateway in the foreground |
| `kestrel up` | Same, plus the API server and web UI, and open the UI |
| `kestrel run <agent.yaml>` | Submit a run and print its id (`--watch` follows it) |
| `kestrel watch` | Follow a run's events live |
| `kestrel ps` | List active runs (`--all` for finished ones) |
| `kestrel pause` / `resume` / `cancel` | Control a run |
| `kestrel budget` | Set a run's budget in USD |
| `kestrel approve` | Resolve a pending approval |

## How it works

```
 kestrel run ──► engine ──► sandbox (gVisor) ──► agent (Claude Code)
                  │              │                    │
                SQLite           └── per-run network ─┘
              (runs, events,                │
                 spend)                     ▼
                                  gateway: token + cost metering
                                            │
                                            ▼
                                     model provider
```

- The **engine** schedules runs, enforces the concurrency cap and keeps the state machine.
- The **sandbox** drivers start each agent in gVisor (Linux). Apple `container` is supported in
  code and not yet verified on a real Mac.
- The **gateway** is the only way out for model traffic. Each run has its own token and its own
  network, so sandboxes cannot reach each other.
- The **store** is SQLite, with an append-only event log.

Crates: `kestrel-types`, `-store`, `-core`, `-sandbox`, `-gateway`, `-agent`, `-server`, `-cli`.
Full design: [`docs/architecture.md`](docs/architecture.md).

## Status

| Milestone | Scope | State |
|---|---|---|
| M1 | Headless core: one agent, sandbox, gateway metering, CLI | Done ([proof](docs/milestones/m1.md)) |
| M2 | Sub-agents with sliced budgets, web UI, HTTP and WebSocket API | In progress: API server and UI scaffold landed |
| M3 | Cheaper runs: prompt caching and model routing | Planned |
| M4 | Schedules, webhooks, Google Chat | Planned |
| M5 | Live meeting agents | Planned |
| M6 | Teams: Postgres, SSO, more sandbox tiers | Planned |

Details and acceptance tests per milestone are in the
[roadmap](docs/architecture.md#7-milestones-and-acceptance-criteria).

## Docs

- [Quickstart](docs/quickstart.md): install, configure, run
- [API reference](docs/api.md): the HTTP and WebSocket API behind the UI
- [Architecture](docs/architecture.md): design, data model, roadmap
- [UI](ui/README.md): the Svelte web app

## Build from source

```sh
cargo build --release -p kestrel-cli
cargo test --workspace
```

## Contributing

Kestrel is built test first. Every change comes with tests, and the workspace keeps line coverage
at or above 95%. Before opening a PR:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo llvm-cov --workspace --fail-under-lines 95
cargo deny check
./scripts/check-dashes.sh
```

The full rules are in [`.claude/CLAUDE.md`](.claude/CLAUDE.md). Kestrel is built with
[Travsr](https://github.com/Travsr-com/travsr), the code graph that lives next to git.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
