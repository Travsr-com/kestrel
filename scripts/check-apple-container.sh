#!/usr/bin/env bash
# Replays the command lines Kestrel's Apple `container` driver issues against the real CLI, and
# says which ones work. Run it on a Mac (macOS 26 or newer, Apple Silicon) with `container`
# installed. See docs/milestones/apple-container-check.md.
#
# It changes nothing outside a temporary folder and containers/networks named kestrel-check-*.
set -u

pass=0
fail=0
report() { # report <PASS|FAIL> <name> [detail]
  if [ "$1" = PASS ]; then pass=$((pass + 1)); else fail=$((fail + 1)); fi
  printf '%-5s %s%s\n' "$1" "$2" "${3:+  ($3)}"
}

if [ "$(uname -s)" != Darwin ]; then
  echo "This check needs macOS." >&2
  exit 2
fi
if ! command -v container >/dev/null 2>&1; then
  echo "Apple 'container' is not installed: https://github.com/apple/container" >&2
  exit 2
fi
echo "macOS $(sw_vers -productVersion) on $(uname -m), $(container --version 2>&1 | head -1)"
container system start >/dev/null 2>&1 || true

work="$(mktemp -d)"
ws="$work/ws"
mkdir -p "$ws"
image="alpine:3.20"
ids=()
cleanup() {
  for id in "${ids[@]:-}"; do
    [ -n "$id" ] && container delete --force "$id" >/dev/null 2>&1
  done
  container network delete kestrel-check-net >/dev/null 2>&1
  rm -rf "$work"
}
trap cleanup EXIT

# The same flags the driver builds (crates/kestrel-sandbox/src/lib.rs, create_argv).
create() { # create <network flag...> -- <command...>
  local net=()
  while [ "$1" != -- ]; do net+=("$1"); shift; done
  shift
  container create --cpus 2 --memory 512M "${net[@]}" \
    --mount "type=bind,source=$ws,target=/workspace" --workdir /workspace \
    --env K=V "$image" "$@"
}

# 1. create prints a container id on stdout.
id="$(create --network none -- sh -c 'echo hi > /workspace/out.txt; echo hi' 2>"$work/err")"
if [ -n "$id" ] && [ "$(printf '%s' "$id" | wc -l)" -le 1 ]; then
  ids+=("$id")
  report PASS "create prints a single container id" "$id"
else
  report FAIL "create prints a single container id" "$(head -c 300 "$work/err")"
  echo "Cannot continue without a container." >&2
  echo "Summary: $pass passed, $fail failed"
  exit 1
fi

# 2. start --attach streams output and the process exits 0.
out="$(container start --attach "$id" 2>"$work/err")"
code=$?
if [ "$code" -eq 0 ] && printf '%s' "$out" | grep -q '^hi$'; then
  report PASS "start --attach streams stdout and exits 0"
else
  report FAIL "start --attach streams stdout and exits 0" "exit $code, $(head -c 200 "$work/err")"
fi

# 3. the bind mount is writable and the file lands on the host.
if [ -f "$ws/out.txt" ] && [ "$(cat "$ws/out.txt")" = hi ]; then
  report PASS "bind mount at /workspace is writable from the container"
else
  report FAIL "bind mount at /workspace is writable from the container"
fi

# 4. the exit code of `start --attach` is the container's exit code (the driver's wait relies on it).
id7="$(create --network none -- sh -c 'exit 7' 2>/dev/null)"
ids+=("$id7")
container start --attach "$id7" >/dev/null 2>&1
code=$?
if [ "$code" -eq 7 ]; then
  report PASS "start --attach exit code equals the container exit code"
else
  report FAIL "start --attach exit code equals the container exit code" "got $code, wanted 7"
fi

# 5. --network none really blocks the network.
idn="$(create --network none -- sh -c 'wget -q -T 3 -O- http://example.com >/dev/null 2>&1; echo "wget=$?"' 2>/dev/null)"
ids+=("$idn")
out="$(container start --attach "$idn" 2>/dev/null)"
case "$out" in
  *wget=0*) report FAIL "--network none blocks outbound traffic" "wget succeeded" ;;
  *wget=*) report PASS "--network none blocks outbound traffic" ;;
  *) report FAIL "--network none blocks outbound traffic" "no result: $out" ;;
esac

# 6. an internal network can be created (GatewayOnly uses `network create --internal`).
if container network create --internal kestrel-check-net >/dev/null 2>"$work/err"; then
  report PASS "network create --internal works"
else
  report FAIL "network create --internal works" "$(head -c 200 "$work/err")"
fi

# 7. stop and delete on an already-gone container: record the real error text. The driver treats
#    any stderr containing "not found" as success, so the text must contain it.
container delete --force "$id" >/dev/null 2>&1
stop_err="$(container stop "$id" 2>&1 >/dev/null)"
delete_err="$(container delete --force "$id" 2>&1 >/dev/null)"
for what in "stop:$stop_err" "delete:$delete_err"; do
  name="${what%%:*}"
  text="${what#*:}"
  if printf '%s' "$text" | grep -qi 'not found'; then
    report PASS "$name on a missing container says 'not found'" "$text"
  else
    report FAIL "$name on a missing container says 'not found'" "got: ${text:-no error at all}"
  fi
done

# 8. information for #24 (run as the host user): does create offer a user flag?
if container create --help 2>&1 | grep -Eq -- '--(user|uid)\b'; then
  report PASS "create offers a user flag" "$(container create --help 2>&1 | grep -E -- '--(user|uid|gid)\b' | head -3 | tr -s ' ' | tr '\n' ';')"
else
  report FAIL "create offers a user flag" "driver cannot run as the host user here (issue #24)"
fi

echo
echo "Summary: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
