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

  Kestrel runs the sandbox as the user who owns the workspace, so the image must not rely on a
  particular uid or a writable home directory. `HOME` is set to `/tmp`.
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
# Leave `listen` out. With Docker, Kestrel binds the gateway to the Docker bridge IP
# (usually 172.17.0.1) on port 7070. Change the port with `port = 7070`.
# Long tool calls (a waiting `kestrel.join`, a question) held open at once across all runs,
# from 1 to 4096. Default 256. Past the limit a call is refused with a clear error.
# max_waiting_calls = 256

[engine]
max_concurrent_runs = 4
# Seconds a stop may take, from 1 to 3600. Default 10.
shutdown_grace_secs = 10
# Resume the runs a stop or crash cut off when serve starts again. Default true. Set false if your
# agents do things that must not happen twice: a resumed agent starts again from its input.
# resume_after_restart = true

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

> **With Docker on this machine, the gateway cannot listen on loopback.** Sandboxes reach it
> through a container, which a loopback address refuses, so `kestrel serve` exits with an error
> if you set one. (Docker in a VM is different, see below.) Without a
> `listen` setting, Kestrel binds the Docker bridge IP, so the gateway is not exposed to your LAN.
> If you set `listen = "0.0.0.0:7070"`, Kestrel warns you: the gateway is then reachable from
> your LAN, and only the per-run token protects it.

> **A host firewall can block the gateway.** Traffic from containers (`docker0`) to the host must be
> allowed. `ufw` and an `INPUT` policy of `DROP` often block it. Allow it with, for example,
> `sudo ufw allow in on docker0 to any port 7070`. Rootless Docker and Podman use a different
> bridge network, so the default bind may not work. Set `listen` and `gateway.advertise_url`
> yourself there.

> **Workspace roots.** Kestrel mounts a workspace into a sandbox only if its real path (symlinks and
> `..` followed) is inside an allowed root. The default root is the `workspaces` directory next to the
> database. To allow other places, list absolute paths:
>
> ```toml
> [sandbox]
> allowed_roots = ["/data/kestrel/workspaces"]
> ```

> **Docker in a VM (Rancher Desktop, Docker Desktop, Colima).** Set `gateway.docker_host` to tell
> Kestrel where the Docker daemon runs:
>
> ```toml
> [gateway]
> docker_host = "auto"   # or "native", or "vm"
> ```
>
> * `auto` (the default) tries to bind the Docker bridge IP on your machine. If it works, Docker
>   runs on this host (`native`). If the address is not one of yours, Docker runs in a VM (`vm`).
>   Kestrel prints which one it chose when it starts.
> * Set `native` or `vm` yourself if `auto` guesses wrong.
> * In `vm` mode the gateway binds `127.0.0.1` by default, because the VM reaches your machine
>   through loopback. `gateway.listen` still wins if you set it.
> * In `vm` mode, any container in the VM can reach the gateway port (and any other loopback
>   service on your machine) through `host.docker.internal`. The per-run token is what protects
>   the gateway. This comes with running Docker in a VM.
> * `auto` can guess wrong. If a second, native Docker owns `172.17.0.1` while your active
>   context is a VM, it picks `native`: set `docker_host = "vm"`. Rootless Docker and Podman may
>   be taken for `vm`: set `docker_host = "native"`, or set `gateway.listen` and
>   `gateway.advertise_url`.
>
> Rancher Desktop was tested. Docker Desktop and Colima are expected to work but are not
> verified. On Rancher Desktop, keep the workspace folder (the folder of the database) under your
> home directory, because only the home directory is shared into the VM.
>
> **Known limit:** on Rancher Desktop's shared mount, agents that run git inside the sandbox can
> fail (issue #54). Use native Docker for coding agents until that is fixed.
>
> **If `docker network inspect bridge` lists an IPv6 range first,** Kestrel cannot read the bridge
> IP. Set `gateway.listen` explicitly (this skips the lookup), or set `gateway.docker_host` to `vm`
> if Docker runs in a VM.

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
  workspace: git-clone
budget: { usd: 2.00, wall_clock: 30m }
```

`tier:mid` becomes the model you set under `[models]`. `workspace: git-clone` gives each run its
own full copy of the repo on branch `kestrel/run-<id>`, so parallel runs never share files. A copy
costs disk space; for a big repo add `clone_depth: 1` under `sandbox` to copy only the latest commit.

## 4. Start the server

```sh
kestrel serve
```

Leave it running. It runs the engine and the gateway, and picks up runs that you submit. Press
Ctrl-C (or send SIGTERM) to stop it.

A stop takes at most `engine.shutdown_grace_secs` seconds. Serve destroys the sandboxes and prints
one `shutdown:` line. It exits 0 when all is clean and 1 when a sandbox could not be destroyed in
time. Press Ctrl-C a second time to quit at once (exit 130); sandboxes may be left behind, and the
next `kestrel serve` removes them.

When serve starts again, the runs it cut off carry on by themselves: each starts its agent again
from its input in a fresh sandbox. A parent that spawns the same children again gets the ones it
already had. Paused runs, runs over budget and runs waiting for an approval stay as they were.
`engine.resume_after_restart = false` leaves the cut-off runs suspended until you `kestrel resume`
them.

For the web UI, start `kestrel up` instead. It runs the same, plus the API server on
`127.0.0.1:7171`, and opens the UI in your browser (`--no-open` only prints the address). The UI
shows only a "how to build" page until it is built: `cd ui && npm ci && npm run build`, then
rebuild Kestrel. The API is described in [api.md](api.md).

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

## Upgrading

Stop every kestrel process (`serve`, `up`, any `run --watch`), install the new binary, then start
it. The first open migrates the database to the new schema; an older binary refuses a database
that a newer one migrated.

The upgrade to schema v5 rewrites the whole event log once, in one transaction:

* It takes longer the more events the database holds.
* The WAL file grows to about the size of the event log. Leave roughly twice the size of the
  events data free on disk.
* Any kestrel process still running, an older `serve` included, is blocked until it finishes and
  may fail with `database is locked` after 30 seconds. That is why you stop them first.
* If it fails (a full disk, say), nothing is changed: the database stays as it was. Free some
  space and start again.

## Known limits in M1

* Resuming a paused run restarts the agent from the beginning.
* The agent cannot run `git` inside the sandbox yet ([#22](https://github.com/Travsr-com/kestrel/issues/22)).
* The macOS driver (Apple `container`) is not verified against the real tool yet
  ([#11](https://github.com/Travsr-com/kestrel/issues/11)).
* With `gateway.advertise_url` set there is no bridge, so all sandboxes share one network and can reach each other.
  Without it, each run gets its own network from `gateway.subnet_pool` (default `10.200.0.0/16`).

## If something goes wrong

* `no secure sandbox available on this host`: Docker or gVisor is missing. Check that `docker info` lists `runsc`
  under runtimes.
* `providers.anthropic.api_key is missing or empty`: the config has no key. Kestrel never reads keys
  from environment variables.
* A run stays `Queued`: `kestrel serve` is not running, or it is already at `max_concurrent_runs`. A
  `subnet pool` line in its log means every network block is in use.
* `no block of subnet pool ... can be used until kestrel serve restarts`: every block of the pool is
  taken by networks outside Kestrel, or by run networks Kestrel could not remove. Set
  `gateway.subnet_pool` to a free range, or restart `kestrel serve`.
* `subnet pool ... has no free /29 block`: docker networks or host routes already cover the pool. Set
  `gateway.subnet_pool` to a free range, for example `subnet_pool = "10.201.0.0/16"`.
