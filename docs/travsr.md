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
* A tool that fails because Travsr's server could not start says only that. The reason, which
  may name host paths, goes to Kestrel's log on the host.
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
index_timeout = 300                 # seconds a run waits for its index, 1 to 86400
call_timeout = 30                   # seconds one tool call may take, 1 to 3600
max_indexing = 2                    # runs indexed at once, 1 to 64
```

Every key is optional; an empty `[travsr]` turns Travsr on with these defaults. An unknown key is a
usage error. Without the section, `/r/{token}/travsr` answers 404 and no Travsr tool is listed.

`command` is an absolute path or a bare name. A relative path (`./travsr`, `bin/travsr`) is a
usage error: Travsr runs inside the run's copy, where it would resolve. A bare name is looked up
once, when `kestrel serve` starts, in the absolute `PATH` entries only. If it is not found,
`serve` says so and every run starts without Travsr.

## How a run is indexed and forgotten

1. The run's workspace is cloned (`git-clone` workspace). If the run's capability grants a
   `travsr.<tool>`, Kestrel reads the commit the new clone is at, before the sandbox exists.
2. While the sandbox is made, Kestrel clones the source repo again, into
   `<data dir>/travsr/<run id>/repo`, and checks out that commit. It uses the same git call as for
   the workspace (`--local --no-hardlinks`, hooks off, a scrubbed environment), plus
   `core.symlinks=false`: a link the commit holds is a plain file in the copy, holding its
   target's path. The commit is recorded next to the copy. Each run gets its own full copy of
   the source repo, even when runs start from the same commit.
3. If the commit holds a `.travsr`, Kestrel deletes it from the copy, so `init` never uses a
   committed index. A committed `.travsr` link is a plain file in the copy, so it is deleted too.
4. Kestrel runs `travsr init --no-connect` in the copy. The agent starts once it finishes.
5. If copying or indexing fails, or the run waits longer than `index_timeout`, the run starts
   anyway, without Travsr. The wait for a free indexing slot (`max_indexing`) counts too, so a
   queued run starts at most `index_timeout` after it began to wait. Its log says why:

   ```
   kestrel: Travsr is unavailable for this run (indexing did not finish within 300 s); the agent starts without it
   ```

   A run that gives up kills its git and, on Unix, `init`'s whole process group, so helpers
   `init` started stop too. An `init` that ends by itself has its group left alone, since the
   group's id may be reused by then; a helper it left running keeps running.
6. The run's first tool call starts `travsr mcp --stdio --db <copy>/.travsr/graph.db`, in the copy.
   Later calls of the run reuse it, one at a time. A server that died, or did not answer within
   `call_timeout`, is started again on the next call. A call whose request is dropped is stopped.
   An idle server keeps running until its run ends.
7. When `kestrel serve` stops, each run's server stops; the copies stay. A run resumed later
   reuses its copy and index. If they are gone, Kestrel makes them again from the source at the
   recorded commit.
8. When the run ends, Kestrel stops its server and deletes `<data dir>/travsr/<run id>`. No
   Travsr command runs: `init` registered nothing. Only a run that was handed to the indexer, or
   whose copy recovery listed, is forgotten; a run without Travsr never reaches it. Stopping the
   server may take 30 s, the whole forget 60 s. If it fails, the run's log says so and Kestrel
   tries again every minute. It never fails the run, and never holds the run's slot.
9. When `kestrel serve` starts, it forgets every copy whose run ended or no longer exists.

## Security model

Travsr runs on the host, as Kestrel's user, outside every sandbox. So it only ever reads data the
agent cannot write.

* **Never the workspace.** The agent may rewrite anything in its workspace, `.git` included. Travsr
  would read those files on the host, run git in them, and serve files by the paths a database
  there names. So Travsr indexes and serves only Kestrel's private copy, and Kestrel itself runs
  git in a workspace only before its sandbox exists (the clone's checkout, and reading its commit).
* **The private copy** is at `<data dir>/travsr/<run id>/repo`. The `travsr` directory and each
  run's directory are owner-only (0700). No sandbox mounts them.
* **No links in the copy.** `init` writes files at the copy's root through whatever it finds
  there: it rewrites `.gitignore`, and writes `.travsrignore` when none exists (a link to nothing
  counts as none). A committed link could point those writes at any file Kestrel's user can
  write. So the copy is checked out with `core.symlinks=false` (also kept in its config): every
  committed link is a plain file holding its target's path.
* **No committed index.** Travsr writes its index to `<copy>/.travsr/graph.db`, through whatever
  `.travsr` it finds. So a `.travsr` from the commit is deleted before `init`. A `.travsr` that is
  a link fails indexing; the copy never holds one, so this check is defence in depth.
* **The index is checked before it is served.** `graph.db`, `.travsr`, the copy and the run's
  directory must be real directories and a regular file, inside the run's directory. A link is
  refused.
* **The command is absolute.** A bare `command` is resolved once at startup, in absolute `PATH`
  entries only, so no file in the copy can stand in for it.
* **No daemon, no download, no connect files, no registry entry.** Every Travsr command gets
  `CI=1`, `TRAVSR_SKIP_DOWNLOAD=1` and `TRAVSR_DISABLE_REGISTRY=1`, and `init` gets `--no-connect`.
  Travsr starts its background daemon only without `CI`, both in `init` and in `mcp`, so none is
  ever left running. The git hooks `init` installs go where git says the copy's hooks are
  (`git rev-parse --git-path hooks`). The copy's own config sets `core.hooksPath` to its own
  `.git/hooks` (and `core.fsmonitor=false`), so a global `core.hooksPath` of the operator's
  never sends them to a hooks directory shared with other repos. Its `.gitignore` and
  `.travsrignore` writes stay in the copy, as it holds no links (see above).
* **A minimal environment.** Travsr gets the absolute entries of `PATH` (a relative one would be
  looked up in the copy), `HOME`, the user and locale, temp and XDG directories (and their
  Windows equivalents), `TRAVSR_BIN_DIR` and `TRAVSR_LANG_TOML`. Never a run token, a provider
  key, or any other `TRAVSR_*` variable.
* **Code Travsr may run.** Travsr's language pass may run build tooling of the indexed code (for
  Rust, `build.rs` and proc macros), and without bubblewrap it grants Rust that unsandboxed. It runs
  on the copy: the operator's source repo at the commit the run started from. Kestrel never
  indexes a workspace, but that commit may include agent work the operator fetched back and
  merged into the source repo.
* **A lasting grant in `~/.travsr`.** When bubblewrap is missing, `travsr init` records that Rust
  may run unsandboxed, in the operator's `~/.travsr/lang.toml` (or the file `TRAVSR_LANG_TOML`
  names). The grant is keyed by the copy's `origin`, which is the source repo's path, so it holds
  for every later `init` of a clone of that source repo, and it stays after the run. Install
  bubblewrap, or point `TRAVSR_LANG_TOML` at a file of Kestrel's own, to keep it apart.
* **The gateway** serves only the ten read tools a run was granted, drops `repo`, caps answers at
  256 KiB and every call at `call_timeout`, counts listings and calls against the run's rate
  limit, and answers 404 without a `[travsr]` section.

This is a deliberate limit: **the index shows the code the run started from, not the agent's
edits.** Indexing the agent's edits safely means running Travsr inside the sandbox, which is the
follow-up.

### What a run sees

| What | In the run's answers |
|---|---|
| The source repo at the commit the run's workspace was cloned at | Yes |
| A file the agent added or changed in its workspace | No, not even after the agent commits it |
| Another run's edits, or its index | No: each run has its own copy and its own server |
| A later commit of the source repo | No, until a new run starts from it |

Seeing a run's own edits after a re-index (issue #96, scenario 1) waits on Travsr indexing a repo
by path from another process ([#1133](https://github.com/Travsr-com/travsr/issues/1133)) and
serving every read tool over MCP from a long-running server
([#1134](https://github.com/Travsr-com/travsr/issues/1134)). The test
`m2_s11_travsr_answers_from_the_run_s_own_copy` holds today's behaviour.

## Testing against a real Travsr

Unit tests use a fake `travsr` (a shell script). One test runs the real one:
`s12_the_index_answers_from_the_private_copy_and_never_sees_the_workspace`, behind the
`integration` feature.

```sh
TRAVSR_BIN=/usr/local/bin/travsr cargo test -p kestrel-cli --features integration --lib travsr::tests
```

* `TRAVSR_BIN` is an absolute path or a bare name (default `travsr`, looked up on `PATH`). A
  relative path fails the test.
* Without `KESTREL_TRAVSR_VERSION`, a travsr that is missing or does not run skips the test, and
  says so.
* With `KESTREL_TRAVSR_VERSION=1.4.0`, that fails the test, and so does a travsr whose
  `--version` reports another version.

CI's `travsr integration (pinned release)` job installs the pinned Travsr release, checks its
SHA-256, and runs this test with the version set. To move the pin, change
`KESTREL_TRAVSR_VERSION` and `KESTREL_TRAVSR_SHA256` in `.github/workflows/ci.yml`.

## Limits

| What | Limit |
|---|---|
| Answer size | 256 KiB of text; the rest is cut, ending with `[truncated by Kestrel: ...]` |
| One tool call | `call_timeout`, then a tool error |
| Indexing | `max_indexing` at once; a run waits at most `index_timeout`, its turn included |
| Forgetting a run | 60 s, then retried every minute |
| Tool calls and listings per run | 20 at once, 10 more per second, shared with the Kestrel tools |
| Request body | 1 MiB |

Known limits on Kestrel's side:

* The index is of the run's starting commit; the agent's edits never show (see above).
* Each run gets its own full copy of the source repo, so disk use grows with the runs that use
  Travsr at once. Sharing one copy per commit is a possible follow-up.
* A run's Travsr server stays up, idle, until the run ends.
* The private directories are made owner-only on Unix only. On Windows they keep the data
  directory's permissions.
* On Windows, a timed-out `init` is killed, but helpers it started may keep running.
* A helper that `init` leaves running when it ends by itself is not killed, on any platform, and
  forgetting the run could delete the copy under it. Today's Travsr leaves none with `CI=1`.
  Killing the group without reaping the leader first (`waitid` with `NOWAIT`) would close this,
  and the id-reuse window after a failed pipe read with it.
* With no `[travsr]` section, a spec that grants `travsr.*` tools still gets a `travsr` server
  entry in its MCP config, which answers 404.

## Travsr source this relies on

The security model above rests on these places in Travsr's source, checked at Travsr commit
`c1305034`. Paths are relative to the travsr repo; line numbers move, so search for the code
quoted. The release CI pins, v1.4.0 (`3649dc6e`), is older than that commit. It has
`init --no-connect`, `mcp --stdio --db`, and the `CI`, `TRAVSR_SKIP_DOWNLOAD` and
`TRAVSR_DISABLE_REGISTRY` checks, in the same functions.

| What Kestrel relies on | Where |
|---|---|
| `init` writes its index to `<repo>/.travsr/graph.db` | `crates/travsr-cli/src/init.rs` ~66 (`repo_root.join(".travsr/graph.db")`) |
| `init` rewrites `<repo>/.gitignore` and writes `<repo>/.travsrignore` when `!path.exists()` | `crates/travsr-daemon/src/lib.rs` ~863 (`scaffold_gitignore`) and ~667 (`scaffold_travsrignore`), called ~1842 and ~1850 |
| `init` puts its hooks where `git rev-parse --git-path hooks` says | `crates/travsr-daemon/src/hook.rs` ~185 (`install_hook`) and ~150 (`resolve_hooks_dir`) |
| `init` starts no daemon with `CI`, also when interrupted | `crates/travsr-cli/src/init.rs` ~191 and ~47 (`var_os("CI")`) |
| `mcp` starts no daemon with `CI` | `crates/travsr-cli/src/main.rs` ~1425 (`var_os("CI").is_none()`) |
| `init` downloads nothing with `TRAVSR_SKIP_DOWNLOAD` | `crates/travsr-cli/src/init.rs` ~87 |
| `--no-connect` writes no connect files | `crates/travsr-cli/src/init.rs` ~304 (`maybe_connect`) |
| No registry entry with `TRAVSR_DISABLE_REGISTRY=1` | `crates/travsr-daemon/src/lib.rs` ~1675 (`register`) and ~2522 (`mark_indexed`) |
| The index stores the `repo_root` it was made for | `crates/travsr-daemon/src/lib.rs` ~1827 (`set_meta("repo_root", ...)`) |
| The unsandboxed grant, and where it is saved | `crates/travsr-cli/src/init.rs` ~482 (`grant_unsandboxed_where_needed`), `crates/travsr-cli/src/lang.rs` ~1858 and ~2111 (`config_path`) |
| The grant's key: the repo's `origin` | `crates/travsr-daemon/src/lib.rs` ~6721 (`detect_corpus`) |

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
