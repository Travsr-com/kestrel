# Store test fixtures

Real databases written by older versions of the store. Migration tests copy one into a temp dir,
open the copy and check nothing is lost:

* `v3.db`: schema v3, used by `tests/migration_v4.rs` (and `tests/migration_v5.rs`).
* `v4.db`: schema v4 with gaps in the `events` rowids, used by `tests/migration_v5.rs`.

Never open a fixture in place. Opening migrates the file, and a fixture must stay at its version.

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

## How `v3.db` was made

A throwaway integration test ran against the v3 code and was then deleted. It did, in order:

1. `Store::open` on a new file.
2. `create_run` three times (alpha, beta, gamma).
3. `append` and `transition` calls in the rowid order of the events table above.
4. `record_model_call` four times, in id order.
5. `push_control(gamma, Cancel)`, `take_control()`, `push_control(beta, Pause)`.
6. Dropped the store, which checkpoints the WAL and leaves one file.

Regenerating gives new run ids, tokens and timestamps, so update this file and the constants in
`tests/migration_v4.rs` if you ever do.

## What `v4.db` contains

`PRAGMA user_version` is 4. The file is in WAL mode, checkpointed, with no `-wal` or `-shm` beside it.
SHA-256: `0519413f0a498a17658a1d3ffc45af7a7ce2c1d5f0568744fe7fbb852d1d32ee`.

### Runs (3)

All are `Queued`, each with one `run_specs` row (the spec above, named after the agent) and one
`run_tokens` row. `run_caps`, `model_calls` and `control` are empty.

| Agent | Run id | Input | Parent | Gateway token |
|---|---|---|---|---|
| `alpha` | `01a203e6-b979-7376-ae42-2b2c01c2ae75` | `first task` | none (root) | `1234feeb6db618a6fd5b04b36da085ab` |
| `beta` | `01a203e6-b99f-7440-a2a4-ef8fc3b1efc9` | `second task` | none (root) | `db66ab368ec118a11534e4b077f04a99` |
| `child` | `01a203e6-b9be-738a-a3a5-c92f7f2bbde6` | `child task` | `alpha` (depth 1) | `19271581d8a5ca8971bb3087b887c1ba` |

### Events (7, with gaps)

The rowid is the global cursor. Rowids 2, 4, 7 and 8 belonged to a fourth run (`scratch`, id
`01a203e6-b97e-750e-87fa-ec25468ff79f`) whose rows were deleted by hand, so the cursor has gaps.

| Rowid | Run | Seq | Kind |
|---|---|---|---|
| 1 | alpha | 1 | `run_created` (agent `alpha`, no parent) |
| 3 | beta | 1 | `run_created` (agent `beta`, no parent) |
| 5 | child | 1 | `run_created` (agent `child`, parent `alpha`) |
| 6 | alpha | 2 | `agent_message` "hello from alpha" |
| 9 | beta | 2 | `agent_message` "hello from beta" |
| 10 | child | 2 | `agent_message` "hello from child" |
| 11 | alpha | 3 | `agent_message` "alpha again" |

## How `v4.db` was made

A throwaway integration test ran against the v4 code at commit `8949305` and was then deleted:

1. `Store::open` on a new file, `create_run` for alpha, scratch and beta, `create_child_run` for
   child under alpha (before its first event).
2. `append` calls in the rowid order above, with scratch's events at rowids 2 (`run_created`), 4,
   7 and 8 (`agent_message`).
3. Dropped the store, then on a raw connection deleted scratch's `events`, `run_specs`,
   `run_tokens` and `runs` rows, and closed it (which checkpoints the WAL).

Regenerating gives new run ids, tokens and timestamps, so update this file and the constants in
`tests/migration_v5.rs` if you ever do.
