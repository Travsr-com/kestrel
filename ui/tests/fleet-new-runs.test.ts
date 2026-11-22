// Issue #84 (found while testing it against a real server): Fleet shows runs started after it
// loaded, live, without a reload. The app's `/ws/events` socket hears of each new run; Fleet fetches
// one it does not know and follows its tree like any other.

import { render, screen, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App.svelte';
import type { Event, RunSummary } from '../src/lib/types';
import { fakeSockets, type FakeSocket } from './fake-socket';

const NOW = Date.parse('2026-11-22T10:00:00Z');

let n = 0;
const uuid = () => `00000000-0000-4000-8000-${String(++n).padStart(12, '0')}`;

function run(over: Partial<RunSummary> = {}): RunSummary {
  const id = over.id ?? uuid();
  return {
    id,
    agent: 'worker',
    status: 'Running',
    parent: null,
    root: id,
    depth: 0,
    spent: 0,
    limit: 2_000_000,
    sandbox_tier: 'gvisor',
    created_at: new Date(NOW - 1_000).toISOString(),
    ...over,
  };
}

/** A server whose run list is `listed` (what existed when Fleet loaded) and that knows `all` by id. */
function serve(listed: RunSummary[], all: RunSummary[], failOnce: string[] = []) {
  const gets: string[] = [];
  const fetch: typeof globalThis.fetch = async (input) => {
    const url = new URL(String(input), 'http://kestrel.test');
    if (url.pathname === '/api/approvals') return new Response('[]');
    if (url.pathname === '/api/runs') {
      const status = url.searchParams.get('status');
      const live = (r: RunSummary) => typeof r.status === 'string' && r.status.toLowerCase() === status;
      return new Response(JSON.stringify(status === null ? listed : listed.filter(live)));
    }
    gets.push(url.pathname);
    const one = all.find((r) => url.pathname === `/api/runs/${r.id}`);
    const fail = failOnce.indexOf(url.pathname);
    if (fail >= 0) {
      failOnce.splice(fail, 1);
      return new Response('{"error":"busy"}', { status: 503 });
    }
    return one ? new Response(JSON.stringify(one)) : new Response('{"error":"no run"}', { status: 404 });
  };
  return { fetch, gets };
}

function show(listed: RunSummary[], all: RunSummary[], failOnce: string[] = []) {
  const served = serve(listed, all, failOnce);
  const sockets = fakeSockets();
  window.location.hash = '#/fleet';
  render(App, { fetch: served.fetch, sockets: sockets.factory });
  return { sockets, gets: served.gets };
}

function feed(sockets: ReturnType<typeof fakeSockets>): FakeSocket {
  const found = sockets.made.filter((s) => s.url.includes('/ws/events')).at(-1);
  if (!found) throw new Error('no events socket');
  return found;
}

const ev = (runId: string, kind: Event['kind'], seq = 1): Event => ({
  run_id: runId,
  seq,
  ts: new Date(NOW).toISOString(),
  kind,
});

const agents = () => screen.queryAllByRole('article').map((a) => within(a).getByRole('heading').textContent?.trim());

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
});

describe('Fleet and runs started after it loaded', () => {
  it('shows a run started after the page loaded, and follows it live', async () => {
    const old = run({ agent: 'old' });
    const fresh = run({ agent: 'fresh', status: 'Provisioning', created_at: new Date(NOW).toISOString() });
    const { sockets } = show([old], [old, fresh]);
    await vi.waitFor(() => expect(agents()).toEqual(['old']));

    const s = feed(sockets);
    s.open();
    s.event(1, ev(fresh.id, { kind: 'run_created', agent: 'fresh' }));

    await vi.waitFor(() => expect(agents()).toEqual(['fresh', 'old']));
    // Its tree is followed like the others, so its status moves live.
    const tree = await vi.waitFor(() => {
      const found = sockets.made.find((x) => x.url.includes(`/ws/tree/${fresh.id}`));
      if (!found) throw new Error('no tree socket for the new run');
      return found;
    });
    tree.open();
    tree.event(1, ev(fresh.id, { kind: 'status_changed', from: 'Provisioning', to: 'Running' }, 1));
    await vi.waitFor(() => {
      const card = screen.queryAllByRole('article').find((a) => a.textContent?.includes('fresh'));
      expect(card).toHaveTextContent('Running');
    });
  });

  it('a run that already ended when it is looked up shows under Finished, not in the active view', async () => {
    const old = run({ agent: 'old' });
    const quick = run({ agent: 'quick', status: 'Completed' });
    const { sockets } = show([old], [old, quick]);
    await vi.waitFor(() => expect(agents()).toEqual(['old']));
    const s = feed(sockets);
    s.open();
    s.event(1, ev(quick.id, { kind: 'run_created', agent: 'quick' }));
    // The finished count goes up once the run was looked up.
    await vi.waitFor(() => expect(screen.getByRole('button', { name: /^Finished/ })).toHaveTextContent('Finished 1'));
    expect(agents()).toEqual(['old']);
  });

  it('does not look up a run its tree socket already brought, and asks again after a failed look-up', async () => {
    const old = run({ agent: 'old' });
    const kid = run({ agent: 'kid', parent: old.id, root: old.id, depth: 1 });
    const fresh = run({ agent: 'fresh' });
    const { sockets, gets } = show([old], [old, kid, fresh], [`/api/runs/${fresh.id}`]);
    await vi.waitFor(() => expect(agents()).toEqual(['old']));
    const tree = sockets.made.find((x) => x.url.includes(`/ws/tree/${old.id}`));
    if (!tree) throw new Error('no tree socket for old');
    tree.open();
    tree.event(1, ev(kid.id, { kind: 'run_created', agent: 'kid', parent: old.id }, 1));

    const s = feed(sockets);
    s.open();
    s.event(1, ev(kid.id, { kind: 'run_created', agent: 'kid', parent: old.id }));
    s.event(2, ev(fresh.id, { kind: 'run_created', agent: 'fresh' }));
    // The first look-up of `fresh` fails; the next change to the live runs (`another`) asks again.
    s.event(3, ev('another', { kind: 'run_created', agent: 'other' }));
    await vi.waitFor(() => expect(agents()).toContain('fresh'));
    expect(gets.filter((p) => p === `/api/runs/${fresh.id}`)).toHaveLength(2);
    // `kid` came on its tree socket: fetched once for its limit, as before, and not looked up again.
    expect(gets.filter((p) => p === `/api/runs/${kid.id}`)).toHaveLength(1);
  });
});
