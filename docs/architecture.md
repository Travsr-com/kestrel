# Travsr Kestrel: Architecture v2 (working plan)

> Status: draft for discussion. Supersedes `travsr_kestrel_draft_architecture_plan.md`.
> Inputs: `Research.md`, the v1 draft, Travsr ground truth (`travsr/.claude/CLAUDE.md`).
> License: Apache-2.0. Team: 2 to 3 engineers. First-class hosts: Linux and macOS from M1.
> First integrations: Google Chat, live meeting bot (Google Meet first).

## 0. What changed from v1, in one screen

| Area | v1 draft | v2 | Why |
| :--- | :--- | :--- | :--- |
| Deployment | Implied cluster (Redis, Qdrant, k8s-style) | One `kestrel` binary. Local mode on any laptop, server mode for self-hosted teams | "Run on anyone's machine" is a hard requirement |
| State | Redis Streams, per-task consumer groups | SQLite WAL event log + in-memory broadcast (local); Postgres (server). `EventLog` trait | No extra daemons locally. v1 consumer-group design could not distribute work |
| Token streaming | Every token persisted as an event | Tokens broadcast live only; final message persisted | Avoids the write amplification the research warned about |
| LLM access | Router + masker as middleware for the native agent only | **Kestrel Gateway**: an Anthropic- and OpenAI-compatible proxy every agent must go through | Third-party agents (Claude Code, OpenCode, Codex) get routing, masking, cost metering and budgets for free |
| Code graph | Re-implemented in `trv-context` with its own schema | Consume Travsr over MCP. Kestrel owns zero graph tables | Travsr already exists; MCP is its only interface |
| Sandboxes | 3 tiers built up front (WASM, gVisor, Firecracker) | `SandboxDriver` trait, drivers picked by host capability | gVisor is Linux-only, Firecracker needs KVM, macOS needs its own path |
| Routing | 4 layers (rules, semantic, KEV, RouteLLM) | Rules + role tiers + prompt caching first; KEV later, only where hardware allows | Same savings, far less to build; KEV latency unproven on CPU |
| PII masking | SLM on every payload, "cryptographic" | Deterministic secret scanning first; SLM for prose PII later; vault persisted encrypted | Placeholders in code break code; tasks can be suspended for days |
| Fine-tuning | Distil frontier outputs continuously | Org-data adapters on an optional GPU worker, behind an eval gate | Training on provider outputs may breach their terms; CPU-only hosts cannot train |
| Naming | `trv-*` crates, `trv kestrel` | `kestrel-*` crates, `kestrel` binary | `trv` does not exist; avoids clashing with `travsr-*` |
| UI | Unspecified web dashboard | Svelte 5 + Vite static SPA embedded in the binary | Lightest stack that still handles live streams, a graph view and a form editor |
| ONNX | `ort` | `candle` for transformer models, `tract-onnx` for small classifiers | Travsr already vetted tract (RUSTSEC notes in its Cargo.toml) |

## 1. Product definition

Kestrel is a self-hostable platform for running fleets of AI agents: define agents, schedule or trigger
them, let them spawn sub-agents for their work, run them headless inside secure sandboxes, and watch and
steer all of it live from a UI.

**MVP promise (what the first release must prove):**
1. **Cheaper:** the same task costs measurably less through Kestrel than running the agent directly.
2. **Headless:** agents run unattended in sandboxes, triggered by CLI, schedule or webhook.
3. **Parallel:** one run fans out into many concurrent agents, with no conflicts between them.
4. **Great UI:** see every running agent, its spawn tree, cost and output live; configure agents without YAML.

**Agent families (catalog, delivered in stages):** coding, review, chat (Google Chat first), live meeting bot
(Google Meet first), research/ops, and spec-first planning (EGM template).

## 2. Design principles

1. **One choke point.** All model traffic goes through the Kestrel Gateway. All other egress goes through the
   sandbox egress proxy. Nothing in a sandbox talks to the internet directly.
2. **Capabilities, not trust.** Every run holds a capability token. A child agent's token is always a subset of
   its parent's (tools, network, secrets, budget, depth). No escalation by spawning.
3. **Secrets never enter sandboxes.** The egress proxy injects credentials for allowed hosts; the gateway holds
   provider keys.
4. **Single binary, progressive infrastructure.** Local mode needs nothing but the binary and a sandbox runtime.
   Server mode adds Postgres. Anything heavier (Redis, GPU workers) is opt-in.
5. **Detect, don't assume, hardware.** Sandbox tier and local-model features switch on based on what the host
   actually has (`/dev/kvm`, CUDA, Metal, RAM).
6. **Deterministic first, models last** (inherited from Travsr): rules, graphs and policies decide what they
   can; models handle the rest.

## 3. Architecture

```text
   kestrel CLI        Web UI (Svelte, embedded)       IDE / external MCP hosts
        │                    │  WebSocket                     │ MCP
        └────────────────────┼────────────────────────────────┘
                             ▼
┌──────────────────────── kestrel-server (one process) ─────────────────────────┐
│  API + WS (axum)   Scheduler + run state machine   Triggers (cron/webhook/event)│
│  Policy engine (capabilities, budgets, approvals)  Kestrel MCP (spawn, ask_human)│
│  EventLog: SQLite WAL (local) | Postgres (server)  + tokio broadcast for live    │
└───────────────┬───────────────────────────────────────────────┬──────────────┘
                │ provisions                                    │
                ▼                                               ▼
┌──────────── Sandbox (per run) ─────────────┐   ┌──────── Kestrel Gateway ─────────┐
│ driver: gVisor | Firecracker | Apple VM    │   │ Anthropic + OpenAI compatible     │
│ workspace: per-run git clone / snapshot    │   │ 1 mask secrets & PII              │
│ runtime: native agent | claude-code |      │──►│ 2 route: rules → role tier → KEV  │
│          opencode | codex | custom image   │   │ 3 cache-aware prompt shaping      │
│ env: *_BASE_URL → gateway, MCP → kestrel   │   │ 4 budget check, meter cost        │
│ egress proxy: allowlist + cred injection   │   │ 5 provider call, unmask, trace    │
└────────────────────────────────────────────┘   └──────────────┬───────────────────┘
                │ MCP                                            │
                ▼                                                ▼
   Travsr (code graph)   Memory (Supermemory MCP | built-in)   Providers / local models
```

### 3.1 Core primitives

| Primitive | Meaning |
| :--- | :--- |
| **Agent** | A declarative definition: runtime, role, model policy, capabilities, sandbox, budget, approvals, memory scope, triggers. Versioned. |
| **Run** | One execution of an Agent. Has `parent_run_id` and `root_run_id`, so spawned children form a tree. |
| **Trigger** | What starts a Run: manual (CLI/UI), cron, webhook, or an event watcher (a condition on another run's events or an external feed). |
| **Capability token** | Signed, attenuable grant (Biscuit, via `biscuit-auth`). Children receive an attenuated copy. |
| **Workspace** | Reusable setup object: a per-run git clone, repo snapshot, or empty dir, mounted into a sandbox. |
| **Approval** | A pending human decision. Suspends the run until resolved from UI, CLI or chat. |

Example Agent definition (the UI edits the same structure as a form):

```yaml
apiVersion: kestrel/v1
kind: Agent
name: pr-reviewer
runtime: claude-code              # native | claude-code | opencode | codex | image
role: reviewer
model_policy: { default: tier:mid, escalate: tier:frontier }
capabilities:
  mcp: [travsr.get_blast_radius, travsr.get_callers, kestrel.spawn, kestrel.ask_human]
  network: [api.github.com]
  secrets: [github_token]         # injected by egress proxy, never visible in sandbox
  spawn: { max_children: 4, max_depth: 2, agents: [test-writer] }
sandbox: { tier: auto, cpu: 2, memory: 4Gi, workspace: git-clone }
budget: { usd: 2.00, wall_clock: 30m }
approvals: { required_for: [gh.comment, git.push] }
memory: { scope: project }
triggers:
  - webhook: github.pull_request.opened
```

### 3.2 Run lifecycle

```text
QUEUED → PROVISIONING → RUNNING ⇄ SUSPENDED(approval | budget | error | manual)
                           └→ COMPLETED | FAILED | CANCELLED
```

Every transition, tool call, model call and approval is an append-only event keyed by `(run_id, seq)`.
Pause, resume, replay and the live UI all read from this log. Cancelling a run cancels its subtree.

A restart cuts off what was `RUNNING` or `PROVISIONING`. The next start suspends those runs as
`SUSPENDED(error: "engine restarted")` and resumes them by itself (section 3.3, "After a restart").
That is the one way a run goes from `PROVISIONING` to `SUSPENDED`.

### 3.3 Agents spawning agents

Exposed to every runtime as the MCP tool `kestrel.spawn(agent, input, budget_usd?, wait?)`.

* The server checks the caller's token: is the child agent allowed, are depth and child-count within limits?
* The child's budget is **carved out of** the parent's remaining budget, so a tree never exceeds the root budget by more than the calls in flight.
* The child token is the parent token plus attenuation (fewer tools, hosts, secrets). Never more.
* `wait=true` blocks and returns the child's result; `wait=false` returns a handle for fan-in via `kestrel.join`.
* Concurrency is capped per host (a semaphore sized to CPU/RAM), so a runaway fan-out queues instead of thrashing.

**How the engine spawns** (`Engine::spawn_child`):

* **Who may spawn.** Only a `Running` run. Its capability token needs `kestrel.spawn`, spawn rights with depth left (`max_depth`), and the child agent in its `agents` list. A root run's token is made from its spec on its first spawn.
* **The child's token.** What the child's spec asks for, cut down to what the parent holds. Anything cut is recorded on the child as a `CapabilitiesDropped` event.
* **Depth is lowered and reported.** A child spec whose `spawn.max_depth` is more than the parent can give (the parent's `max_depth` minus one) gets what the parent can give. The spawn still succeeds, so an agent can spawn copies of itself: each copy gets one level less. The `kestrel.spawn` answer adds `depth` (what the child got) and a `note`, for example `max_depth lowered from 5 to 1 (the most this parent can give)`, and the child records the same in an `agent_log` event. A spec at or below that depth gets exactly what it asks for, with no note and no event.
* **Budget.** Each run may spend up to its own limit. A spawn moves the child's budget out of the parent's limit, in the same store transaction that creates the child, so the limits of a tree always add up to the root budget. When a child ends, it keeps what it spent and the rest goes back to its parent (or further up, if the parent has ended too). The gateway checks a run's limit before each model call, so a run can go over by the calls it has in flight. A tree can go over its root budget by those overshoots, never more.
* **`kestrel budget` in a tree.** A root's limit is its own share (what it kept), set as given. Raising a spawned run takes the difference from its parent, and is refused with a log line when the parent has less left. Lowering one gives the difference back. No run is lowered below what it already spent, and a raise gets exactly what was asked. To add money to a tree, raise its root.
* **Out of its own budget.** A parent that handed out everything it had keeps running while it has children going: the gateway refuses its own calls, but it can wait for them. With no children left, it is suspended as usual.
* **When a run's token stops working.** Once the run has ended (completed, failed or cancelled), every gateway route refuses its token with 410: model calls, `kestrel.*` tools and Travsr alike, so a finished run cannot spend more. A call already in flight finishes and is metered once; a call whose body was still arriving when the run ended is refused once the body is in. When the gateway cannot read the run's state (a store error), the call gets a 503 the agent retries, and nothing is forwarded or metered. A suspended run keeps its token, because it can be resumed with it. A budget-suspended run's model calls are refused by the budget check as before. A run suspended for an approval still has a running agent, so the budget check is what bounds its model calls while it waits.
* **Default budget.** With no `budget_usd`, the child gets the smaller of its spec's budget and half of what the parent has left.
* **Limits.** The parent's `max_children` counts live and finished children. One tree holds at most 256 runs (`Engine::with_tree_limit`). A running parent holds a concurrency slot like any run, so with `max_concurrent_runs = N` a parent leaves `N - 1` slots for its children.
* **Tokens.** A capability token is valid for 7 days. A root's expired token is made again from its spec at its next spawn. A spawned run's token comes from its parent's and is not remade. A child's stored spec lists only the capabilities it was granted.
* **Model tier.** A child spec without `model_policy` runs one tier below its parent (lever 1 in section 3.5).
* **Workspace.** Each child gets its own clone. A `git-clone` child whose `repo` is relative clones its parent's repo.
* Because a parent's limit shrinks when it spawns, `kestrel ps` shows the budget the parent kept, not the one it started with.

**How a parent waits** (`Engine::join`):

* `join(parent, children, mode, timeout)` waits for the named children (all of them when none are named) and returns each one's result: `Completed` with its summary, `Failed` with the reason, `Cancelled`, or `TimedOut`. Each result also has what the child spent. `All` waits for every child; `Any` returns as soon as one has ended, with only those that ended. A child that times out keeps running.
* A parent that waits gives its concurrency slot back. It takes it back in line with queued runs before the join returns, so a fan-out to as many children as there are slots cannot deadlock. Its sandbox keeps running meanwhile (it is inside a tool call), so running sandboxes can briefly outnumber the slots by the parents that wait.
* Several joins of one parent share this: the first gives the slot back, the last takes it again. A timeout bounds the wait for the slot too: past its deadline a join returns without it, and the parent takes the slot back once one is free. A join whose caller goes away also leaves the slot to come back to the parent.
* A join reads everything from the store. Calling it again, after a restart too, returns the children that ended at once.

**After a restart** (`Engine::recover`, then `run_forever`). An agent process cannot resume in the middle of a join, so Kestrel makes running it again safe and automatic:

* **Cut-off runs are resumed.** Every run that was `Running` or `Provisioning` becomes `Suspended(Error("engine restarted"))`, and recovery queues a resume for it, parents and children alike. The resumes wait for slots in the usual line, so a tree comes back without anyone running `kestrel resume`. If serve stops again before a queued resume is delivered, the next start queues it again: the start that suspended a run with a resume logs `kestrel: the engine restarted; this run resumes when a slot is free`, and only runs with that line after their last restart are resumed this way. Paused runs, runs over budget, runs waiting for an approval and runs suspended for any other error stay as they are.
* **Turning it off.** A resumed agent starts again from its input in a fresh sandbox, so whatever it did before it does again. For agents whose work must not happen twice, set `engine.resume_after_restart = false` (default `true`, `Engine::with_resume_after_restart`). The cut-off runs then stay suspended until someone resumes them.
* **No crash loop.** A run that takes serve down would otherwise be resumed on every start. Recovery counts the run's earlier `StatusChanged` events to `Suspended(Error("engine restarted"))`. The first three restarts that stop a run resume it; from the fourth on, it stays suspended and its log says `kestrel: not resumed automatically: ...; resume it by hand`. `kestrel resume` still works.
* **Upgrading.** Only runs that a start with automatic resume stopped are resumed. Runs already `Suspended(Error("engine restarted"))` before the upgrade, or stopped while the setting was `false`, stay suspended after the upgrade or after the setting is turned on. Resume them with `kestrel resume` if you want them back.
* **Spawn returns the child it already made.** The store keeps each spawn with a key: a SHA-256 of the child's agent name and input. Within one start of the parent's agent, its nth spawn with a key returns the nth child it spawned with that key before (oldest first), live or finished. Past those, it spawns a new one. So a parent that spawns the same worker three times gets three children, and gets the same three back after a restart. The count starts over each time the parent's agent starts (a resume after a restart, a pause, a budget stop or an approval).
* **A child returned again is not spawned again.** No budget is carved, no `ChildSpawned` is written, and it does not count against `max_children` or the tree limit. The answer carries `"existing": true` and the child's `status` now (it may have ended: `kestrel.join` gives its result), and the parent's log names the child. The `budget_usd` and `wait` of the repeated call are not part of the key: the child keeps the budget it got.
* **Why agent and input, not the JSON-RPC id.** A restarted Claude Code opens a new MCP session. Its ids start over and follow whatever the model calls in that run (`initialize`, `tools/list`, other tools), so after a restart an id can name a different call. What the agent reproduces is what it asks for: the same agent and input. An agent that rewords a child's input gets a new child.
* **The join answers once.** A join from the re-run parent reads its children from the store: finished ones answer at once, live ones are waited for, and each is in the answer once.
* **A child cut off while provisioning** never started its agent. It is suspended and resumed like the others, so it starts once, and the parent's join counts it.
* Children spawned before the keys existed (schema v5) get theirs when the store migrates to v6.

**Cancelling a tree:**

* Cancelling a run cancels every run below it first, from the leaves up, then the run itself. Every sandbox is stopped. A queued child never starts.
* Cancelling a child alone never touches its parent: the parent's join reports `Cancelled`.
* A run that ends with children still going cancels them before its own `RunFinished`, so no child is left behind. `spawn.keep_children: true` in the spec leaves them running; a child keeps its own children only if its parent keeps its. Cancelling a tree reaches every live run in it, also below a child that ended and kept its children.
* While a run's cancel waits for the runs below it, the run may not spawn. A cancel still waiting when the engine stops goes back into the store, so the next start delivers it.
* Pausing a parent, or changing its budget, does not touch its children.

**How a sandbox calls the tools** (`kestrel-gateway`, MCP protocol version `2025-06-18`, streamable HTTP):

* The tools live on the gateway at `POST {gateway}/r/{run_token}/mcp`, next to the model routes, because the gateway is a sandbox's only way out. The run is the one the token resolves to, never one named in a request. An unknown token is 401, a run that has ended 410; neither echoes the token.
* Methods: `initialize`, `ping`, `tools/list`, `tools/call`. Tools: `kestrel.spawn(agent, input, budget_usd?, wait?)`, `kestrel.join(children?, mode?, timeout_s?)`, `kestrel.ask_human(question)`. `GET` is 405: the server sends nothing unasked.
* `tools/list` shows the tools in the run's capability (`mcp` in its spec); `kestrel.spawn` also needs spawn rights. Calling another is a tool error, "not permitted", and the run's log records it.
* `kestrel.spawn` finds the agent by name in `engine.agents_dir` (default `agents` next to the database): agent `worker` is `worker.yaml`.
* `kestrel.ask_human` records a question approval and the run waits on it, as for any approval. The answer is the text a person approves it with (the Approvals view has an Answer box). Declining fails the run.
* A long call (`join`, `ask_human`, a `spawn` that waits) is never silent for more than 15 s while it waits, so the bridge (`socat -T 300`) never sees it idle. What the client gets depends on its `Accept` header:
  * With `text/event-stream` (as MCP clients send it): an event stream, a keepalive comment every 15 s, then the result as one `message` event.
  * JSON only: a call that answers within 15 s gets a plain JSON body. A longer one gets `Content-Type: application/json` with a space every 15 s, then the JSON-RPC result. Leading whitespace is valid JSON, so the body parses as the result.
* Each call is keyed by run and JSON-RPC id: a client that reconnects and calls again with the same id gets the same result, without a second spawn or wait. Results are kept for at least an hour.
* Limits, so a sandbox cannot make the gateway hold much: bodies up to 1 MiB (413); ids a string of at most 128 bytes or an integer; per run a burst of 20 tool calls refilled at 10 per second, at most 8 calls waiting and 256 results kept (429 beyond); at most 256 children and a timeout of at most a day per join.
* A question is answered within the run: its wall clock keeps going while the question waits, because the sandbox keeps working. A question still waiting when the engine restarts is declined (unless it was answered just before) and its run fails ("engine restarted while its question waited"), and its live children are cancelled unless its spec keeps them: nobody waits for the answer any more.
* A long call (`join`, `ask_human`, a `spawn` that waits) holds a connection through the shared bridge until it answers, maybe for hours, and a parent in `join` gives its slot back meanwhile. So the gateway holds at most `gateway.max_waiting_calls` long calls open at once across all runs (default 256, the tree limit, so one tree whose runs each wait on one call never fills it; 1 to 4096). A new call past that is refused at once with a tool error the agent reads, nothing is started, and the run's log says why. Waiting on a call that is still running (a request with its id, whatever tool it names) takes room too, and past the cap is told the call is still running; a call that has finished is answered whatever the count.
* The bridge is sized from the same settings: `2 * max_waiting_calls + 4 * engine.max_concurrent_runs` connections (528 by default, at most 8192). 2 covers a waiting call and an idle keep-alive connection its client may keep; 4 covers a running run's model call, a short tool or Travsr call, and two more. Its process and memory limits grow with it. Not counted: an agent that keeps more connections than that open at once, and a parent that runs on without its slot after a join timed out. Then new connections wait on the bridge until one closes. A run may hold up to 8 long calls (overlapping joins, spawns that wait made in parallel), so a tree whose runs each hold several can reach the cap; its long calls are then refused clearly.
* `serve` keeps the key that signs capability tokens in `root.key` next to the database. Keep it: a new key refuses the tokens of runs spawned before.

### 3.4 Sandbox drivers

| Host | Default driver | Strong tier | Notes |
| :--- | :--- | :--- | :--- |
| Linux | gVisor (`runsc`, systrap platform) | Firecracker when `/dev/kvm` exists | Works on ARM64 and x86_64 |
| macOS (Apple Silicon) | Apple `container` (lightweight VM per container) | same | In scope from M1. Needs macOS 26+; older macOS and Intel Macs fall back to a Lima VM running `runsc` |
| Windows | WSL2 + Linux driver | Firecracker if nested virt is on | |
| Any | `docker-runc` | none | Allowed only for trusted runtimes, shown as "weak isolation" in the UI |

WASM (`wasmtime`) is kept only for small, user-authored policy and watchdog plugins, not for agents.

**Run networks (docker drivers).** Each run gets its own `--internal` network, so a sandbox can reach the gateway and nothing else, not even another sandbox. Kestrel picks the subnet itself: a `/29` block from `gateway.subnet_pool` (default `10.200.0.0/16`, 8192 blocks), skipping blocks that docker networks or host routes already use. Run networks never use docker's default address pools, so a large fan-out does not run out of them. The bridge's own shared `kestrel-gw` network gets a span of the same plan, so serve starts even when the default pools are used up.

* `kestrel-gw` is created with `--subnet` and `--gateway` on the lowest free aligned `/24` of the plan (32 blocks, room for 253 containers), and no run network gets a block of it. Every `kestrel serve` on one docker daemon puts its bridge on this one network, so a `/24` leaves room for many instances.
* When the pool cannot hold a `/24` plus one run block, the span shrinks to the largest aligned one that leaves a run block. When not even a `/29` plus one run block fits, serve stops with an error naming `gateway.subnet_pool`.
* An existing `kestrel-gw` as large as that span is reused as it is and takes nothing from the plan. A smaller one (a `/29` from an older build) is removed and recreated on the span when no container is on it, counting created and stopped containers too (`docker ps -a --filter network=kestrel-gw`). Otherwise serve keeps it and prints a warning with its subnet and how to recreate it.
* If another serve removes `kestrel-gw` between the plan and the bridge joining it, the bridge creates it once on the span the plan would give it and joins again.
* When `kestrel-gw` has no address left, the bridge start fails with an error that says it is full, that other or leaked `kestrel-gateway-*` bridges are the likely cause, and how to free it.
* One gateway bridge container serves every run. It joins each run network with `docker network connect`. One bridge per run would double the containers and slow every start.
* The network's host side gets no address, so the bridge is the only way out of it.
* When every block is in use, new runs wait `Queued` (one event says why) and start when a run ends. When no block can come back (all are taken by networks outside Kestrel, or held by networks that could not be removed), a queued run fails at once and says to change `gateway.subnet_pool` or restart.
* Networks carry `kestrel.run` and `kestrel.instance` labels. `kestrel serve` removes leftover ones at startup.
* With `gateway.advertise_url` there is no bridge: all sandboxes share the `kestrel-gw` network and can reach each other. Serve prints a warning. The docker driver creates `kestrel-gw` from the default pools if it is missing, but one that serve created earlier is the explicit `/24`, which holds 253 sandboxes at once.

### 3.4.1 High-density server sandbox pools

Kestrel server mode will use its own high-density sandbox-pool design for long-lived, mostly-idle
Runs. The design takes inspiration from systems such as Google Agent Substrate: keep a small pool
of warm workers, suspend an idle sandbox, and restore its memory and filesystem state when work
arrives. Kestrel owns the interface, lifecycle semantics and deployment choices.

Kestrel keeps ownership of the product-facing state:

* **Kestrel:** Agent versions, Runs and spawn trees, capability attenuation, approvals, budgets,
  event log, model policy, routing and cost records.
* **Kestrel worker-pool driver:** worker placement, per-Run sandbox lifecycle, snapshots, and
  kernel and network isolation.

`WorkerPoolDriver` is added only in server mode, after the local gVisor and Apple `container`
drivers are proven. A long-lived Run or session maps to one Kestrel sandbox session. Short,
one-shot jobs continue to use the local drivers. Kestrel maps its runtime image, CPU, memory,
filesystem and network policy to a selected worker pool.

```text
Kestrel Run ── WorkerPoolDriver ── Kestrel sandbox session on a warm worker
                                               │
                                   Kestrel egress policy and workload identity
                                               │
                                               ▼
                                      Kestrel Gateway
```

The sandbox session has no provider credential and no direct provider egress. Its egress policy permits only
the Kestrel Gateway. A Kestrel-issued workload identity is exchanged over mTLS and bound to the
Run, so the Gateway can apply its capability, budget and model policy. Provider keys remain in the
Gateway. Snapshots may preserve a workspace and runtime memory, but never provider credentials or
Kestrel control-plane authority.

This keeps local mode a one-binary product while providing a path to active-only compute economics
when a self-hosted team needs Kubernetes-scale, long-lived agent sessions.

### 3.5 Kestrel Gateway (the "cheaper" engine)

Cost levers, in build order (biggest return for least effort first):

1. **Role tiers.** Model chosen by agent role and tier (`cheap`, `mid`, `frontier`), mapped per org to actual models.
   Spawned children default one tier down unless their definition says otherwise.
2. **Prompt caching.** Keep stable prefixes (system prompt, tool schemas, repo map) first and mark them cacheable;
   the gateway reports cache hit rate per agent.
3. **Context budgets.** Coding agents get Travsr `get_context(query, token_budget)` instead of file dumps.
4. **Hard budgets.** Per run and per tree; crossing 80% emits a warning event, crossing 100% suspends.
5. **Rule routing.** Deterministic rules (task type, size of input, tool-only turns) send easy calls to cheap tiers.
6. **Learned routing (later).** KEV-style classifier, only when local hardware can run it inside a latency budget
   that is measured on that host, with frontier fallback below a confidence threshold.
7. **Batch pricing** for scheduled, non-interactive single-shot jobs (nightly summaries, digests).

Every model call records: run, agent, role, provider, model, input/output/cached tokens, cost, latency, route reason.

#### 3.5.1 Stateful, cache-aware model routing

Routing is a decision over an agent session, not an independent choice for each request. A cheap
model can be the right choice for a bounded step yet be the wrong choice when switching loses a
warm context or makes recovery more likely. The Gateway therefore makes a durable `RouteDecision`
before a provider call:

1. **Admission policy:** enforce data residency, provider allow-list, required capabilities
   (tools, schema, vision and context), remaining budget and maximum in-flight cost.
2. **Cache affinity:** prefer the healthy model profile and provider cohort that already holds the
   Run's compatible prefix cache.
3. **Deterministic rules:** classify known low-risk work such as extraction, summarisation, log
   tagging and bounded tool-result interpretation.
4. **Escalation:** move up a tier only for an explicit signal: invalid structured output, repeated
   tool failure, failing tests, critic rejection, or a turn-limit or confidence threshold.
5. **Fallback:** retry only on a compatible healthy deployment. Record the reason and preserve the
   selected profile for the next call unless an escalation signal requires a change.

A `ModelProfile` replaces a bare `tier -> model name` map in server configuration. It describes a
provider and model plus capabilities, allowed data classes and roles, pricing for input, output and
cache reads/writes, latency and health limits, and the cache cohort it belongs to. `cheap`, `mid`
and `frontier` remain the portable policy interface in an Agent definition.

The Gateway reserves the configured maximum completion cost before starting a call and reconciles
the reservation against provider-reported usage. This prevents concurrent calls from spending more
than a Run or root-tree budget merely because they were admitted before their predecessors finished.

#### 3.5.2 SLMs as bounded workers

Small language models are useful where the input, output and success condition are narrow. Kestrel
uses a locally served SLM, when host capability and an evaluation gate allow it, for:

* route classification and task-difficulty scoring;
* structured extraction, normalisation and validation;
* log, diff and test-output summarisation;
* tool-result tagging and context compression;
* offline clustering of failed runs and proposed routing-rule improvements.

An SLM does not grant a capability, approve a side effect, act as the sole secret or PII detector,
or replace a coding agent for open-ended planning, novel debugging, multi-file changes or recovery.
Every SLM route is evaluated against a frozen task set and is promoted only when successful-task
cost, latency and quality improve without a regression.

#### 3.5.3 Cache contract

Kestrel distinguishes three cache layers:

| Layer | Purpose | Safety boundary |
| :--- | :--- | :--- |
| Provider prefix cache | Reuse a stable prompt prefix at Anthropic or OpenAI | Same org, provider, model and compatible rendered prefix |
| Local-model KV cache | Reuse local inference prefill, for example with vLLM automatic prefix caching | Same model weights, adapter and cache cohort; cache storage is sensitive |
| Semantic or tool-result cache | Reuse a pure read-only result | Exact tool arguments, input revision, capability and retention policy |

The Gateway does not treat an agent response or arbitrary mutable tool result as a semantic cache
entry. A result is cacheable only when the tool declares it pure and read-only, and its key includes
the source revision and policy version.

Every cacheable model request follows this immutable layout:

```text
policy version + agent version + role instructions + stable tool schemas
→ deterministically ordered repository/context bundle
→ append-only conversation history
→ current task + latest tool results + volatile state
```

The stable prefix is content-addressed as a `ContextBundle`. Sibling agents with the same Agent
version and bundle are scheduled close together and normally remain on the same model profile while
the prefix is warm. Dynamic content stays at the end. Provider-specific controls are used only when
the adapter's API contract supports them: Anthropic cache breakpoints, OpenAI cache keys or
breakpoints, and automatic prefix caching for a local vLLM server.

The Gateway reports `cache_profile`, `context_bundle_hash`, cache read/write tokens, hit ratio,
cache-write cost, time to first token and miss reason. These are the signals used to decide whether
a cache policy is helping rather than simply increasing cache-write spend.

### 3.6 Privacy perimeter

* **Secrets:** deterministic detectors (gitleaks-style rules + entropy) on both prompts and tool results entering
  context. Matches become stable placeholders like `[SECRET_3]`.
* **Unmasking happens at the tool boundary inside the perimeter**, never in text sent back out.
* **Vault:** per run tree, encrypted at rest (key from OS keychain locally, KMS or a file key on servers), deleted on
  terminal state, so suspended runs survive restarts.
* **Prose PII (later):** local NER/SLM pass for chat and meeting agents, where names and emails matter most.
* **Prompt injection:** tool output is never trusted to grant capability. Side-effect tools require tokens and,
  where configured, approvals. Content from repos, web pages and transcripts is labelled as untrusted input.

### 3.7 Context and memory

* **Code:** Travsr over MCP (stdio when local, Streamable HTTP when remote). Kestrel stores nothing about code structure.
* **Memory:** `MemoryProvider` trait. Built-in SQLite store (facts + embeddings via `sqlite-vec`) scoped by
  `org / project / agent / user`; optional Supermemory MCP backend for teams that already use it.

### 3.8 Workflows and self-improvement

* **Workflow templates**, not a hard-coded engine: EGM (Elephant drafts, Goldfish reviewers spawned with empty
  context, readiness gate, then implementer) ships as a template built from `kestrel.spawn`.
* **Harness improvement (Harness²-style):** an offline loop over failed runs that proposes edits to an agent's
  prompt, tools or watchdogs, replays the task, and keeps an edit only when replay shows it fixed the failure.
  Edits land as a new Agent version for a human to promote.

### 3.9 Org fine-tuning (later milestone)

* **Goal:** small local adapters that know the org: routing decisions, secret and PII detection, review style,
  internal architecture conventions.
* **Data:** the org's own material (code via Travsr, docs, human-approved run outputs, human edits). Check provider
  terms before including raw frontier outputs.
* **Trainer:** pluggable job on an optional GPU worker (Python/Unsloth in a sandbox image; Rust training is not
  mature enough). **Serving:** `candle` (CUDA/Metal/CPU).
* **Gate:** frozen eval set; promote only with improvement and no regression; adapters loaded side by side, not
  merged, so rollback is instant.

### 3.10 Integrations: Google Chat and the live meeting bot

Both sit behind a `Channel` trait (inbound events become Triggers; outbound messages are capability-checked
tools), so Slack, Teams or Zoom can be added later without touching the core.

**Google Chat**
* Connection type **Pub/Sub pull**, not an HTTP endpoint. Kestrel subscribes from inside the firewall, so it works
  on a laptop with no public URL. Trade-off: async only (no dialogs, no synchronous card updates; patch the whole
  message instead).
* Mentions and DMs start or continue a Run; approvals can be answered from a Chat card; run status posts back
  to the thread.
* Needs a Google Cloud project, a Chat app, a topic and a pull subscription. `kestrel channels add gchat` walks
  through it.

**Live meeting bot (Google Meet first)**
* Google is sunsetting the Meet Media API, the only official real-time media path for Meet, and points
  developers to the REST API, which has no live audio. So the bot cannot rely on an official Meet streaming API.
* Approach: a **headless browser bot**. Chromium runs in its own sandbox with a virtual audio device, signs in
  with a dedicated bot account, joins by link, and waits to be admitted by the host.
* Pipeline: meeting audio → streaming speech-to-text (local `whisper.cpp`-class model when hardware allows,
  otherwise a provider through the Gateway, metered like any model call) → live transcript events → a meeting
  agent that keeps notes and action items and can spawn follow-up runs.
* Capability stages: (1) listen, transcribe, live notes in the UI; (2) reply in meeting chat when addressed;
  (3) speak via TTS. Each stage is a separate capability so orgs can stop at "listen only".
* Build vs reuse: evaluate the open-source, self-hostable meeting-bot projects (for example Vexa, Attendee) in M0.
  Check licenses for Apache-2.0 compatibility and how resilient they are to Meet UI changes before writing our own.
* Risks: browser bots break when Meet's UI changes (keep selectors in one module with a canary test); workspace
  admins may block unknown accounts; recording consent varies by jurisdiction. The bot announces itself on join,
  and transcripts are stored under the org's retention policy.
* Later: Zoom (official real-time media streams) and Teams behind the same `MeetingConnector` trait.

## 4. UI

**Svelte 5 + Vite**, built as a static SPA (no SSR, no SvelteKit server) and embedded in the binary
(`rust-embed`), live over WebSocket. `kestrel up` starts the server on `127.0.0.1` and opens the browser.

Why Svelte 5 over React, SolidJS or htmx:
* Compiles away the framework, so bundles stay small and the binary stays light.
* Runes give fine-grained reactivity: a token arriving on one card updates that card only. No virtual DOM
  diffing on every streamed event, which matters when dozens of agents stream at once.
* Svelte Flow (official xyflow port) covers the spawn-tree graph; CodeMirror 6 covers the YAML editor.
* Astro (used by `travsr-web`) supports Svelte components, so styles and components can be shared later.
* SolidJS is equally fast but has a thinner ecosystem for graphs; htmx is lighter still but fights the
  graph view and the form/YAML two-way editor.

Styling: plain CSS with design tokens (custom properties), no CSS framework. Charts: a small SVG chart helper or
uPlot for the cost time series.

| View | Shows |
| :--- | :--- |
| **Fleet** | Every active run as a live card: agent, status, elapsed, cost ticker, sandbox tier. Filters by agent, trigger, status. |
| **Run** | Spawn tree (graph), event timeline, live token stream, tool calls, terminal output, diff viewer, budget bar. Pause, resume, cancel, inject a message. |
| **Approvals** | Inbox of pending decisions with full context; approve, reject, or edit and approve. |
| **Agents** | Builder with form and YAML in sync, version history, "test run" button. |
| **Triggers** | Schedules, webhooks, watchers; next fire time; history. |
| **Costs** | Spend by agent, role, model, project; cache hit rate; savings vs all-frontier baseline. |
| **Settings** | Providers and tier mapping, secrets, sandbox drivers detected, MCP servers (Travsr, memory). |

## 5. Data model (SQLite; same shape in Postgres)

```sql
CREATE TABLE agents      (name TEXT, version INTEGER, spec_json TEXT NOT NULL, created_at TEXT,
                          PRIMARY KEY (name, version));
CREATE TABLE runs        (id TEXT PRIMARY KEY, agent TEXT, agent_version INTEGER,
                          parent_run_id TEXT, root_run_id TEXT NOT NULL,
                          depth INTEGER NOT NULL DEFAULT 0, trigger_id TEXT,
                          status TEXT NOT NULL, budget_usd REAL, spent_usd REAL DEFAULT 0,
                          sandbox_driver TEXT, created_at TEXT, finished_at TEXT);
CREATE TABLE run_caps    (run_id TEXT PRIMARY KEY, token BLOB NOT NULL);
CREATE TABLE events      (cursor INTEGER PRIMARY KEY AUTOINCREMENT, run_id TEXT NOT NULL,
                          seq INTEGER NOT NULL, kind TEXT NOT NULL, payload_json TEXT,
                          ts TEXT NOT NULL, UNIQUE (run_id, seq));
CREATE TABLE model_calls (id TEXT PRIMARY KEY, run_id TEXT, provider TEXT, model TEXT, tier TEXT,
                          model_profile TEXT, route_reason TEXT, cache_profile TEXT,
                          context_bundle_hash TEXT, cache_miss_reason TEXT,
                          tokens_in INTEGER, tokens_out INTEGER, tokens_cache_read INTEGER,
                          tokens_cache_write INTEGER, reserved_cost_usd REAL, cost_usd REAL,
                          latency_ms INTEGER, ttft_ms INTEGER, ts TEXT);
CREATE TABLE approvals   (id TEXT PRIMARY KEY, run_id TEXT, action_json TEXT, status TEXT,
                          decided_by TEXT, decided_at TEXT);
CREATE TABLE triggers    (id TEXT PRIMARY KEY, agent TEXT, kind TEXT, spec_json TEXT,
                          enabled INTEGER, next_fire_at TEXT);
CREATE INDEX runs_parent ON runs(parent_run_id);
CREATE INDEX runs_root ON runs(root_run_id);
CREATE INDEX events_run ON events(run_id);
CREATE INDEX runs_status ON runs(status);
```

`events.cursor` is the global event cursor that `/ws/tree/{id}?after=` and `/ws/events?after=` hand to
clients. It is an explicit `AUTOINCREMENT` primary key, not SQLite's implicit rowid, so `VACUUM`
cannot renumber it and a value is never handed out twice. Schema v5 added it; the migration copies
every existing event with its old rowid as its cursor.

The first open at v5 rewrites the whole `events` table in one transaction (create `events_new`,
copy, drop `events`, rename, recreate the indexes):

* It takes time in proportion to the number of events.
* The WAL grows to about the size of the table. Leave roughly twice the events data free on disk.
* Other kestrel processes, an older `kestrel serve` included, are blocked meanwhile and may fail
  with "database is locked" after the 30 s busy timeout. Stop them before upgrading.
* If it fails (a full disk, say), nothing changes and the database stays at v4.

See [Upgrading](quickstart.md#upgrading).

Traces for future training are a view over `events` + `model_calls` + `approvals`, not a separate pipeline.

## 6. Workspace layout

```text
kestrel/
├── Cargo.toml                 # MSRV 1.89 (rusqlite 0.41)
├── crates/
│   ├── kestrel-types/         # Agent spec, Run, Event, errors
│   ├── kestrel-store/         # EventLog + state: SQLite (local), Postgres (server)
│   ├── kestrel-core/          # scheduler, run state machine, policy, Capability, budgets, triggers
│   ├── kestrel-sandbox/       # SandboxDriver: gvisor, firecracker, apple, docker-runc; egress proxy
│   ├── kestrel-gateway/       # LLM proxy: masking, routing, caching, metering
│   ├── kestrel-agent/         # native agent loop + adapters (claude-code, opencode, codex)
│   ├── kestrel-mcp/           # Kestrel MCP server (spawn, join, ask_human) + MCP client (travsr, memory)
│   ├── kestrel-server/        # axum API + WS, serves embedded UI
│   ├── kestrel-cli/           # `kestrel` binary: up, serve, run, watch, approve, agents
│   └── kestrel-ml/            # (later) candle local models, KEV-style classifier
└── web/                       # Svelte 5 + Vite static SPA, embedded via rust-embed
```

Dependency rule (no cycles): `types ← store ← core ← {sandbox, gateway, mcp, agent, channels} ← server ← cli`.
Add `kestrel-channels/` (Google Chat, meeting bot connector, later Slack/Teams/Zoom) when M4 starts.

Licensing: Apache-2.0. Copy Travsr's `deny.toml` so `cargo deny` blocks dependencies with licenses that aren't
Apache-compatible, plus RUSTSEC advisories, in CI from the first commit.

## 7. Milestones and acceptance criteria

**Team lanes (2 to 3 engineers).** Each milestone splits across lanes, with the API and event schema in
`kestrel-types` agreed first so lanes can work in parallel:
* **Lane A, Runtime (Rust):** core, store, sandbox drivers, gateway.
* **Lane B, Experience (Svelte + server API):** UI, CLI output, WebSocket protocol.
* **Lane C, Integrations (Rust):** agent adapters, MCP, Google Chat, meeting bot. With only 2 people, Lane C
  starts at M2, and the M0 meeting spike goes to Lane A.

**M0. Spikes (de-risk before building)**
* gVisor runs an agent container on Linux ARM64 and x86_64; Apple `container` runs one on macOS 26.
* Claude Code and OpenCode work unmodified with their base URL pointed at a pass-through gateway stub.
* Meeting bot: a headless Chromium in a sandbox joins a Meet call, captures audio, and produces a live transcript.
  Compare reusing an open-source bot against building our own.
* Measure: sandbox cold start per driver, gateway added latency, transcript lag.
* Exit: a short findings note; driver priority and meeting-bot build-vs-reuse decided.

**M1. Headless core (Linux and macOS)**
* Agent spec, run state machine, SQLite event log, gVisor and Apple `container` drivers, gateway pass-through
  with cost metering. CI runs on both Linux and macOS runners.
* Runtimes: `claude-code` and `opencode` adapters.
* CLI: `kestrel run`, `kestrel watch`, `kestrel approve`, pause/resume/cancel.
* Accept: a coding task runs headless in a sandbox, survives a server restart while suspended, and reports exact cost.

**M2. Parallel and spawn, plus the UI**
* `kestrel.spawn` / `kestrel.join` with Biscuit attenuation, budget carving, depth and concurrency limits. A parent
  waiting in `join` releases its concurrency slot, so a full cap cannot deadlock.
* Sandboxes reach these tools only through the gateway: `kestrel.*` at `/r/{token}/mcp` and Travsr at
  `/r/{token}/travsr`, authorised by the run token. A separate MCP port would be unreachable from a sandbox.
* Per-run git clone workspaces (branch `kestrel/run-<id>`, #22) instead of worktrees; Travsr wired over MCP.
* Parent and root run link in the store (schema v4); explicit subnets for per-run networks, since Docker's default
  address pools hold only about 30 networks.
* `kestrel-server`: HTTP and WebSocket API for the UI (loopback and token by default), serving the embedded UI.
* UI: Fleet, Run (spawn tree, live stream), Approvals. The 95% coverage rule also applies to `ui/src` (vitest).
* Accept: one run fans out to 5+ parallel children in separate per-run clones with no conflicts; a child cannot use a
  tool, host or secret its parent lacks (negative tests); children's total spend never exceeds the root budget by more
  than the calls in flight; the UI shows the whole tree live.
* Status: done. The acceptance suite in [m2.md](milestones/m2.md) (#45, 14 scenarios) passes in the `e2e-m2` CI
  job.

**M3. Cheaper**
* Model profiles, role tiers, cache-aware prompt shaping, context bundles, reserved hard budgets,
  deterministic sticky routing, secret masking and the Costs view.
* Local SLM helpers for bounded routing, extraction and summarisation only after a frozen-eval gate.
  An SLM cannot authorize a tool or make a security decision on its own.
* Benchmark suite: a fixed task set run (a) directly with the agent and (b) through Kestrel.
* Accept: lower cost per successful task than baseline with equal or better success rate, published as a
  report. The report includes p50/p95 time to first token and completion time, cache-read and
  cache-write rates, escalation rate, routing overhead and budget-reservation accuracy.

**M4. Triggers and agent catalog**
* Cron, webhooks, event watchers; Agents builder and Triggers views.
* Native agent runtime; templates: coding, PR review, Google Chat agent, EGM spec-first workflow.
* Google Chat channel over Pub/Sub pull, with approvals answerable from Chat cards.
* Accept: a PR opened on GitHub triggers a review agent that comments after approval, without any human starting
  it; a Google Chat mention starts a run on a laptop with no public URL and the reply lands in the same thread.

**M5. Live meeting bot and org intelligence**
* Meeting bot stage 1 and 2: joins Meet, live transcript in the UI, notes and action items, replies in meeting chat
  when addressed, spawns follow-up runs after the call.
* Memory provider; prose PII masking (meeting transcripts are the main source of personal data).
* Harness improvement loop; optional GPU worker for org adapters with eval gate; KEV-style routing where hardware allows.
* Accept: the bot joins a 30-minute Meet call, the transcript lags speech by under a few seconds in the UI, and
  action items become runs after the call; a promoted adapter beats its predecessor on the frozen eval set with
  no regression.

**M6. Team scale**
* Postgres backend and multiple workers, SSO and RBAC, audit log export, Firecracker tier, and a
  Kestrel-native high-density worker-pool driver for Kubernetes-scale deployments. Redis is added
  only if measured need.
* Meeting bot stage 3 (speaking via TTS); Zoom and Teams connectors; Slack channel.

## 8. Decisions log

| Decision | Choice |
| :--- | :--- |
| Deployment | Any local machine or self-hosted server, one binary |
| UI | Svelte 5 + Vite static SPA embedded in the binary |
| License | Apache-2.0, enforced with `cargo deny` |
| Team | 2 to 3 engineers, three lanes |
| Hosts in M1 | Linux and macOS |
| First chat surface | Google Chat via Pub/Sub pull |
| Meetings | Live meeting bot, Google Meet first, headless-browser approach (Meet Media API is being sunset) |
| Repo | Separate `Kestrel` repo; Travsr used only over MCP |
| Workspaces | Per-run git clones (#22), not git worktrees: a worktree's `.git` points outside the sandbox mount, so git fails inside it. The YAML value is `git-clone`; the optional `sandbox.clone_depth` makes the clone shallow |
| Sandbox tool access | `kestrel.*` and Travsr MCP are served under the gateway path (`/r/{token}/mcp`, `/r/{token}/travsr`), because the gateway is a sandbox's only way out |
| UI coverage | The 95% line rule covers Rust and the Svelte UI (vitest), enforced in CI |

## 9. Open questions

1. Is Google Meet the right first meeting platform (it pairs with Google Chat), or does Zoom matter more?
2. Should the meeting bot speak (TTS) in the first release, or is listen + chat reply enough until M6?
3. Who owns the Google Cloud project for the Chat app in self-hosted installs: Kestrel docs walk each org through
   creating their own (recommended), or a shared Kestrel-published app?

## 10. Design references

* [Google Agent Substrate on GKE](https://cloud.google.com/blog/products/containers-kubernetes/agent-substrate-available-on-gke)
  informs the warm-worker, snapshot and egress design. Kestrel does not depend on or embed it.
* [OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching) documents stable
  prefixes, cache keys, retention and cache-aware routing behavior.
* [Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching) documents
  explicit breakpoints, TTLs and cache usage accounting.
* [vLLM automatic prefix caching](https://docs.vllm.ai/en/v0.10.1/features/automatic_prefix_caching.html)
  documents KV reuse for compatible local-model prefixes.
* [Context-guided routing for stateful agents](https://arxiv.org/abs/2610.19292) motivates session-aware,
  cache-aware routing rather than independent per-call decisions.
