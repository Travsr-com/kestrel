# Kestrel

Run fleets of AI agents on your own machine or server: headless, sandboxed, parallel, and cheaper.

> Status: early development (milestone M1). Not ready for use yet.

## What it does

* **Runs agents headless** inside sandboxes (gVisor on Linux, Apple `container` on macOS).
* **Runs them in parallel.** Agents can spawn sub-agents, each with a slice of the parent's budget and permissions.
* **Cuts model cost.** All model traffic goes through one gateway that meters, caches and routes it.
* **Shows everything live** in a web UI and the `kestrel` CLI.

## Learn more

* Architecture and roadmap: [`docs/architecture.md`](docs/architecture.md)
* Contributor rules: [`.claude/CLAUDE.md`](.claude/CLAUDE.md)

## License

Apache-2.0. See [`LICENSE`](LICENSE).
