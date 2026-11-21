// An in-memory stand-in for the `kestrel-server` HTTP API, served through a `fetch` function.
// It answers from the shared contract fixture, so the UI is tested against the same JSON the
// Rust types round-trip.

import contract from './fixtures/contract.json';

export interface Call {
  method: string;
  path: string;
  auth: string | null;
  body: unknown;
}

export interface MockServer {
  fetch: typeof fetch;
  calls: Call[];
  /** The token the server accepts. `null` accepts any request, with or without a token. */
  token: string | null;
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  });
}

export function createMockServer(token: string | null = null): MockServer {
  const server: MockServer = {
    calls: [],
    token,
    fetch: async (input, init) => {
      const url = new URL(String(input), 'http://kestrel.test');
      const method = init?.method ?? 'GET';
      const headers = new Headers(init?.headers);
      const auth = headers.get('authorization');
      const body = typeof init?.body === 'string' ? JSON.parse(init.body) : null;
      server.calls.push({ method, path: url.pathname + url.search, auth, body });

      if (server.token !== null && auth !== `Bearer ${server.token}`) {
        return json(401, contract.error);
      }

      const parts = url.pathname.split('/').filter(Boolean);
      if (parts[0] !== 'api') return json(404, { error: 'not found' });

      if (method === 'GET' && parts[1] === 'runs' && parts.length === 2) {
        const status = url.searchParams.get('status');
        const runs = contract.runs.filter(
          (r) => status === null || (typeof r.status === 'string' && r.status.toLowerCase() === status),
        );
        return json(200, runs);
      }
      if (parts[1] === 'runs' && parts.length >= 3) {
        const run = contract.runs.find((r) => r.id === parts[2]);
        if (!run) return json(404, { error: 'run not found' });
        if (method === 'GET' && parts.length === 3) return json(200, run);
        if (method === 'GET' && parts[3] === 'tree') return json(200, contract.tree);
        if (method === 'GET' && parts[3] === 'events') {
          const after = Number(url.searchParams.get('after') ?? '0');
          return json(200, contract.events.filter((e) => e.seq > after));
        }
        // #37 answers a command with 202 and no body.
        if (method === 'POST') return new Response(null, { status: 202 });
      }
      if (method === 'GET' && parts[1] === 'approvals' && parts.length === 2) {
        const status = url.searchParams.get('status');
        return json(200, contract.approvals.filter((a) => status === null || a.status === status));
      }
      if (method === 'POST' && parts[1] === 'approvals' && parts.length === 4) {
        const approval = contract.approvals.find((a) => a.id === parts[2]);
        if (!approval) return json(404, { error: 'approval not found' });
        if (approval.status !== 'pending') return json(409, { error: 'already resolved' });
        return new Response(null, { status: 202 });
      }
      return json(404, { error: 'not found' });
    },
  };
  return server;
}
