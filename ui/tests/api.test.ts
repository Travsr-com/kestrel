// The typed API client against the mock server: paths, bearer token, and error mapping.

import { describe, expect, it } from 'vitest';
import contract from './fixtures/contract.json';
import { ApiFailure, Unauthorized, createApi } from '../src/lib/api';
import { createMockServer } from './mock-server';

const ROOT = contract.runs[0]!.id;
const PENDING = contract.approvals[0]!.id;
const APPROVED = contract.approvals[1]!.id;

function client(token: string | null = 'secret', serverToken: string | null = 'secret') {
  const server = createMockServer(serverToken);
  const api = createApi({ fetch: server.fetch, getToken: () => token });
  return { server, api };
}

describe('createApi', () => {
  it('sends the token as a bearer header', async () => {
    const { server, api } = client();
    await api.listRuns();
    expect(server.calls[0]).toMatchObject({ method: 'GET', path: '/api/runs', auth: 'Bearer secret' });
  });

  it('sends no authorization header without a token', async () => {
    const { server, api } = client(null, null);
    await api.listRuns();
    expect(server.calls[0]?.auth).toBeNull();
  });

  it('lists runs with filters as query parameters', async () => {
    const { server, api } = client();
    const runs = await api.listRuns({ status: 'running', agent: 'planner', root: ROOT });
    expect(runs.map((r) => r.id)).toEqual([ROOT]);
    expect(server.calls[0]?.path).toBe(`/api/runs?status=running&agent=planner&root=${ROOT}`);
  });

  it('reads one run, its tree and its events', async () => {
    const { server, api } = client();
    expect((await api.getRun(ROOT)).agent).toBe('planner');
    expect(await api.tree(ROOT)).toHaveLength(2);
    expect((await api.events(ROOT, 1)).map((e) => e.seq)).toEqual([2, 3]);
    expect(server.calls.map((c) => c.path)).toEqual([
      `/api/runs/${ROOT}`,
      `/api/runs/${ROOT}/tree`,
      `/api/runs/${ROOT}/events?after=1`,
    ]);
  });

  it('escapes ids in paths', async () => {
    const { server, api } = client();
    await expect(api.getRun('a/b')).rejects.toBeInstanceOf(ApiFailure);
    expect(server.calls[0]?.path).toBe('/api/runs/a%2Fb');
  });

  it('lists approvals by status', async () => {
    const { server, api } = client();
    const pending = await api.approvals('pending');
    expect(pending.map((a) => a.id)).toEqual([PENDING]);
    expect(server.calls[0]?.path).toBe('/api/approvals?status=pending');
  });

  it('lists every approval, of any status, without a status (PR #85 review)', async () => {
    const { server, api } = client();
    const all = await api.approvals();
    expect(all.map((a) => a.id)).toEqual(contract.approvals.map((a) => a.id));
    expect(all.map((a) => a.id)).toContain(APPROVED);
    expect(server.calls[0]?.path).toBe('/api/approvals');
  });

  it('posts run commands', async () => {
    const { server, api } = client();
    await api.pause(ROOT);
    await api.resume(ROOT);
    await api.cancel(ROOT);
    await api.setBudget(ROOT, '5.00');
    await api.message(ROOT, 'try again');
    expect(server.calls.map((c) => [c.method, c.path, c.body])).toEqual([
      ['POST', `/api/runs/${ROOT}/pause`, null],
      ['POST', `/api/runs/${ROOT}/resume`, null],
      ['POST', `/api/runs/${ROOT}/cancel`, null],
      ['POST', `/api/runs/${ROOT}/budget`, { usd: '5.00' }],
      ['POST', `/api/runs/${ROOT}/message`, { text: 'try again' }],
    ]);
  });

  it('posts approve and reject with optional text', async () => {
    const { server, api } = client();
    await api.approve(PENDING);
    await api.approve(PENDING, 'git push origin main');
    await api.reject(PENDING);
    await api.reject(PENDING, 'too risky');
    expect(server.calls.map((c) => c.body)).toEqual([
      null,
      { payload: 'git push origin main' },
      null,
      { reason: 'too risky' },
    ]);
    expect(server.calls[2]?.path).toBe(`/api/approvals/${PENDING}/reject`);
  });

  it('throws Unauthorized on 401, naming the token the request carried', async () => {
    const { api } = client('wrong');
    const err = await api.listRuns().catch((e: unknown) => e);
    expect(err).toBeInstanceOf(Unauthorized);
    expect(err).toMatchObject({ token: 'wrong' });
    const anonymous = await client(null).api.listRuns().catch((e: unknown) => e);
    expect(anonymous).toMatchObject({ token: null });
  });

  it.each([202, 204])('accepts a command answered %i with an empty body', async (status) => {
    const api = createApi({ fetch: async () => new Response(null, { status }), getToken: () => null });
    await expect(api.cancel(ROOT)).resolves.toBeUndefined();
    await expect(api.approve(PENDING, 'x')).resolves.toBeUndefined();
  });

  it('throws ApiFailure with the status and server message otherwise', async () => {
    const { api } = client();
    const missing = await api.getRun('nope').catch((e: unknown) => e);
    expect(missing).toBeInstanceOf(ApiFailure);
    expect(missing).toMatchObject({ status: 404, message: 'run not found' });
    const conflict = await api.approve(APPROVED).catch((e: unknown) => e);
    expect(conflict).toMatchObject({ status: 409, message: 'already resolved' });
  });

  it('falls back to the status text when the error body is not JSON', async () => {
    const api = createApi({
      fetch: async () => new Response('gateway exploded', { status: 502, statusText: 'Bad Gateway' }),
      getToken: () => null,
    });
    await expect(api.listRuns()).rejects.toMatchObject({ status: 502, message: 'HTTP 502 Bad Gateway' });
  });

  it('uses the base URL when one is given', async () => {
    const server = createMockServer();
    const api = createApi({ fetch: server.fetch, getToken: () => null, baseUrl: 'http://kestrel.test' });
    await api.listRuns();
    expect(server.calls[0]?.path).toBe('/api/runs');
  });
});
