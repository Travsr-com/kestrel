# Kestrel UI

The web UI for Kestrel: Fleet, Run and Approvals views. A static Svelte 5 app with no server code.
`kestrel-server` (issue #37) embeds the build from `ui/dist`; when there is no build it serves a page
that says how to make one, so `cargo build` never needs Node.

## Run it against a local Kestrel

Use the Node version in `.nvmrc`.

```sh
cd ui
npm ci
npm run dev
```

Start Kestrel's API server in another terminal. The dev server proxies `/api` and `/ws` to
`http://127.0.0.1:7171`; set `KESTREL_URL` to point it elsewhere:

```sh
KESTREL_URL=http://127.0.0.1:8080 npm run dev
```

The UI asks for the API token on the first request. It keeps the token in memory only, so a new tab
asks again.

## Build for the server

```sh
npm run build   # writes ui/dist
cargo build --release -p kestrel-cli
```

## Checks

These run in CI on every PR (`ui` job). Line coverage of `src/` must stay at or above 95%.

| Command | What it checks |
| :--- | :--- |
| `npm run lint` | ESLint, with Svelte a11y warnings as errors |
| `npm run check` | Types (`svelte-check`) |
| `npm test -- --coverage` | Tests, axe accessibility checks of every view, coverage gate |
| `npm run build` then `npm run size` | Build, and the bundle is under 150 kB gzipped |
| `npm run licenses` | Every dependency license is on the allow list |

## Layout

* `src/lib/types.ts`: API JSON types. They mirror `kestrel-types` (`api.rs`); `tests/fixtures/contract.json`
  is checked against both.
* `src/lib/api.ts`, `src/lib/ws.ts`: the HTTP client and the reconnecting event socket.
* `src/lib/router.ts`: hash routes (`#/fleet`, `#/runs/<id>`, `#/approvals`).
* `src/views/`: one component per page. `src/styles/tokens.css`: design tokens.
* `tests/`: vitest with `@testing-library/svelte`; `tests/mock-server.ts` stands in for the API.
