# A stand-in for Claude Code in headless mode, used by the M1 end-to-end tests.
# Build: docker build -t kestrel-echo-agent:test tests/e2e/echo-agent
FROM alpine:3.20

# git: scenario 6 has the agent commit inside its clone, like a real coding agent.
RUN apk add --no-cache curl git

# A fixed uid that is NOT the CI runner's (1001). Kestrel runs the sandbox as the workspace owner
# (`--user`), so the image must not depend on its own uid for the bind mount to be writable.
RUN adduser -D -u 1000 agent

# The engine runs `claude <flags> -- <input>`, so the script is installed as `claude`.
COPY --chmod=0755 claude.sh /usr/local/bin/claude

USER agent
WORKDIR /workspace
CMD ["claude", "--help"]
