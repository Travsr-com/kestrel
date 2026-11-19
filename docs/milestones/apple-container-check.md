# Checking the Apple `container` driver on a real Mac

The macOS sandbox driver was written from Apple's docs and source, and tested with a fake command
runner. It has never run against the real tool. This page is how to close that gap
([#11](https://github.com/Travsr-com/kestrel/issues/11)).

## What you need

* A Mac with Apple Silicon and **macOS 26 or newer**.
* Apple's [`container`](https://github.com/apple/container) installed.
* A clone of this repository.

## Run the check

```sh
./scripts/check-apple-container.sh
```

It replays the same commands the driver issues and prints one line per check, then a summary. It
only touches a temporary folder and containers named by the script, and it cleans up after itself.
It exits 0 when everything passed.

## What each check proves

| Check | Why it matters |
| :--- | :--- |
| `create` prints a single container id | The driver takes the id from the output of `create`. |
| `start --attach` streams stdout and exits 0 | The driver reads the agent's output from this process. |
| the bind mount is writable | Agents write their work to `/workspace`. |
| the `start --attach` exit code equals the container's | The Apple CLI has no `wait`, so the driver uses this exit code as the run's result. |
| `--network none` blocks outbound traffic | It is the base of the sandbox's network isolation. |
| `network create --internal` works | The gateway-only network uses it. |
| `stop` and `delete` on a missing container say "not found" | The driver treats that text as success, so cleanup can be repeated. |
| `create` offers a user flag | Needed to run the agent as your user ([#24](https://github.com/Travsr-com/kestrel/issues/24)). |

## Report the result

Paste the full output of the script into [#11](https://github.com/Travsr-com/kestrel/issues/11),
together with `container --version` and the macOS version. For every `FAIL`, the fix is one of:

1. Change the flag in `crates/kestrel-sandbox/src/lib.rs`, and update its command-line snapshot in
   `crates/kestrel-sandbox/tests/scenarios.rs` in the same commit, with the evidence in the message.
2. If the CLI cannot do it at all, note that in the issue and in `docs/quickstart.md`.

## Running it in CI

GitHub's hosted macOS runners probably cannot run `container`: it needs macOS 26 and virtualization,
and hosted Apple Silicon runners generally do not offer nested virtualization. Until there is a
self-hosted Mac runner, this stays a manual check. When one exists, add a CI job that runs this
script on it and fails when the script does.
