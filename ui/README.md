# Kestrel UI

The web UI for Kestrel: Fleet, Run and Approvals views. A static Svelte 5 app with no server code.

`kestrel up` serves it: the binary embeds `ui/dist` when it is built, and serves a page saying how
to build the UI when `ui/dist` is missing. `cargo build` never needs Node. The API is in
[docs/api.md](../docs/api.md).

## Develop

Use the Node version in `.nvmrc`.

```sh
cd ui
npm ci
npm test
```

To run the UI against a live Kestrel, start `kestrel up --no-open` in another terminal, then:

```sh
npm run dev
```

The dev server proxies `/api` and `/ws` to `http://127.0.0.1:7171`, and sends the server's own
address as `Host` and `Origin` so the server's cross-site check lets it through. Set `KESTREL_URL`
when the server is elsewhere:

```sh
KESTREL_URL=http://127.0.0.1:8080 npm run dev
```

The UI takes the API token from `?token=` in the address `kestrel up` prints, then removes it from
the address bar. Without one it asks on the first request. It keeps the token in memory only, so a
new tab asks again.

## Checks

These run in CI on every PR (`ui` job). Line coverage of `src/` must stay at or above 95%.

| Command | What it checks |
| :--- | :--- |
| `npm run lint` | ESLint, with Svelte a11y warnings as errors |
| `npm run check` | Types (`svelte-check`) |
| `npm test -- --coverage` | Tests, axe accessibility checks of every view, coverage gate |
| `npm run build` then `npm run size` | Build (to `ui/dist`), and the bundle is under 150 kB gzipped |
| `npm run licenses` | Every dependency license is on the allow list |

## Layout

* `src/lib/types.ts`: API JSON types. They mirror `kestrel-types` (`api.rs`); `tests/fixtures/contract.json`
  is checked against both.
* `src/lib/api.ts`, `src/lib/ws.ts`: the HTTP client and the reconnecting event socket.
* `src/lib/router.ts`: hash routes (`#/fleet`, `#/runs/<id>`, `#/approvals`).
* `src/views/`: one component per page. `src/styles/tokens.css`: design tokens.
* `tests/`: vitest with `@testing-library/svelte`; `tests/mock-server.ts` stands in for the API.
