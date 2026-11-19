#!/bin/sh
# Behaves like `claude --bare -p --output-format stream-json --verbose --model X
# --permission-mode bypassPermissions -- <input>`, just enough for Kestrel's Claude Code adapter.
#
# It makes one model call through the gateway, records what the tests check in /workspace,
# and prints a system/init line, an assistant line and a final result line.
#
# Files written to /workspace:
#   env.txt      the environment (scenario 4: no real provider key)
#   egress.txt   one line per outside host tried, "blocked ..." or "reachable ..." (scenario 3)
#   README.txt   one line appended, so a tracked file changes (scenario 1)
#
# If the input contains `sleep=N`, it sleeps N seconds after the model call, once per
# workspace (a marker file remembers it), so a test can act while the run is in flight.
set -u

model="unknown"
while [ $# -gt 0 ]; do
  case "$1" in
    --model) model="${2:-unknown}"; shift 2 ;;
    --output-format | --permission-mode) shift 2 ;;
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
  if curl -s -o /dev/null --max-time 5 "$url"; then
    echo "reachable $url" >> /workspace/egress.txt
  else
    echo "blocked $url (curl exit $?)" >> /workspace/egress.txt
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

echo "echo-agent: $input" >> /workspace/README.txt

printf '{"type":"assistant","session_id":"%s","parent_tool_use_id":null,"message":{"id":"msg_e2e","type":"message","role":"assistant","model":"%s","content":[{"type":"text","text":"echo-agent edited README.txt"}],"stop_reason":"end_turn"}}\n' \
  "$session" "$model"
result_line false "done"
exit 0
