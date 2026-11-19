# Quickstart

Run one AI coding agent in a sandbox, with a spending limit, and watch it work.

This is the M1 slice. It works on Linux with Docker and gVisor. The web UI, schedules, chat and
meeting agents come in later milestones (see [architecture](architecture.md)).

## What you need

* Linux with **Docker** and **gVisor** (`runsc`) registered as a Docker runtime.
  Install: <https://gvisor.dev/docs/user_guide/install/>
* **Rust 1.89 or newer** (to build Kestrel for now).
* An **Anthropic API key**.
* A **container image for your agent**. It must contain the `claude` command (Claude Code), run as a
  non-root user, and have `curl`. Kestrel does not publish an image yet.
* A **git repository** with at least one commit for the agent to work in.

## 1. Build

```sh
git clone https://github.com/Travsr-com/kestrel
cd kestrel
cargo build --release -p kestrel-cli
export PATH="$PWD/target/release:$PATH"
```

## 2. Write the config

Save this as `~/.config/kestrel/config.toml`, then run `chmod 600` on it. It holds your API key, and
Kestrel warns you if other users can read it.

```toml
[gateway]
listen = "0.0.0.0:7070"

[engine]
max_concurrent_runs = 4

[providers.anthropic]
base_url = "https://api.anthropic.com"
api_key = "sk-ant-..."

[models]
mid = "claude-sonnet-5-5"

# USD per million tokens, as exact decimals.
[prices."claude-sonnet-5-5"]
input_per_mtok = "3.00"
output_per_mtok = "15.00"
cache_read_per_mtok = "0.30"
cache_write_per_mtok = "3.75"
```

> **The gateway cannot listen on loopback.** Sandboxes reach it through a container, which a
> loopback address refuses, so `kestrel serve` exits with an error if you try. Only the per-run
> token protects the gateway, so use a firewall or a private network.

> **Add a price for every model you use.** A call to a model with no price entry is not counted
> against the run's budget.

## 3. Write an agent

Save this as `agent.yaml`:

```yaml
name: fixer
runtime: claude-code
role: coder
model_policy: { default: tier:mid }
sandbox:
  tier: auto
  image: my-claude-code-image:latest   # your image from "What you need"
  cpu: 2
  memory: 4Gi
  workspace: git-worktree
budget: { usd: 2.00, wall_clock: 30m }
```

`tier:mid` becomes the model you set under `[models]`. `workspace: git-worktree` gives each run its
own copy of the repo on its own branch (`kestrel/run-...`), so parallel runs never share files.

## 4. Start the server

```sh
kestrel serve
```

Leave it running. It runs the engine and the gateway, and picks up runs that you submit. Press
Ctrl-C to stop it.

## 5. Submit a run

In another terminal, from inside your git repository:

```sh
kestrel run agent.yaml --input "Fix the failing test in src/lib.rs" --watch
```

`--watch` follows the run live. Without it, `run` prints the run id and returns.

## Everyday commands

| Command | What it does |
| :--- | :--- |
| `kestrel ps` | List active runs. Add `--all` for finished ones. Shows spend so far. |
| `kestrel watch <run-id>` | Follow a run's events. Works on finished runs too. |
| `kestrel pause <run-id>` / `resume` | Stop and restart a run. |
| `kestrel cancel <run-id>` | Stop a run for good. |
| `kestrel budget <run-id> 5` | Set the run's budget to 5 USD. |
| `kestrel approve <approval-id>` | Approve an action the agent asked about. Add `--reject` to refuse. |

Add `--json` to any command for machine-readable output.

## What happens at the limit

At 80% of the budget the run records a warning. At 100% Kestrel stops the sandbox and suspends the
run. Raise the limit and continue:

```sh
kestrel budget <run-id> 5
kestrel resume <run-id>
```

A resumed run starts the agent command again in the same workspace.

## Known limits in M1

* Resuming a paused run restarts the agent from the beginning.
* The agent cannot run `git` inside the sandbox yet ([#22](https://github.com/Travsr-com/kestrel/issues/22)).
* The macOS driver (Apple `container`) is not verified against the real tool yet
  ([#11](https://github.com/Travsr-com/kestrel/issues/11)).
* All sandboxes share one internal network ([#10](https://github.com/Travsr-com/kestrel/issues/10)).

## If something goes wrong

* `no secure sandbox available on this host`: Docker or gVisor is missing. Check that `docker info` lists `runsc`
  under runtimes.
* `providers.anthropic.api_key is missing or empty`: the config has no key. Kestrel never reads keys
  from environment variables.
* A run stays `Queued`: `kestrel serve` is not running, or it is already at `max_concurrent_runs`.
