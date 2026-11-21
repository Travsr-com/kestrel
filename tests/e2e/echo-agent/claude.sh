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

env > /workspace/env.txt

: > /workspace/egress.txt
for url in https://example.com https://1.1.1.1; do
  curl -s -o /dev/null --max-time 5 "$url"
  code=$?
  if [ "$code" -eq 0 ]; then
    echo "reachable $url" >> /workspace/egress.txt
  else
    echo "blocked $code $url" >> /workspace/egress.txt
  fi
done

body=$(printf '{"model":"%s","max_tokens":64,"messages":[{"role":"user","content":"echo"}]}' "$model")
if ! curl -sS -f --max-time 30 -o /tmp/response.json \
  -H "x-api-key: ${ANTHROPIC_API_KEY:-}" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d "$body" "${ANTHROPIC_BASE_URL:-}/v1/messages" 2> /tmp/curl.err; then
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

echo "echo-agent: $input" >> /workspace/README.txt

printf '{"type":"assistant","session_id":"%s","parent_tool_use_id":null,"message":{"id":"msg_e2e","type":"message","role":"assistant","model":"%s","content":[{"type":"text","text":"echo-agent edited README.txt"}],"stop_reason":"end_turn"}}\n' \
  "$session" "$model"
result_line false "done"
exit 0
