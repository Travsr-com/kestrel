// Issue #84 (found while testing it against a real server): Fleet shows runs started after it
// loaded, live, without a reload. The app's `/ws/events` socket hears of each new run; Fleet fetches
// one it does not know and follows its tree like any other.

import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
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
    takes_messages: true,
    created_at: new Date(NOW - 1_000).toISOString(),
    ...over,
  };
}

/** Calls `read` once the body of `res` has been read as JSON. */
function watched(res: Response, read: () => void): Response {
  const json = res.json.bind(res);
  res.json = async () => {
    const body: unknown = await json();
    read();
    return body;
  };
  return res;
}

interface Options {
  /** Run look-ups (paths) that fail once with a 503. */
  failOnce?: string[];
  /** Run look-ups (paths) whose answer waits until `release` is called. */
  hold?: string[];
}

/** A server whose run list is `listed` (what existed when Fleet loaded) and that knows `all` by id. */
function serve(listed: RunSummary[], all: RunSummary[], { failOnce = [], hold = [] }: Options = {}) {
  const gets: string[] = [];
  /** Live-run lists read by the app (one per live status per sync), and failed look-ups read. */
  let lists = 0;
  let failures = 0;
  let release: () => void = () => {};
  const held = new Promise<void>((done) => (release = done));
  const fetch: typeof globalThis.fetch = async (input) => {
    const url = new URL(String(input), 'http://kestrel.test');
    if (url.pathname === '/api/approvals') return new Response('[]');
    if (url.pathname === '/api/runs') {
      const status = url.searchParams.get('status');
      const live = (r: RunSummary) => typeof r.status === 'string' && r.status.toLowerCase() === status;
      if (status === null) return new Response(JSON.stringify(listed));
      return watched(new Response(JSON.stringify(listed.filter(live))), () => (lists += 1));
    }
    gets.push(url.pathname);
    if (hold.includes(url.pathname)) await held;
    const one = all.find((r) => url.pathname === `/api/runs/${r.id}`);
    const fail = failOnce.indexOf(url.pathname);
    if (fail >= 0) {
      failOnce.splice(fail, 1);
      return watched(new Response('{"error":"busy"}', { status: 503 }), () => (failures += 1));
    }
    return one ? new Response(JSON.stringify(one)) : new Response('{"error":"no run"}', { status: 404 });
  };
  return { fetch, gets, lists: () => lists, failures: () => failures, release: () => release() };
}

function show(listed: RunSummary[], all: RunSummary[], options: Options = {}) {
  const served = serve(listed, all, options);
  const sockets = fakeSockets();
  window.location.hash = '#/fleet';
  const result = render(App, { fetch: served.fetch, sockets: sockets.factory });
  return { ...served, sockets, unmount: result.unmount };
}

/** Lets every promise already settled run its callbacks (each step yields to the event loop). */
async function flush() {
  for (let i = 0; i < 5; i += 1) await vi.advanceTimersByTimeAsync(0);
}

/** The live statuses the app lists, one request each per sync. */
const LIVE_STATUSES = 4;

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

describe('Fleet look-ups that cannot succeed, and look-ups in flight', () => {
  it('does not ask again for a run the server says it does not have', async () => {
    const old = run({ agent: 'old' });
    const { sockets, gets } = show([old], [old]);
    await vi.waitFor(() => expect(agents()).toEqual(['old']));
    const s = feed(sockets);
    s.open();
    s.event(1, ev('gone-run', { kind: 'run_created', agent: 'ghost' }));
    await vi.waitFor(() => expect(gets.filter((p) => p === '/api/runs/gone-run')).toHaveLength(1));
    await vi.advanceTimersByTimeAsync(180_000);
    expect(gets.filter((p) => p === '/api/runs/gone-run')).toHaveLength(1);
  });

  it('stops asking for a run that failed to load once it is no longer live', async () => {
    const old = run({ agent: 'old' });
    const brief = run({ agent: 'brief' });
    const path = `/api/runs/${brief.id}`;
    const { sockets, gets, failures } = show([old], [old, brief], { failOnce: [path] });
    await vi.waitFor(() => expect(agents()).toEqual(['old']));
    const s = feed(sockets);
    s.open();
    s.event(1, ev(brief.id, { kind: 'run_created', agent: 'brief' }));
    await vi.waitFor(() => expect(failures()).toBe(1));
    await flush();
    s.event(2, ev(brief.id, { kind: 'status_changed', from: 'Running', to: 'Completed' }));
    await flush();
    await vi.advanceTimersByTimeAsync(180_000);
    expect(gets.filter((p) => p === path)).toHaveLength(1);
  });

  it('asks once for a run whose look-up is still out when another new run arrives', async () => {
    const old = run({ agent: 'old' });
    const first = run({ agent: 'first' });
    const second = run({ agent: 'second' });
    const path = `/api/runs/${first.id}`;
    const { sockets, gets, release } = show([old], [old, first, second], { hold: [path] });
    await vi.waitFor(() => expect(agents()).toEqual(['old']));
    const s = feed(sockets);
    s.open();
    s.event(1, ev(first.id, { kind: 'run_created', agent: 'first' }));
    await vi.waitFor(() => expect(gets.filter((p) => p === path)).toHaveLength(1));
    s.event(2, ev(second.id, { kind: 'run_created', agent: 'second' }));
    await vi.waitFor(() => expect(agents()).toContain('second'));
    release();
    await vi.waitFor(() => expect(agents()).toContain('first'));
    expect(gets.filter((p) => p === path)).toHaveLength(1);
  });
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

  it('does not look up a run its tree socket already brought', async () => {
    const old = run({ agent: 'old' });
    const kid = run({ agent: 'kid', parent: old.id, root: old.id, depth: 1 });
    const { sockets, gets } = show([old], [old, kid]);
    await vi.waitFor(() => expect(agents()).toEqual(['old']));
    const tree = treeOf(sockets, old.id);
    tree.open();
    tree.event(1, ev(kid.id, { kind: 'run_created', agent: 'kid', parent: old.id }, 1));

    const s = feed(sockets);
    s.open();
    s.event(1, ev(kid.id, { kind: 'run_created', agent: 'kid', parent: old.id }));
    await flush();
    await vi.advanceTimersByTimeAsync(6_000);
    // `kid` came on its tree socket: fetched once for its limit, as before, and not looked up again.
    expect(gets.filter((p) => p === `/api/runs/${kid.id}`)).toHaveLength(1);
  });

  it('asks again by itself after a failed look-up, with no further socket events (PR #85 review)', async () => {
    const old = run({ agent: 'old' });
    const fresh = run({ agent: 'fresh', created_at: new Date(NOW).toISOString() });
    const server = show([old], [old, fresh], { failOnce: [`/api/runs/${fresh.id}`] });
    await vi.waitFor(() => expect(agents()).toEqual(['old']));
    const s = feed(server.sockets);
    s.open();
    // The sync the opening starts changes the live runs too: it settles first, so it cannot be what
    // asks again.
    await vi.waitFor(() => expect(server.lists()).toBe(2 * LIVE_STATUSES));
    await flush();

    s.event(1, ev(fresh.id, { kind: 'run_created', agent: 'fresh' }));
    await vi.waitFor(() => expect(server.failures()).toBe(1));
    await flush();
    const lookUps = () => server.gets.filter((p) => p === `/api/runs/${fresh.id}`);
    expect(lookUps()).toHaveLength(1);
    expect(agents()).toEqual(['old']);

    // No socket event from here on: only time passes.
    await vi.advanceTimersByTimeAsync(6_000);
    await vi.waitFor(() => expect(agents()).toEqual(['fresh', 'old']));
    expect(lookUps()).toHaveLength(2);
  });

  it('asks nothing more after Fleet is gone, even after a failed look-up (PR #85 review)', async () => {
    const old = run({ agent: 'old' });
    const fresh = run({ agent: 'fresh' });
    const server = show([old], [old, fresh], { failOnce: [`/api/runs/${fresh.id}`] });
    await vi.waitFor(() => expect(agents()).toEqual(['old']));
    const s = feed(server.sockets);
    s.open();
    s.event(1, ev(fresh.id, { kind: 'run_created', agent: 'fresh' }));
    await vi.waitFor(() => expect(server.failures()).toBe(1));
    await flush();
    server.unmount();
    await vi.advanceTimersByTimeAsync(130_000);
    expect(server.gets.filter((p) => p === `/api/runs/${fresh.id}`)).toHaveLength(1);
  });

  it('fetches a new child once when the events socket brings it before its tree socket (PR #85 review)', async () => {
    const old = run({ agent: 'old' });
    const kid = run({ agent: 'kid', parent: old.id, root: old.id, depth: 1, limit: 700_000 });
    const server = show([old], [old, kid], { hold: [`/api/runs/${kid.id}`] });
    await vi.waitFor(() => expect(agents()).toEqual(['old']));
    const tree = treeOf(server.sockets, old.id);
    tree.open();
    const s = feed(server.sockets);
    s.open();
    s.event(1, ev(kid.id, { kind: 'run_created', agent: 'kid', parent: old.id }));
    const fetches = () => server.gets.filter((p) => p === `/api/runs/${kid.id}`);
    // The look-up is on its way when the tree socket brings the child too.
    await vi.waitFor(() => expect(fetches()).toHaveLength(1));
    tree.event(1, ev(kid.id, { kind: 'run_created', agent: 'kid', parent: old.id }, 1));
    await flush();
    server.release();
    await flush();
    await vi.advanceTimersByTimeAsync(6_000);
    expect(fetches()).toHaveLength(1);
    // The one answer filled the child in.
    await userEvent.setup({ advanceTimers: vi.advanceTimersByTime }).click(screen.getByRole('checkbox', { name: 'Roots only' }));
    const card = screen.queryAllByRole('article').find((a) => a.textContent?.includes('kid'));
    expect(card).toHaveTextContent('$0.70');
  });
});

function treeOf(sockets: ReturnType<typeof fakeSockets>, root: string): FakeSocket {
  const found = sockets.made.find((x) => x.url.includes(`/ws/tree/${root}`));
  if (!found) throw new Error(`no tree socket for ${root}`);
  return found;
}
