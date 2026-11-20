# Store test fixtures

`v3.db` is a real schema v3 database, written by the store at commit `49fbcfd` (the last v3 commit).
`tests/migration_v4.rs` copies it into a temp dir, opens the copy and checks nothing is lost.

Never open `v3.db` in place. Opening migrates the file, and the fixture must stay at v3.

## What `v3.db` contains

`PRAGMA user_version` is 3. The file is in WAL mode, checkpointed, with no `-wal` or `-shm` beside it.
SHA-256: `627d3ba7b2e97dd4cee0b9fe99e11cb784c29189f24812957c24fdb63298d4c8`.

### Runs (3)

| Agent | Run id | Input | Status | Created at (UTC) | Events | Spent (micro USD) |
|---|---|---|---|---|---|---|
| `alpha` | `01a1f9a6-a0fa-713c-9285-84f322e2bdf0` | `first task` | `Completed` | `2026-11-20T15:50:09.402919Z` | 6 | 2000000 |
| `beta` | `01a1f9a6-a0ff-77ec-a2d5-479a961d99ff` | `second task` | `Queued` | `2026-11-20T15:50:09.4071576Z` | 2 | 500000 |
| `gamma` | `01a1f9a6-a102-7752-a215-cf40438bdab3` | `third task` | `Cancelled` | `2026-11-20T15:50:09.4103933Z` | 2 | 0 |

Each run has one `run_specs` row and one `run_tokens` row. Every spec is:

```yaml
name: <agent>
runtime: opencode
role: r
model_policy: { default: tier:cheap }
sandbox: { tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }
budget: { usd: 1.00, wall_clock: 10m }
```

| Agent | Gateway token |
|---|---|
| `alpha` | `6bf4513999e8cb561871c8618a5fd313` |
| `beta` | `9fca38a322617a872d27ba1c5bc1d0aa` |
| `gamma` | `f32edea3504095b94d1a1a6676bd6b50` |

### Events (10)

The runs are interleaved on purpose, so the rowids of one run are not contiguous.
`run_created` has no `parent` field, as v3 wrote it.

| Rowid | Run | Seq | Kind |
|---|---|---|---|
| 1 | alpha | 1 | `run_created` (agent `alpha`) |
| 2 | beta | 1 | `run_created` (agent `beta`) |
| 3 | alpha | 2 | `status_changed` Queued to Provisioning |
| 4 | gamma | 1 | `run_created` (agent `gamma`) |
| 5 | alpha | 3 | `status_changed` Provisioning to Running |
| 6 | alpha | 4 | `agent_message` "hello from alpha" |
| 7 | beta | 2 | `agent_message` "hello from beta" |
| 8 | gamma | 2 | `status_changed` Queued to Cancelled |
| 9 | alpha | 5 | `status_changed` Running to Completed |
| 10 | alpha | 6 | `run_finished` (summary "done") |

### Model calls (4)

Provider `anthropic`, model `test-model`, 10 tokens in, 20 out, latency 5 ms, status 200.

| Id | Run | Cost (micro USD) |
|---|---|---|
| 1 | alpha | 1250000 |
| 2 | alpha | 750000 |
| 3 | alpha | none |
| 4 | beta | 500000 |

### Control commands (2)

| Id | Run | Command | Claimed |
|---|---|---|---|
| 1 | gamma | `Cancel` | yes |
| 2 | beta | `Pause` | no |

## How it was made

A throwaway integration test ran against the v3 code and was then deleted. It did, in order:

1. `Store::open` on a new file.
2. `create_run` three times (alpha, beta, gamma).
3. `append` and `transition` calls in the rowid order of the events table above.
4. `record_model_call` four times, in id order.
5. `push_control(gamma, Cancel)`, `take_control()`, `push_control(beta, Pause)`.
6. Dropped the store, which checkpoints the WAL and leaves one file.

Regenerating gives new run ids, tokens and timestamps, so update this file and the constants in
`tests/migration_v4.rs` if you ever do.
