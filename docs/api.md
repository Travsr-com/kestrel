# API reference

The HTTP and WebSocket API that the web UI and scripts use. `kestrel up` starts it next to the
engine and the gateway, on `127.0.0.1:7171` by default.

```sh
kestrel up            # prints: Open the UI at http://127.0.0.1:7171/?token=<token>
kestrel up --no-open  # the same, without opening a browser
```

## Access

* **Token.** Every `/api` and `/ws` request needs the token: `Authorization: Bearer <token>`. It is
  made on first start and kept in `server.token` next to the database (mode 0600). Without it the
  answer is `401`. A token you write into that file yourself must be at least 32 letters, digits,
  `-` or `_`.
* **The printed address carries the token.** When `kestrel up` opens your browser, the address is
  on the browser's command line, which other users of the same machine can see while the browser
  runs. On a shared machine use `--no-open` and paste the token into the UI instead.
* **Host and Origin.** `Host` must be the address the server listens on (`127.0.0.1:7171` or
  `localhost:7171` by default), and a browser's `Origin` must be that same address. Anything else
  gets `403`. This stops other web pages, and DNS names pointed at 127.0.0.1, from calling it.
  There is no CORS.
* **Listen address.** `--listen 0.0.0.0:7171` works, with a warning: the token is then all that
  protects it. On such a bind, `Host` must be an IP address, not a name.
* **The UI** (`/`, `/fleet`, `/runs/<id>`, and its files) needs no token: it holds no data. Any
  path that is not under `/api` or `/ws` and has no file extension gets the UI's `index.html`.
  Unknown `/api` and `/ws` paths get a JSON `404`.

The examples set the token once. The file is next to the database: `$XDG_DATA_HOME/kestrel/` when
that is set, else `~/.local/share/kestrel/` on Linux and `~/Library/Application Support/kestrel/`
on macOS, or the folder of `--db`.

```sh
export T=$(cat ~/.local/share/kestrel/server.token)
export K=http://127.0.0.1:7171
```

Errors are JSON: `{"error": "no run 0193..."}`. Ids are run and approval UUIDs. Amounts are
micro-dollars (`420000` is $0.42). The shapes are the types in `crates/kestrel-types/src/api.rs`.

## Runs

| Route | What it returns |
| :--- | :--- |
| `GET /api/runs?status=&agent=&root=` | Runs, newest first. `status` is `queued`, `provisioning`, `running`, `suspended`, `completed`, `failed` or `cancelled`. Filters combine. |
| `GET /api/runs/{id}` | One run: status, agent, parent, root, depth, spent, limit, sandbox tier, created time, and whether its agent takes messages (`takes_messages`). |
| `GET /api/runs/{id}/tree` | Every run in the tree of `{id}`'s root, each parent before its children. |
| `GET /api/runs/{id}/events?after=<seq>` | The run's events after `seq` (0 for all). |

```sh
curl -H "Authorization: Bearer $T" "$K/api/runs?status=running"
curl -H "Authorization: Bearer $T" "$K/api/runs/$RUN"
curl -H "Authorization: Bearer $T" "$K/api/runs/$RUN/tree"
curl -H "Authorization: Bearer $T" "$K/api/runs/$RUN/events?after=4"
```

## Run commands

Each answers `202` with no body once the command is in the store's control channel. The engine
applies it like the same CLI command.

A command the run's status does not allow is refused with `409` and the reason, and nothing is
sent:

```json
{"error": "run 0193...: a queued run cannot be paused, only a running one"}
```

| Route | Same as | Allowed when the run is |
| :--- | :--- | :--- |
| `POST /api/runs/{id}/pause` | `kestrel pause` | running |
| `POST /api/runs/{id}/resume` | `kestrel resume` | suspended, but not waiting on an approval (approve or reject it instead) |
| `POST /api/runs/{id}/cancel` | `kestrel cancel` | queued, provisioning, running or suspended; or ended with a live run below it |
| `POST /api/runs/{id}/budget` with `{"usd": "5.00"}` | `kestrel budget`. A negative or non-decimal amount is `400`. | queued, provisioning, running or suspended |
| `POST /api/runs/{id}/message` with `{"text": "..."}` | `kestrel message`. See [Messages](#messages). | running, and its agent takes messages |

A completed, failed or cancelled run has ended: every command answers `409`
(`the run has ended (completed), so it cannot be paused`). One exception: cancel on an ended run
that still has a live run below it (it kept its children, `spawn.keep_children`) answers `202` and
cancels that live tree. The ended run itself keeps its status. Once nothing below it is live,
cancel is `409` too.

A status can still change between this check and the engine reading the command; the engine then
ignores the command and notes it in the run's log.

```sh
curl -X POST -H "Authorization: Bearer $T" "$K/api/runs/$RUN/pause"
curl -X POST -H "Authorization: Bearer $T" "$K/api/runs/$RUN/resume"
curl -X POST -H "Authorization: Bearer $T" "$K/api/runs/$RUN/cancel"
curl -X POST -H "Authorization: Bearer $T" -d '{"usd":"5.00"}' "$K/api/runs/$RUN/budget"
curl -X POST -H "Authorization: Bearer $T" -d '{"text":"also fix the docs"}' "$K/api/runs/$RUN/message"
```

## Messages

`POST /api/runs/{id}/message` gives a running agent a message from a person. The agent takes it
with the `kestrel.messages` tool, which it is told to call between steps.

* **Which runs take one.** A run whose spec grants `kestrel.messages` (in `capabilities.mcp`).
  `GET /api/runs/{id}` says so in `takes_messages`. Both runtimes (Claude Code and OpenCode) take
  messages when granted.
* **Answers.** `202` once the message is stored. `409` with the reason for a run that is not
  running (`a queued run cannot take a message, only a running one`), has ended, or whose agent
  cannot take messages. `400` for a body that is not `{"text": "<string>"}`, an empty text, or a
  control character other than a newline (`\n`) or a tab (a carriage return is refused too). `413`
  for a text over 16 KiB (16384 bytes). The body is checked before the run.
* **Events.** `message_sent` (`{"kind": "message_sent", "text": "..."}`) when it is stored, and
  `message_delivered` (`{"kind": "message_delivered", "seqs": [4, 7]}`, the `seq`s of the
  `message_sent` events it handed over) when the agent first took it.
* **What the agent gets.** `{"messages": [{"seq": 4, "text": "..."}]}`, oldest first, and an empty
  list when there is none. The text is not logged anywhere else. It is in the run's events, so
  anyone with the API token can read it: do not put secrets in it.
* **Delivery.** `kestrel.messages()` returns the messages not taken yet: at most once, so a message
  is lost if serve stops after recording the delivery and before the answer reaches the agent.
  `kestrel.messages({"after": <seq>})`, with the `seq` of the last message the agent got (`0` for
  none yet), returns every later message, taken or not: at least once, so a retry is safe. The
  agent note tells the agent to pass `after`. Answers are never kept by JSON-RPC id, so a client
  that reconnects and starts its ids again still reads the inbox.
* **A run that ends first.** A message the agent never took stays `message_sent` with no
  `message_delivered`. Nothing delivers it later, and an ended run takes no new message.

## Approvals

| Route | What it does |
| :--- | :--- |
| `GET /api/approvals?status=pending` | Approvals in request order. `status` is `pending`, `approved`, `rejected` or `expired` (undecided, and its run is not waiting on it, so it cannot be decided now); leave it out for all. |
| `POST /api/approvals/{id}/approve` | Approves. Optional body `{"payload": "<edited action>"}` for "edit and approve"; it is stored on the `approval_resolved` event. |
| `POST /api/approvals/{id}/reject` | Rejects. Optional body `{"reason": "..."}`, stored on the event and in the run's failure message. |

Approve and reject answer `202`, `404` for an unknown approval, `409` when it is resolved
already or its run no longer waits on it, and `400` for a body field the action does not take
(`reason` on approve, `payload` on reject). When two people decide at once, both get `202` and
the engine applies the first.

```sh
curl -H "Authorization: Bearer $T" "$K/api/approvals?status=pending"
curl -X POST -H "Authorization: Bearer $T" -d '{"payload":"git push origin fix"}' "$K/api/approvals/$APPROVAL/approve"
curl -X POST -H "Authorization: Bearer $T" -d '{"reason":"not on main"}' "$K/api/approvals/$APPROVAL/reject"
```

## Event sockets

| Route | What it streams |
| :--- | :--- |
| `GET /ws/runs/{id}?after=<seq>` | One run's events after `seq`, then live ones. |
| `GET /ws/tree/{id}?after=<cursor>` | Events of every run in the tree of `{id}`'s root, after the global `cursor`, then live ones, in commit order. |
| `GET /ws/events?after=<cursor>&kinds=<kind,...>` | Events of every run after the global `cursor`, then live ones, in commit order. Without `after`, only events from now on. `kinds` keeps only those event kinds, such as `approval_requested,approval_resolved`. |

* **Frames** are JSON: `{"type": "event", "cursor": 17, "event": {...}}`, and
  `{"type": "heartbeat"}` every 15 s.
* **Reconnecting.** Send the last `cursor` back as `after=`. Nothing is skipped and nothing comes
  twice.
* **Token.** Browsers cannot set headers on a WebSocket, so the token travels as subprotocols:
  `kestrel.v1, kestrel.token.<token>` (the server answers `kestrel.v1`), or as `?token=`.
* **Refusals and close frames.** A missing or wrong token is refused before the socket opens
  (`401`). Close code 4401 is reserved for a token refused after it opens. A client that stops
  reading is dropped once its queue stays full for 10 s (`ws_stall`): it gets close code 1008,
  "the client is not reading", and has `ws_close_grace` (10 s by default) to read what was sent
  and that frame before the connection is cut. When the server stops, every socket gets a close
  frame (code 1001) and at most 1 s to take it, a slow client in its grace too.

```sh
websocat -H "Authorization: Bearer $T" "ws://127.0.0.1:7171/ws/runs/$RUN?after=0"
websocat -H "Authorization: Bearer $T" "ws://127.0.0.1:7171/ws/tree/$RUN?after=0"
```
