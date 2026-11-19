# A stand-in for Claude Code in headless mode, used by the M1 end-to-end tests.
# Build: docker build --build-arg UID="$(id -u)" -t kestrel-echo-agent:test tests/e2e/echo-agent
FROM alpine:3.20

RUN apk add --no-cache curl

# The agent runs as the host user that owns the workspace, so it can write the bind mount.
ARG UID=1000
RUN adduser -D -u "${UID}" agent

# The engine runs `claude <flags> -- <input>`, so the script is installed as `claude`.
COPY --chmod=0755 claude.sh /usr/local/bin/claude

USER agent
WORKDIR /workspace
CMD ["claude", "--help"]
