# Travsr in Kestrel

An agent can ask Travsr about the code in its own workspace: who calls a function, what a change can
break, where a pattern appears. Kestrel indexes each run's clone with Travsr and serves Travsr's read
tools to the sandbox through the gateway. The sandbox gets no network path of its own.

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

* Every call answers about the run's own clone. A child run gets its own clone, indexed on its own.
* A child holds at most its parent's tools, so a parent can pass on fewer.
* A tool that fails (Travsr down, too slow, an error) returns an MCP tool error with a short reason.
  The run goes on.

## The tools

Only these read tools are served. Anything else (`index_repo`, `repos_remove`, `get_index_status`,
`get_daemon_logs`, `synonym_*`, ...) is refused with "not permitted", and nothing reaches Travsr.

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
anyway is dropped: the run's Travsr server knows only the run's clone.

## Settings

```toml
[travsr]
command = "/usr/local/bin/travsr"   # default "travsr", looked up on PATH
index_timeout = 300                 # seconds one clone may take to index, 1 to 86400
call_timeout = 30                   # seconds one tool call may take, 1 to 3600
max_indexing = 2                    # clones indexed at once, 1 to 64
```

Every key is optional; an empty `[travsr]` turns Travsr on with these defaults. An unknown key is a
usage error. Without the section, `/r/{token}/travsr` answers 404 and no Travsr tool is listed.

## How a clone is indexed and forgotten

1. The run's clone is made (`git-clone` workspace).
2. If the run's capability grants a `travsr.<tool>`, Kestrel runs `travsr init` with the clone as
   working directory, while the sandbox is made. The agent starts once it finishes.
3. Kestrel adds `.travsr/` to the clone's `.git/info/exclude`, so the agent never commits the index.
4. If indexing fails or takes longer than `index_timeout`, the run starts anyway, without Travsr. Its
   log says why:

   ```
   kestrel: Travsr is unavailable for this run (indexing did not finish within 300 s); the agent starts without it
   ```

5. The run's first tool call starts `travsr mcp --stdio --db <clone>/.travsr/graph.db`, in the clone.
   Later calls of the run reuse it, one at a time. A server that died is started again.
6. When the run's sandbox goes (the run ends, `kestrel serve` stops, or a restart cleans up), Kestrel
   stops the server and runs `travsr repos --remove <clone path>`. If that fails, the run's log says
   so and Kestrel tries again every minute. It never fails the run.

The index stays in the clone, as the clone does. A run resumed after a restart is indexed again.

## Limits

| What | Limit |
|---|---|
| Answer size | 256 KiB of text; the rest is cut, ending with `[truncated by Kestrel: ...]` |
| One tool call | `call_timeout`, then a tool error |
| Indexing | `index_timeout` per clone, `max_indexing` at once; the others wait their turn |
| Tool calls per run | 20 at once, 10 more per second, shared with the Kestrel tools |
| Request body | 1 MiB |

Travsr runs on the host as Kestrel's user. It gets only `PATH`, `HOME`, the locale, temp and XDG
directories (and their Windows equivalents) and `TRAVSR_*` variables from Kestrel's environment:
never a run token or a provider key.

Known limits on Kestrel's side:

* A clone is indexed once, before the agent starts. Edits the agent makes later are in the index
  only after the run is indexed again (when it is resumed after a restart).
* Forgets still failing when `kestrel serve` stops are not retried by the next start. Remove such an
  entry with `travsr repos --remove <clone path>`.

## Known Travsr issues

Kestrel works around these issues of Travsr. Each workaround is listed with the issue.

* [#1111](https://github.com/Travsr-com/travsr/issues/1111): a long-running MCP server does not see
  repos registered after it started. Kestrel starts one server per run, after the run's clone is
  indexed, instead of sharing a global one.
* [#1117](https://github.com/Travsr-com/travsr/issues/1117): the `repo` argument resolves only as the
  exact absolute path Travsr registered. Each run's server serves one repo (`--db`), so no call needs
  a `repo`, and the gateway drops any `repo` the agent sends.
* [#1110](https://github.com/Travsr-com/travsr/issues/1110): one repo registered twice, and a shared
  repo name resolving to one of several checkouts. Every run's clone is a checkout of the same repo;
  binding each server to its clone's index keeps runs from reading each other's checkout.
* [#1133](https://github.com/Travsr-com/travsr/issues/1133): no API indexes a repo by path. Kestrel
  runs `travsr init` with the clone as working directory and waits for it to exit.
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
