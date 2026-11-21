# Travsr in Kestrel

An agent can ask Travsr about the code it was given: who calls a function, what a change can
break, where a pattern appears. Kestrel indexes a private copy of each run's code with Travsr and
serves Travsr's read tools to the sandbox through the gateway. The sandbox gets no network path of
its own, and Travsr never touches the run's workspace.

Travsr is off unless `kestrel.toml` has a `[travsr]` section.

## What the agent gets

Grant the tools in the agent's capability, and give it a `git-clone` workspace:

```yaml
capabilities:
  mcp: [travsr.get_callers, travsr.search_symbol, travsr.get_blast_radius]
sandbox: { tier: auto, cpu: 1, memory: 2Gi, workspace: git-clone }
```

The agent then has an MCP server named `travsr` at `{gateway}/r/{run token}/travsr`, next to the
Kestrel tools at `/mcp`. It lists the granted tools by Travsr's names (`get_callers`); Claude Code
calls them `mcp__travsr__get_callers`.

* Every call answers about the code the run started from: its source repo at the commit its
  workspace was cloned at. The agent's own edits are not in the index (see
  [Security model](#security-model)).
* A child run is indexed on its own, from its own workspace's commit.
* A child holds at most its parent's tools, so a parent can pass on fewer.
* A tool that fails (Travsr down, too slow, an error) returns an MCP tool error with a short reason.
  The run goes on.

## The tools

Only these read tools are served. Anything else (`index_repo`, `repos_remove`, `get_index_status`,
`get_daemon_logs`, `synonym_*`, ...) is refused with "not permitted", and nothing reaches Travsr.
Such a name is neither echoed nor written to the run's log.

| Tool | Required arguments |
|---|---|
| `search_symbol` | `name` |
| `get_callers` | `symbol` |
| `get_dependencies` | `file` |
| `get_blast_radius` | `file` |
| `get_execution_path` | `source`, `sink` |
| `get_repo_map` | none |
| `get_context` | `query` |
| `find_references` | `symbol` |
| `find_pattern` | `pattern` |
| `get_snippets` | `symbols` |

The schemas the agent sees are Travsr's own, without a `repo` argument. A `repo` the agent sends
anyway is dropped: the run's Travsr server knows only the run's copy.

A read tool the run was not granted is refused too, and the run's log says so.

## Settings

```toml
[travsr]
command = "/usr/local/bin/travsr"   # default "travsr", looked up on PATH
index_timeout = 300                 # seconds one run may take to copy and index, 1 to 86400
call_timeout = 30                   # seconds one tool call may take, 1 to 3600
max_indexing = 2                    # runs indexed at once, 1 to 64
```

Every key is optional; an empty `[travsr]` turns Travsr on with these defaults. An unknown key is a
usage error. Without the section, `/r/{token}/travsr` answers 404 and no Travsr tool is listed.

## How a run is indexed and forgotten

1. The run's workspace is cloned (`git-clone` workspace). If the run's capability grants a
   `travsr.<tool>`, Kestrel reads the commit the new clone is at, before the sandbox exists.
2. While the sandbox is made, Kestrel clones the source repo again, into
   `<data dir>/travsr/<run id>/repo`, and checks out that commit. It uses the same git call as for
   the workspace (`--local --no-hardlinks`, hooks off, a scrubbed environment). The commit is
   recorded next to the copy.
3. Kestrel runs `travsr init --no-connect` in the copy. The agent starts once it finishes.
4. If copying or indexing fails or takes longer than `index_timeout`, the run starts anyway,
   without Travsr. Its log says why:

   ```
   kestrel: Travsr is unavailable for this run (indexing did not finish within 300 s); the agent starts without it
   ```

5. The run's first tool call starts `travsr mcp --stdio --db <copy>/.travsr/graph.db`, in the copy.
   Later calls of the run reuse it, one at a time. A server that died, or did not answer within
   `call_timeout`, is started again on the next call. A call whose request is dropped is stopped.
6. When `kestrel serve` stops, each run's server stops; the copies stay. A run resumed later
   reuses its copy and index. If they are gone, Kestrel makes them again from the source at the
   recorded commit.
7. When the run ends, Kestrel stops its server, runs `travsr repos --remove <copy>` and deletes
   `<data dir>/travsr/<run id>`. Each step is bounded (30 s for the server and `repos --remove`, 60
   s for the whole forget). If it fails, the run's log says so and Kestrel tries again every
   minute. It never fails the run, and never holds the run's slot.
8. When `kestrel serve` starts, it forgets every copy whose run ended or no longer exists.

## Security model

Travsr runs on the host, as Kestrel's user, outside every sandbox. So it only ever reads data the
agent cannot write.

* **Never the workspace.** The agent may rewrite anything in its workspace, `.git` included. Travsr
  would read those files on the host, run git in them, and serve files by the paths a database
  there names. So Travsr indexes and serves only Kestrel's private copy, and Kestrel itself runs
  git in a workspace only before its sandbox exists (the clone's checkout, and reading its commit).
* **The private copy** is at `<data dir>/travsr/<run id>/repo`. The `travsr` directory and each
  run's directory are owner-only (0700). No sandbox mounts them.
* **The index is checked before it is served.** `graph.db`, `.travsr`, the copy and the run's
  directory must be real directories and a regular file, inside the run's directory. A link is
  refused.
* **No daemon, no download, no connect files, no registry entry.** Every Travsr command gets
  `CI=1`, `TRAVSR_SKIP_DOWNLOAD=1` and `TRAVSR_DISABLE_REGISTRY=1`, and `init` gets `--no-connect`.
  Travsr starts its background daemon only without `CI`, both in `init` and in `mcp`, so none is
  ever left running. Hooks and `.travsrignore` that `init` writes land in the copy.
* **A minimal environment.** Travsr gets `PATH`, `HOME`, the user and locale, temp and XDG
  directories (and their Windows equivalents), `TRAVSR_BIN_DIR` and `TRAVSR_LANG_TOML`. Never a
  run token, a provider key, or any other `TRAVSR_*` variable.
* **Code Travsr may run.** Travsr's language pass may run build tooling of the indexed code (for
  Rust, `build.rs` and proc macros), and without bubblewrap it grants Rust that unsandboxed. It runs
  on the copy, which is the operator's source repo at a commit, never code an agent wrote.
* **The gateway** serves only the ten read tools a run was granted, drops `repo`, caps answers at
  256 KiB and every call at `call_timeout`, and answers 404 without a `[travsr]` section.

This is a deliberate limit: **the index shows the code the run started from, not the agent's
edits.** Indexing the agent's edits safely means running Travsr inside the sandbox, which is the
follow-up.

## Limits

| What | Limit |
|---|---|
| Answer size | 256 KiB of text; the rest is cut, ending with `[truncated by Kestrel: ...]` |
| One tool call | `call_timeout`, then a tool error |
| Indexing | `index_timeout` per run, `max_indexing` at once; the others wait their turn |
| Forgetting a run | 60 s, then retried every minute |
| Tool calls per run | 20 at once, 10 more per second, shared with the Kestrel tools |
| Request body | 1 MiB |

Known limits on Kestrel's side:

* The index is of the run's starting commit; the agent's edits never show (see above).
* The private directories are made owner-only on Unix only. On Windows they keep the data
  directory's permissions.
* With no `[travsr]` section, a spec that grants `travsr.*` tools still gets a `travsr` server
  entry in its MCP config, which answers 404.

## Known Travsr issues

Kestrel works around these issues of Travsr. Each workaround is listed with the issue.

* [#1111](https://github.com/Travsr-com/travsr/issues/1111): a long-running MCP server does not see
  repos registered after it started. Kestrel starts one server per run, after the run's copy is
  indexed, instead of sharing a global one.
* [#1117](https://github.com/Travsr-com/travsr/issues/1117): the `repo` argument resolves only as the
  exact absolute path Travsr registered. Each run's server serves one repo (`--db`), so no call needs
  a `repo`, and the gateway drops any `repo` the agent sends.
* [#1110](https://github.com/Travsr-com/travsr/issues/1110): one repo registered twice, and a shared
  repo name resolving to one of several checkouts. Every run's copy is a checkout of the same repo;
  binding each server to its copy's index, and registering none, keeps runs from reading each
  other's.
* [#1133](https://github.com/Travsr-com/travsr/issues/1133): no API indexes a repo by path. Kestrel
  runs `travsr init` with the copy as working directory and waits for it to exit.
* [#1134](https://github.com/Travsr-com/travsr/issues/1134): `travsr serve` (HTTP) has no
  `tools/list`, serves only some read tools and is not MCP streamable HTTP. Kestrel speaks to
  `travsr mcp --stdio` instead.
* [#1026](https://github.com/Travsr-com/travsr/issues/1026): references can be ambiguous between a
  type and its impl in the same file. Noted only; Kestrel passes Travsr's answers on as they are.

### Why `command`, not `url`

The issue that added Travsr (#44) proposed `[travsr] url = ...`, an address of a running Travsr.
None of Travsr's network servers can index a repo by path (#1133), serve every read tool with
`tools/list` (#1134), or see repos registered after they started (#1111). So Kestrel runs the
`travsr` command itself, and the setting names that command.
