#!/bin/sh
# Behaves like `claude --bare -p --output-format stream-json --verbose --model X
# --permission-mode bypassPermissions -- <input>`, just enough for Kestrel's Claude Code adapter.
#
# It makes one model call through the gateway, records what the tests check in /workspace,
# and prints a system/init line, an assistant line and a final result line.
#
# Files written to /workspace:
#   env.txt      the environment (scenario 4: no real provider key)
#   egress.txt   one line per outside host tried, "blocked <curl exit code> <url>" or "reachable <url>" (scenario 3)
#   README.txt   one line appended, so a tracked file changes (scenario 1)
#   git.txt      only when the input contains `git=1` (scenario 6): the agent commits inside its
#                git clone and records `branch=`, `status_rc=`, `add_rc=`, `commit_rc=` and `log=`
#   mcp.txt      only when the input contains `mcp=1` (issue #42): the answers to an MCP
#                `initialize`, `tools/list` and a `tools/call` of kestrel.join (as an event
#                stream) at the gateway's /r/<token>/mcp
#   spawn.txt    only when the input contains `spawn=1` (issue #43): the `--allowedTools` it was
#                given, whether it could write to /kestrel, then the answers to a `tools/list` and a
#                `tools/call` of kestrel.spawn (with wait) at the Kestrel server URL read from the
#                `--mcp-config` file, the way a real runtime finds it
#
# If the input contains `sleep=N`, it sleeps N seconds after the model call, once per
# workspace (a marker file remembers it), so a test can act while the run is in flight.
#
# M2 markers (issue #45). Each is a whole word `name=value` of the input, so `c.nap=4` is not
# `nap=4`. The MCP calls go to the Kestrel server URL in the `--mcp-config` file, as `spawn=1`.
#
# A parent:
#   fanout=N     spawn N children with kestrel.spawn (no wait), then kestrel.join all of them.
#                Child ids go to kids.out (one per line, so a restarted parent spawns only the
#                ones it has not), the spawn answers to fanout.out, `joining` to joining.out
#                just before the join, and the join answer to join.out.
#   kids=NAME    the agent to spawn (default `worker`); each=USD the budget_usd of each child;
#                first=S sleep S seconds before the first spawn (once per workspace); gap=S sleep
#                S seconds between spawns.
#   c.WORD       passed on to every child as WORD, after `kid=<n>` (so `c.c.nap=9` reaches a
#                grandchild as `nap=9`).
# A child:
#   nap=N        sleep N seconds after the model call, once per workspace.
#   hold=N       after the model call, write holding.out and wait until the test writes `go` into
#                the workspace, for at most N seconds; then write `go` or `timeout` to held.out.
#   spend=N      make model calls until N were made or the gateway refuses one (spend.out); after a
#                refusal it waits, so the engine suspends it while it runs.
#   hosts=1      also try api.github.com and evil.example (egress.txt).
#   tools=1      the `--allowedTools`, a tools/list, a call of kestrel.ask_human and a call of
#                kestrel.join with no children (tools.out).
#   ask=1        asks a person with kestrel.ask_human (ask.out) and writes the answer (answer.out).
#   escalate=1   spawns with a budget above its own, then a grandchild with escalate=2 and wait
#                (escalate.out); escalate=2 lists its tools and tries to spawn (escalate.out).
#   dump=1       everything under /kestrel, listed and read at every depth, and the mounts
#                (dump.out).
#   commit=1     at the end writes kid-<n>.txt (shared.txt with same=1), commits everything but
#                the test's own files in one commit, and records the result (commit.out).
set -u

model="unknown"
mcp_config=""
allowed=""
while [ $# -gt 0 ]; do
  case "$1" in
    --model) model="${2:-unknown}"; shift 2 ;;
    --mcp-config) mcp_config="${2:-}"; shift 2 ;;
    --allowedTools) allowed="${2:-}"; shift 2 ;;
    --output-format | --permission-mode | --append-system-prompt) shift 2 ;;
    --) shift; break ;;
    *) shift ;;
  esac
done
input="$*"
session="00000000-0000-4000-8000-0000000e2e00"

result_line() { # $1 = is_error (true/false), $2 = result text without quotes
  printf '{"type":"result","subtype":"success","session_id":"%s","is_error":%s,"result":"%s","num_turns":1}\n' \
    "$session" "$1" "$2"
}

# The value of the first word `$1=value` of the input; nothing (and status 1) when there is none.
marker() (
  set -f
  for word in $input; do
    case "$word" in
      "$1="*) printf '%s' "${word#*=}"; exit 0 ;;
    esac
  done
  exit 1
)

# The input of child number $1: `kid=$1`, then every `c.WORD` of this input as `WORD`.
child_input() (
  set -f
  out="kid=$1"
  for word in $input; do
    case "$word" in
      c.*) out="$out ${word#c.}" ;;
    esac
  done
  printf '%s' "$out"
)

try_host() { # $1 = url; one line in egress.txt
  curl -s -o /dev/null --max-time 5 "$1"
  code=$?
  if [ "$code" -eq 0 ]; then
    echo "reachable $1" >> /workspace/egress.txt
  else
    echo "blocked $code $1" >> /workspace/egress.txt
  fi
}

model_call() { # one Messages API call through the gateway; status 1 when it failed
  body=$(printf '{"model":"%s","max_tokens":64,"messages":[{"role":"user","content":"echo"}]}' "$model")
  curl -sS -f --max-time 30 -o /tmp/response.json \
    -H "x-api-key: ${ANTHROPIC_API_KEY:-}" \
    -H "anthropic-version: 2023-06-01" \
    -H "content-type: application/json" \
    -d "$body" "${ANTHROPIC_BASE_URL:-}/v1/messages" 2> /tmp/curl.err
}

# The Kestrel server URL from the MCP config Kestrel mounted at /kestrel.
kestrel_url() {
  sed -n 's/.*"kestrel":{[^}]*"url":"\([^"]*\)".*/\1/p' "$mcp_config" 2> /dev/null
}

mcp_call() { # $1 = request, $2 = seconds; the answer on stdout
  curl -sS -f --max-time "$2" -H "content-type: application/json" \
    -H "accept: application/json, text/event-stream" -d "$1" "$(kestrel_url)"
}

env > /workspace/env.txt

: > /workspace/egress.txt
for url in https://example.com https://1.1.1.1; do
  try_host "$url"
done
if marker hosts > /dev/null; then
  for url in https://api.github.com https://evil.example; do
    try_host "$url"
  done
fi

if ! model_call; then
  echo "echo-agent: gateway call failed: $(cat /tmp/curl.err)" >&2
  result_line true "gateway call failed"
  exit 1
fi

printf '{"type":"system","subtype":"init","session_id":"%s","cwd":"/workspace","model":"%s","permissionMode":"bypassPermissions","tools":[]}\n' \
  "$session" "$model"

secs=$(printf '%s' "$input" | sed -n 's/.*sleep=\([0-9][0-9]*\).*/\1/p')
if [ -n "$secs" ] && [ ! -e /workspace/.echo-agent-slept ]; then
  touch /workspace/.echo-agent-slept
  sleep "$secs"
fi

case "$input" in
  *mcp=1*)
    # The Kestrel tools, reached the only way out of the sandbox: the gateway, through the bridge.
    mcp() {
      curl -sS -f --max-time 30 -H "content-type: application/json" \
        -H "accept: application/json, text/event-stream" -d "$1" "${ANTHROPIC_BASE_URL%/}/mcp"
    }
    {
      mcp '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"echo-agent","version":"1"}}}'
      echo
      mcp '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
      echo
      mcp '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"kestrel.join","arguments":{}}}'
      echo
    } > /workspace/mcp.txt 2>&1
    ;;
  *spawn=1*)
    # The Kestrel server as a real runtime finds it: in the MCP config Kestrel mounted at /kestrel.
    url=$(sed -n 's/.*"kestrel":{[^}]*"url":"\([^"]*\)".*/\1/p' "$mcp_config" 2> /dev/null)
    call() { # $1 = request, $2 = seconds
      curl -sS -f --max-time "$2" -H "content-type: application/json" \
        -H "accept: application/json, text/event-stream" -d "$1" "$url"
    }
    {
      echo "allowed=$allowed"
      if touch /kestrel/agent-was-here 2> /dev/null; then echo "config_writable=yes"; else echo "config_writable=no"; fi
      case "$url" in
        */mcp) echo "url=ok" ;;
        *) echo "url=missing" ;;
      esac
      call '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' 30
      echo
      call '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"kestrel.spawn","arguments":{"agent":"echo","input":"child task","budget_usd":"0.50","wait":true}}}' 150
      echo
    } > /workspace/spawn.txt 2>&1
    ;;
  *git=1*)
    # What a real coding agent does: configure an identity, look, write, add, commit. All of it
    # inside /workspace, which must be a self-contained git clone for this to work.
    export HOME=/tmp
    cd /workspace || exit 1
    git config user.email agent@example.invalid
    git config user.name "echo agent"
    git status --porcelain > /dev/null 2>&1
    status_rc=$?
    echo "agent work" > agent-commit.txt
    git add agent-commit.txt
    add_rc=$?
    git commit -q -m "agent commit" > /tmp/commit.out 2>&1
    commit_rc=$?
    {
      echo "branch=$(git rev-parse --abbrev-ref HEAD 2>&1)"
      echo "status_rc=$status_rc"
      echo "add_rc=$add_rc"
      echo "commit_rc=$commit_rc"
      echo "log=$(git log --oneline -1 2>&1)"
    } > /workspace/git.txt
    ;;
esac

# M2 (issue #45) ------------------------------------------------------------------------------

kid=$(marker kid || echo 0)

spend=$(marker spend || true)
if [ -n "$spend" ]; then
  # The call above was the first one.
  calls=1
  refused=no
  while [ "$calls" -lt "$spend" ]; do
    if ! model_call; then
      refused=yes
      break
    fi
    calls=$((calls + 1))
  done
  echo "calls=$calls refused=$refused" > /workspace/spend.out
  if [ "$refused" = yes ]; then
    # Out of budget: wait for the engine to suspend the run, as a real agent would be cut off.
    sleep 600
  fi
fi

if marker tools > /dev/null; then
  {
    echo "allowed=$allowed"
    echo "== tools/list"
    mcp_call '{"jsonrpc":"2.0","id":"list","method":"tools/list"}' 30
    echo
    echo "== kestrel.ask_human"
    mcp_call '{"jsonrpc":"2.0","id":"ask","method":"tools/call","params":{"name":"kestrel.ask_human","arguments":{"question":"may I?"}}}' 30
    echo
    echo "== kestrel.join"
    mcp_call '{"jsonrpc":"2.0","id":"join","method":"tools/call","params":{"name":"kestrel.join","arguments":{}}}' 30
    echo
  } > /workspace/tools.out 2>&1
fi

if marker dump > /dev/null; then
  {
    echo "== ls -laR /kestrel"
    ls -laR /kestrel 2>&1
    # Every file at any depth, not only the top level.
    find /kestrel -type f 2>&1 | while IFS= read -r f; do
      echo "== $f"
      cat "$f" 2>&1
      echo
    done
    echo "== /proc/mounts"
    cat /proc/mounts 2>&1
  } > /workspace/dump.out 2>&1
fi

case "$(marker escalate || true)" in
  1)
    agent=$(marker kids || echo worker)
    {
      echo "== spawn over budget"
      mcp_call "{\"jsonrpc\":\"2.0\",\"id\":\"over\",\"method\":\"tools/call\",\"params\":{\"name\":\"kestrel.spawn\",\"arguments\":{\"agent\":\"$agent\",\"input\":\"kid=$kid.0 escalate=2 kids=$agent\",\"budget_usd\":\"5.00\"}}}" 60
      echo
      echo "== spawn grandchild"
      mcp_call "{\"jsonrpc\":\"2.0\",\"id\":\"grandchild\",\"method\":\"tools/call\",\"params\":{\"name\":\"kestrel.spawn\",\"arguments\":{\"agent\":\"$agent\",\"input\":\"kid=$kid.1 escalate=2 kids=$agent dump=1\",\"budget_usd\":\"0.02\",\"wait\":true}}}" 300
      echo
    } > /workspace/escalate.out 2>&1
    ;;
  2)
    agent=$(marker kids || echo worker)
    {
      echo "== tools/list"
      mcp_call '{"jsonrpc":"2.0","id":"list","method":"tools/list"}' 30
      echo
      echo "== spawn below max_depth 0"
      mcp_call "{\"jsonrpc\":\"2.0\",\"id\":\"deeper\",\"method\":\"tools/call\",\"params\":{\"name\":\"kestrel.spawn\",\"arguments\":{\"agent\":\"$agent\",\"input\":\"kid=$kid.1\",\"budget_usd\":\"0.01\"}}}" 60
      echo
    } > /workspace/escalate.out 2>&1
    ;;
esac

if marker ask > /dev/null; then
  question="Which colour should kid $kid use?"
  answer=$(mcp_call "{\"jsonrpc\":\"2.0\",\"id\":\"ask\",\"method\":\"tools/call\",\"params\":{\"name\":\"kestrel.ask_human\",\"arguments\":{\"question\":\"$question\"}}}" 900 2>&1)
  printf '%s\n' "$answer" > /workspace/ask.out
  printf '%s\n' "$answer" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p' | head -n 1 > /workspace/answer.out
fi

n=$(marker fanout || true)
if [ -n "$n" ]; then
  agent=$(marker kids || echo worker)
  each=$(marker each || true)
  first=$(marker first || true)
  gap=$(marker gap || true)
  if [ -n "$first" ] && [ ! -e /workspace/.echo-agent-first ]; then
    touch /workspace/.echo-agent-first
    sleep "$first"
  fi
  budget=""
  if [ -n "$each" ]; then
    budget=",\"budget_usd\":\"$each\""
  fi
  touch /workspace/kids.out
  # A parent started again (after a restart) spawns only the children it has not spawned yet.
  i=$(($(wc -l < /workspace/kids.out) + 1))
  while [ "$i" -le "$n" ]; do
    if [ "$i" -gt 1 ] && [ -n "$gap" ]; then
      sleep "$gap"
    fi
    task=$(child_input "$i")
    answer=$(mcp_call "{\"jsonrpc\":\"2.0\",\"id\":\"spawn-$i\",\"method\":\"tools/call\",\"params\":{\"name\":\"kestrel.spawn\",\"arguments\":{\"agent\":\"$agent\",\"input\":\"$task\"$budget}}}" 60 2>&1)
    printf '%s\n' "$answer" >> /workspace/fanout.out
    child=$(printf '%s\n' "$answer" | sed -n 's/.*"child":"\([0-9a-f-]*\)".*/\1/p' | head -n 1)
    if [ -z "$child" ]; then
      echo "echo-agent: spawn $i failed: $answer" >&2
      break
    fi
    echo "$child" >> /workspace/kids.out
    i=$((i + 1))
  done
  echo joining > /workspace/joining.out
  mcp_call '{"jsonrpc":"2.0","id":"join","method":"tools/call","params":{"name":"kestrel.join","arguments":{}}}' 3600 > /workspace/join.out 2>&1
fi

hold=$(marker hold || true)
if [ -n "$hold" ]; then
  # Wait for the test to write /workspace/go, so the test decides when this sandbox may end.
  echo holding > /workspace/holding.out
  waited=0
  while [ ! -e /workspace/go ] && [ "$waited" -lt "$hold" ]; do
    sleep 1
    waited=$((waited + 1))
  done
  if [ -e /workspace/go ]; then echo go > /workspace/held.out; else echo timeout > /workspace/held.out; fi
fi

nap=$(marker nap || true)
if [ -n "$nap" ] && [ ! -e /workspace/.echo-agent-napped ]; then
  touch /workspace/.echo-agent-napped
  sleep "$nap"
fi

echo "echo-agent: $input" >> /workspace/README.txt

if marker commit > /dev/null; then
  # One commit of the child's work in its own clone. The files this script writes for the tests
  # are left out, so `git status` is clean afterwards.
  (
    export HOME=/tmp
    cd /workspace || exit 1
    git config user.email agent@example.invalid
    git config user.name "echo agent"
    mkdir -p .git/info
    printf '%s\n' env.txt egress.txt '.echo-agent-*' '*.out' >> .git/info/exclude
    if marker same > /dev/null; then file=shared.txt; else file="kid-$kid.txt"; fi
    echo "work of kid $kid" > "$file"
    git add -A
    add_rc=$?
    git commit -q -m "kid $kid" > /tmp/commit.err 2>&1
    commit_rc=$?
    {
      echo "file=$file"
      echo "add_rc=$add_rc"
      echo "commit_rc=$commit_rc"
      echo "branch=$(git rev-parse --abbrev-ref HEAD 2>&1)"
      echo "status=$(git status --porcelain 2>&1 | tr '\n' ' ')"
    } > /workspace/commit.out
  )
fi

printf '{"type":"assistant","session_id":"%s","parent_tool_use_id":null,"message":{"id":"msg_e2e","type":"message","role":"assistant","model":"%s","content":[{"type":"text","text":"echo-agent edited README.txt"}],"stop_reason":"end_turn"}}\n' \
  "$session" "$model"
result_line false "done"
exit 0
