// Issue #39: the Fleet view. Component tests against a fake API and fake tree sockets.

import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import Fleet from '../src/views/Fleet.svelte';
import { createApi } from '../src/lib/api';
import { FleetState } from '../src/lib/fleet.svelte';
import type { Event, RunStatus, RunSummary } from '../src/lib/types';
import { fakeSockets } from './fake-socket';
import { ids, seriousViolations } from './axe';

const NOW = Date.parse('2026-11-21T10:00:00Z');

let seq = 0;
function run(over: Partial<RunSummary> = {}): RunSummary {
  seq += 1;
  const id = over.id ?? `00000000-0000-4000-8000-${String(seq).padStart(12, '0')}`;
  return {
    id,
    agent: 'worker',
    status: 'Running',
    parent: null,
    root: id,
    depth: 0,
    spent: 420_000,
    limit: 2_000_000,
    sandbox_tier: 'gvisor',
    takes_messages: true,
    created_at: new Date(NOW - 125_000).toISOString(),
    ...over,
  };
}

/** A fake API answering `/api/runs` and `/api/runs/{id}` from `runs`, or failing. */
function serve(runs: RunSummary[], fail = { list: 0 }) {
  const calls: string[] = [];
  const fetch: typeof globalThis.fetch = async (input) => {
    const url = new URL(String(input), 'http://kestrel.test');
    calls.push(url.pathname);
    if (url.pathname === '/api/runs') {
      if (fail.list > 0) {
        fail.list -= 1;
        return new Response(JSON.stringify({ error: 'database is locked' }), { status: 500 });
      }
      return new Response(JSON.stringify(runs));
    }
    const one = runs.find((r) => url.pathname === `/api/runs/${r.id}`);
    return one
      ? new Response(JSON.stringify(one))
      : new Response(JSON.stringify({ error: 'no run' }), { status: 404 });
  };
  return { api: createApi({ fetch, getToken: () => null }), calls };
}

function show(runs: RunSummary[], fail = { list: 0 }) {
  const { api, calls } = serve(runs, fail);
  const sockets = fakeSockets();
  const onUnauthorized = vi.fn();
  const result = render(Fleet, {
    api,
    onUnauthorized,
    sockets: sockets.factory,
    base: 'ws://kestrel.test',
  });
  return { ...result, sockets, calls, onUnauthorized };
}

function cost(runId: string, micro: number): Event['kind'] {
  return {
    kind: 'model_call', run_id: runId, provider: 'p', model: 'm', tokens_in: 1, tokens_out: 1,
    tokens_cache_read: 0, tokens_cache_write: 0, cost: micro, latency_ms: 1, status: 200,
  };
}

function socketFor(sockets: ReturnType<typeof fakeSockets>, id: string) {
  const found = sockets.made.find((s) => s.url.includes(id));
  if (!found) throw new Error(`no socket for ${id}`);
  return found;
}

function event(runId: string, kind: Event['kind'], s = 1): Event {
  return { run_id: runId, seq: s, ts: new Date(NOW).toISOString(), kind };
}

function cards(): HTMLElement[] {
  return screen.queryAllByRole('article');
}

function card(agent: string): HTMLElement {
  const found = cards().find((c) => within(c).queryByRole('link', { name: agent }));
  if (!found) throw new Error(`no card for ${agent}`);
  return found;
}

/** The filter chip named `name`, whatever its count. */
function chip(name: string): HTMLElement {
  return screen.getByRole('button', { name: new RegExp(`^${name} \\d+$`) });
}

const searchBox = () => screen.getByRole('searchbox', { name: 'Search runs and agents' });

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
});

const user = () => userEvent.setup({ advanceTimers: vi.advanceTimersByTime });

describe('Fleet', () => {
  // 1
  it('shows each run as a card: agent, status text, cost over limit, elapsed', async () => {
    show([run({ agent: 'alpha' }), run({ agent: 'beta' }), run({ agent: 'gamma' })]);
    await screen.findByRole('link', { name: 'alpha' });
    expect(cards()).toHaveLength(3);
    const alpha = card('alpha');
    expect(alpha).toHaveTextContent('Running');
    expect(alpha).toHaveTextContent('$0.42 of $2.00');
    expect(alpha).toHaveTextContent('2m 05s');
    expect(alpha).toHaveTextContent('gvisor');
  });

  // 2
  it('a cost event updates only its own card', async () => {
    const a = run({ agent: 'alpha', spent: 0 });
    const b = run({ agent: 'beta' });
    const { sockets } = show([a, b]);
    await screen.findByRole('link', { name: 'alpha' });
    const socket = socketFor(sockets, a.id);
    socket.open();
    socket.event(1, event(a.id, { kind: 'run_created', agent: 'alpha' }, 1));
    await vi.advanceTimersByTimeAsync(0);
    const before = { alpha: card('alpha'), beta: card('beta') };
    const others: MutationRecord[] = [];
    const watch = new MutationObserver((m) => others.push(...m));
    const betaCost = card('beta').querySelector('.cost');
    if (!betaCost) throw new Error('no cost on beta');
    watch.observe(betaCost, { subtree: true, childList: true, characterData: true, attributes: true });
    socket.event(2, event(a.id, {
      kind: 'model_call', run_id: a.id, provider: 'anthropic', model: 'm', tokens_in: 1, tokens_out: 1,
      tokens_cache_read: 0, tokens_cache_write: 0, cost: 50_000, latency_ms: 1, status: 200,
    }, 2));
    await vi.advanceTimersByTimeAsync(0);
    expect(card('alpha')).toHaveTextContent('$0.05 of $2.00');
    // Neither card was rendered again: both are the same nodes, and beta's cost did not change.
    expect(card('alpha')).toBe(before.alpha);
    expect(card('beta')).toBe(before.beta);
    expect(others).toEqual([]);
    watch.disconnect();
  });

  // 3
  it('roots only shows a fan-out as one card with its child count', async () => {
    const root = run({ agent: 'lead' });
    const children = Array.from({ length: 5 }, (_, i) =>
      run({
        agent: `child-${i}`,
        parent: root.id,
        root: root.id,
        depth: 1,
        status: i < 3 ? 'Running' : 'Queued',
      }),
    );
    show([root, ...children]);
    await screen.findByRole('link', { name: 'lead' });
    expect(cards()).toHaveLength(1);
    expect(card('lead')).toHaveTextContent('5 sub-agents, 3 running');
    await user().click(screen.getByRole('checkbox', { name: 'Roots only' }));
    expect(cards()).toHaveLength(6);
    const child = card('child-0');
    expect(within(child).getByRole('link', { name: 'Parent: lead' })).toHaveAttribute('href', `#/runs/${root.id}`);
  });

  // 4
  it('filters by status chip and agent search, together, and clears', async () => {
    show([
      run({ agent: 'alpha', status: { Suspended: 'Budget' } }),
      run({ agent: 'beta', status: { Suspended: 'Manual' } }),
      run({ agent: 'alpha', status: 'Running' }),
    ]);
    await screen.findAllByRole('article');
    const u = user();
    await u.click(chip('Suspended'));
    expect(cards()).toHaveLength(2);
    await u.type(searchBox(), 'alpha');
    expect(cards()).toHaveLength(1);
    await u.click(chip('All'));
    await u.clear(searchBox());
    expect(cards()).toHaveLength(3);
  });

  // 5
  it('says how to start a run when there is none', async () => {
    show([]);
    expect(await screen.findByText('No runs yet.')).toBeInTheDocument();
    expect(screen.getByText(/kestrel run/)).toHaveTextContent('kestrel run <agent.yaml>');
  });

  // 6
  it('shows an error with Retry, and Retry fetches again', async () => {
    const { calls } = show([run({ agent: 'alpha' })], { list: 1 });
    expect(await screen.findByRole('alert')).toHaveTextContent('database is locked');
    await user().click(screen.getByRole('button', { name: 'Retry' }));
    expect(await screen.findByRole('link', { name: 'alpha' })).toBeInTheDocument();
    expect(calls.filter((c) => c === '/api/runs')).toHaveLength(2);
  });

  // 7
  it('says it is reconnecting until the socket is back, resumes after its cursor and counts each event once', async () => {
    const a = run({ agent: 'alpha', spent: 0 });
    const { sockets } = show([a]);
    await screen.findByRole('link', { name: 'alpha' });
    const first = sockets.made[0];
    if (!first) throw new Error('no socket');
    first.open();
    first.event(1, event(a.id, cost(a.id, 10_000), 1));
    first.event(2, event(a.id, cost(a.id, 20_000), 2));
    first.drop();
    expect(await screen.findByText(/Reconnecting/)).toBeInTheDocument();
    await vi.advanceTimersByTimeAsync(600);
    const second = sockets.made[1];
    if (!second) throw new Error('no reconnect');
    expect(second.url).toContain('after=2');
    // Connecting is not connected: the banner stays.
    await vi.advanceTimersByTimeAsync(0);
    expect(screen.getByText(/Reconnecting/)).toBeInTheDocument();
    second.open();
    second.event(2, event(a.id, cost(a.id, 20_000), 2));
    second.event(3, event(a.id, cost(a.id, 40_000), 3));
    await vi.advanceTimersByTimeAsync(0);
    expect(screen.queryByText(/Reconnecting/)).not.toBeInTheDocument();
    expect(card('alpha')).toHaveTextContent('$0.07 of $2.00');
  });

  it('keeps the cost of a child created while it was away', async () => {
    const root = run({ agent: 'lead', spent: 0 });
    // The server has not seen the child's cost yet when the view asks for it.
    const child = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1, spent: 0 });
    const { api } = serve([root, child]);
    const sockets = fakeSockets();
    render(Fleet, {
      api: { ...api, listRuns: async () => [root] },
      onUnauthorized: () => {},
      sockets: sockets.factory,
      base: 'ws://kestrel.test',
    });
    await screen.findByRole('link', { name: 'lead' });
    const socket = socketFor(sockets, root.id);
    socket.open();
    // A replay delivers the child's creation and its cost in one burst, before any fetch returns.
    socket.event(1, event(child.id, { kind: 'run_created', agent: 'kid', parent: root.id }, 1));
    socket.event(2, event(child.id, cost(child.id, 30_000), 2));
    await vi.advanceTimersByTimeAsync(0);
    await user().click(screen.getByRole('checkbox', { name: 'Roots only' }));
    expect(card('kid')).toHaveTextContent('$0.03 of $2.00');
  });

  it('asks for a token when a socket is refused', async () => {
    const a = run({ agent: 'alpha' });
    const { sockets, onUnauthorized } = show([a]);
    await screen.findByRole('link', { name: 'alpha' });
    socketFor(sockets, a.id).drop(4401);
    expect(onUnauthorized).toHaveBeenCalledWith(null);
  });

  it('a socket that keeps failing fetches its root, so a refused token is found', async () => {
    const root = run({ agent: 'lead' });
    const sockets = fakeSockets();
    const onUnauthorized = vi.fn();
    let refused = false;
    const fetch: typeof globalThis.fetch = async (input) =>
      String(input).endsWith('/api/runs')
        ? new Response(JSON.stringify([root]))
        : refused
          ? new Response(JSON.stringify({ error: 'token required' }), { status: 401 })
          : new Response(JSON.stringify(root));
    render(Fleet, {
      api: createApi({ fetch, getToken: () => 'old' }),
      onUnauthorized,
      sockets: sockets.factory,
      base: 'ws://kestrel.test',
    });
    await screen.findByRole('link', { name: 'lead' });
    refused = true;
    // A browser reports a 401 at the upgrade as an ordinary failed connection.
    socketFor(sockets, root.id).drop(1006);
    await vi.waitFor(() => expect(onUnauthorized).toHaveBeenCalledWith('old'));
  });

  it('tries again to fetch a new run whose fetch failed', async () => {
    const root = run({ agent: 'lead' });
    const child = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1, limit: 700_000 });
    const sockets = fakeSockets();
    let fails = 1;
    const fetch: typeof globalThis.fetch = async (input) => {
      if (String(input).endsWith('/api/runs')) return new Response(JSON.stringify([root]));
      if (String(input).endsWith(child.id) && fails-- > 0) return new Response('{}', { status: 503 });
      return new Response(JSON.stringify(String(input).endsWith(child.id) ? child : root));
    };
    render(Fleet, { api: createApi({ fetch, getToken: () => null }), onUnauthorized: () => {}, sockets: sockets.factory, base: 'ws://kestrel.test' });
    await screen.findByRole('link', { name: 'lead' });
    const socket = socketFor(sockets, root.id);
    socket.open();
    socket.event(1, event(child.id, { kind: 'run_created', agent: 'kid', parent: root.id }, 1));
    await user().click(screen.getByRole('checkbox', { name: 'Roots only' }));
    expect(card('kid')).toHaveTextContent('limit not known yet');
    await vi.advanceTimersByTimeAsync(6_000);
    expect(card('kid')).toHaveTextContent('$0.70');
  });

  it('retries a new run whose fetch failed after its status moved, until the server answers', async () => {
    const root = run({ agent: 'lead' });
    const child = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1, limit: 700_000 });
    const sockets = fakeSockets();
    let fails = 1;
    const fetch: typeof globalThis.fetch = async (input) => {
      if (String(input).endsWith('/api/runs')) return new Response(JSON.stringify([root]));
      if (String(input).endsWith(child.id) && fails-- > 0) return new Response('{}', { status: 503 });
      return new Response(JSON.stringify(String(input).endsWith(child.id) ? child : root));
    };
    render(Fleet, { api: createApi({ fetch, getToken: () => null }), onUnauthorized: () => {}, sockets: sockets.factory, base: 'ws://kestrel.test' });
    await screen.findByRole('link', { name: 'lead' });
    const socket = socketFor(sockets, root.id);
    socket.open();
    // The status moves before the failed fetch comes back.
    socket.event(1, event(child.id, { kind: 'run_created', agent: 'kid', parent: root.id }, 1));
    socket.event(2, event(child.id, { kind: 'status_changed', from: 'Queued', to: 'Provisioning' }, 2));
    await user().click(screen.getByRole('checkbox', { name: 'Roots only' }));
    expect(card('kid')).toHaveTextContent('limit not known yet');
    await vi.advanceTimersByTimeAsync(6_000);
    expect(card('kid')).toHaveTextContent('$0.70');
  });

  it('does no list work on a clock tick: only the elapsed times change', async () => {
    const done = run({ agent: 'done', status: 'Completed' });
    const { sockets } = show([run({ agent: 'live' }), done]);
    await screen.findByRole('link', { name: 'live' });
    const made = sockets.made.length;
    const live = vi.spyOn(FleetState.prototype, 'liveRoots');
    const rebuild = vi.spyOn(FleetState.prototype, 'rebuild');
    await vi.advanceTimersByTimeAsync(3_000);
    expect(live).not.toHaveBeenCalled();
    expect(rebuild).not.toHaveBeenCalled();
    expect(sockets.made).toHaveLength(made);
    live.mockRestore();
    rebuild.mockRestore();
  });

  it('backs off retrying a failing fetch, and asks in between do not add requests', async () => {
    const root = run({ agent: 'lead' });
    const child = 'kid-1';
    const sockets = fakeSockets();
    const asked: string[] = [];
    const fetch: typeof globalThis.fetch = async (input) => {
      const url = String(input);
      asked.push(url);
      if (url.endsWith('/api/runs')) return new Response(JSON.stringify([root]));
      if (url.endsWith(child)) return new Response('{}', { status: 503 });
      return new Response(JSON.stringify(root));
    };
    render(Fleet, { api: createApi({ fetch, getToken: () => null }), onUnauthorized: () => {}, sockets: sockets.factory, base: 'ws://kestrel.test' });
    await screen.findByRole('link', { name: 'lead' });
    const socket = socketFor(sockets, root.id);
    socket.open();
    socket.event(1, event(child, { kind: 'run_created', agent: 'kid', parent: root.id }, 1));
    for (let i = 0; i < 12; i++) {
      socket.event(2 + i, event(child, { kind: 'child_spawned', child: `g${i}`, agent: 'g', budget: 1 }, 2 + i));
      await vi.advanceTimersByTimeAsync(10_000);
    }
    // 120 s: the first fetch, then retries after 5, 10, 20, 40 and 60 s at most.
    expect(asked.filter((u) => u.endsWith(child)).length).toBeLessThanOrEqual(6);
  });

  it('fetches a root once per outage, however many times its socket retries', async () => {
    const root = run({ agent: 'lead' });
    const { sockets, calls } = show([root]);
    await screen.findByRole('link', { name: 'lead' });
    const fetched = () => calls.filter((c) => c === `/api/runs/${root.id}`).length;
    for (let i = 0; i < 4; i++) {
      sockets.made.at(-1)?.drop();
      await vi.advanceTimersByTimeAsync(11_000);
    }
    expect(fetched()).toBe(1);
    sockets.made.at(-1)?.open();
    sockets.made.at(-1)?.drop();
    await vi.advanceTimersByTimeAsync(0);
    expect(fetched()).toBe(2);
  });

  it('closes the socket of a tree once all of it has ended and lingered', async () => {
    const a = run({ agent: 'alpha' });
    const { sockets } = show([a]);
    await screen.findByRole('link', { name: 'alpha' });
    const socket = socketFor(sockets, a.id);
    socket.open();
    socket.event(1, event(a.id, { kind: 'status_changed', from: 'Running', to: 'Completed' }, 1));
    await vi.advanceTimersByTimeAsync(3_000);
    expect(socket.closed).toBe(false);
    await vi.advanceTimersByTimeAsync(3_000);
    expect(socket.closed).toBe(true);
    expect(sockets.made).toHaveLength(1);
  });

  it('follows a live child under a root that already ended', async () => {
    const root = run({ agent: 'lead', status: 'Cancelled' });
    const { sockets } = show([root, run({ agent: 'kid', parent: root.id, root: root.id, depth: 1 })]);
    await screen.findByRole('checkbox', { name: 'Roots only' });
    expect(sockets.made.map((s) => s.url)).toEqual([`ws://kestrel.test/ws/tree/${root.id}?after=0`]);
  });

  it('asks for a parent once at a time, however many spawns a replay brings', async () => {
    const root = run({ agent: 'lead' });
    const { sockets, calls } = show([root]);
    await screen.findByRole('link', { name: 'lead' });
    const socket = socketFor(sockets, root.id);
    socket.open();
    for (let i = 1; i <= 20; i++) {
      socket.event(i, event(root.id, { kind: 'child_spawned', child: `c${i}`, agent: 'kid', budget: 1 }, i));
    }
    const asks = () => calls.filter((c) => c === `/api/runs/${root.id}`);
    // One request, then one more for everything that came in while it ran.
    await vi.waitFor(() => expect(asks()).toHaveLength(2));
    await vi.advanceTimersByTimeAsync(100);
    expect(asks()).toHaveLength(2);
  });

  it('follows at most 50 trees, those on the page first, and replays a tree it follows again without counting twice', async () => {
    const runs = Array.from({ length: 60 }, (_, i) =>
      run({ agent: `run-${i}`, spent: 0, created_at: new Date(NOW - 1_000 * (i + 1)).toISOString() }),
    );
    const { sockets } = show(runs);
    await screen.findAllByRole('article');
    const open = () => sockets.made.filter((s) => !s.closed).map((s) => s.url);
    expect(open()).toHaveLength(50);
    // The newest 50 are on page 1, and those are the ones followed.
    expect(open()).toContain(`ws://kestrel.test/ws/tree/${runs[0]?.id}?after=0`);
    expect(open()).not.toContain(`ws://kestrel.test/ws/tree/${runs[55]?.id}?after=0`);
    // Page 2 shows runs 50 to 59; the 40 oldest-followed of page 1 stay, runs 40 to 49 make room.
    const r45 = runs[45]?.id ?? '';
    const first = socketFor(sockets, r45);
    first.open();
    first.event(1, event(r45, cost(r45, 10_000), 1));
    await user().click(screen.getByRole('button', { name: 'Next' }));
    expect(open()).toContain(`ws://kestrel.test/ws/tree/${runs[55]?.id}?after=0`);
    expect(open()).toHaveLength(50);
    await user().click(screen.getByRole('button', { name: 'Previous' }));
    // Followed again from the start: the replayed cost is not added on top.
    expect(first.closed).toBe(true);
    const again = sockets.made.filter((s) => s.url.includes(r45)).at(-1);
    if (!again || again === first) throw new Error('not followed again');
    again.open();
    again.event(1, event(r45, cost(r45, 10_000), 1));
    await vi.advanceTimersByTimeAsync(0);
    expect(card('run-45')).toHaveTextContent('$0.01 of $2.00');
  });

  it('asks for a token when a run fetch is refused', async () => {
    const root = run({ agent: 'lead' });
    const sockets = fakeSockets();
    const onUnauthorized = vi.fn();
    const fetch: typeof globalThis.fetch = async (input) =>
      String(input).endsWith('/api/runs')
        ? new Response(JSON.stringify([root]))
        : new Response(JSON.stringify({ error: 'token required' }), { status: 401 });
    render(Fleet, {
      api: createApi({ fetch, getToken: () => 'old' }),
      onUnauthorized,
      sockets: sockets.factory,
      base: 'ws://kestrel.test',
    });
    await screen.findByRole('link', { name: 'lead' });
    const socket = socketFor(sockets, root.id);
    socket.open();
    socket.event(1, event(root.id, { kind: 'child_spawned', child: 'c', agent: 'kid', budget: 1 }, 1));
    await vi.waitFor(() => expect(onUnauthorized).toHaveBeenCalledWith('old'));
  });

  it('closes every socket and ignores late answers once it is gone', async () => {
    const a = run({ agent: 'alpha' });
    const b = run({ agent: 'beta' });
    let answer: (r: Response) => void = () => {};
    const sockets = fakeSockets();
    const fetch: typeof globalThis.fetch = (input) =>
      String(input).endsWith('/api/runs')
        ? Promise.resolve(new Response(JSON.stringify([a, b])))
        : new Promise((resolve) => (answer = resolve));
    const { unmount } = render(Fleet, {
      api: createApi({ fetch, getToken: () => null }),
      onUnauthorized: () => {},
      sockets: sockets.factory,
      base: 'ws://kestrel.test',
    });
    await screen.findByRole('link', { name: 'alpha' });
    const socket = socketFor(sockets, a.id);
    socket.open();
    socket.event(1, event(a.id, { kind: 'child_spawned', child: 'c', agent: 'kid', budget: 1 }, 1));
    unmount();
    expect(sockets.made.every((s) => s.closed)).toBe(true);
    expect(sockets.made).toHaveLength(2);
    answer(new Response(JSON.stringify({ ...a, limit: 1 })));
    await vi.advanceTimersByTimeAsync(10);
    expect(sockets.made).toHaveLength(2);
  });

  it('announces a page change, not every change to the list', async () => {
    show(Array.from({ length: 60 }, (_, i) => run({ agent: `run-${i}` })));
    await screen.findAllByRole('article');
    const pager = screen.getByRole('navigation', { name: 'Pages' });
    expect(pager.querySelector('[aria-live]')).toBeNull();
    await user().click(screen.getByRole('button', { name: 'Next' }));
    expect(screen.getByText('Page 2 of 2')).toBeInTheDocument();
    // Next on the last page changes nothing, so it says nothing new.
    await user().click(screen.getByRole('button', { name: 'Next' }));
    await user().click(screen.getByRole('button', { name: 'Previous' }));
    expect(screen.getByText('Page 1 of 2')).toBeInTheDocument();
    await user().click(screen.getByRole('button', { name: 'Previous' }));
    expect(screen.getByText('Page 1 of 2')).toBeInTheDocument();
    await user().click(screen.getByRole('button', { name: 'Next' }));
    await user().type(searchBox(), 'run-1');
    expect(screen.queryByText('Page 2 of 2')).not.toBeInTheDocument();
  });

  it('forgets a page that no longer exists, so the list does not jump back to it', async () => {
    const runs = Array.from({ length: 51 }, (_, i) =>
      run({ agent: `run-${i}`, created_at: new Date(NOW - 1_000 * (i + 1)).toISOString() }),
    );
    const { sockets } = show(runs);
    await screen.findAllByRole('article');
    const u = user();
    await u.click(screen.getByRole('checkbox', { name: 'Roots only' }));
    await u.click(screen.getByRole('button', { name: 'Next' }));
    expect(screen.getByText('Showing 51 to 51 of 51')).toBeInTheDocument();
    const done = runs[0]?.id ?? '';
    const ending = socketFor(sockets, done);
    ending.open();
    ending.event(1, event(done, { kind: 'status_changed', from: 'Running', to: 'Completed' }, 1));
    await vi.advanceTimersByTimeAsync(6_000);
    expect(screen.getByText('Showing 1 to 50 of 50')).toBeInTheDocument();
    const live = runs[1]?.id ?? '';
    const growing = socketFor(sockets, live);
    growing.open();
    growing.event(1, event('kid-x', { kind: 'run_created', agent: 'kid', parent: live }, 1));
    await vi.advanceTimersByTimeAsync(0);
    expect(screen.getByText('Showing 1 to 50 of 51')).toBeInTheDocument();
  });

  it('describes each card link with its status, so links with the same agent differ', async () => {
    show([run({ agent: 'worker' }), run({ agent: 'worker' })]);
    const links = await screen.findAllByRole('link', { name: 'worker' });
    const described = links.map((l) =>
      (l.getAttribute('aria-describedby') ?? '').split(' ').map((id) => document.getElementById(id)?.textContent).join(' '),
    );
    expect(described[0]).toContain('Running');
    expect(described[0]).not.toEqual(described[1]);
  });

  it('a run known only from its creation says its limit is not known yet, and an ended run keeps its run time', async () => {
    const root = run({ agent: 'lead', spent: 0 });
    const { api } = serve([root]);
    const sockets = fakeSockets();
    render(Fleet, {
      // The fetch for the new child never answers.
      api: { ...api, getRun: (id: string) => (id === root.id ? api.getRun(id) : new Promise(() => {})) },
      onUnauthorized: () => {},
      sockets: sockets.factory,
      base: 'ws://kestrel.test',
    });
    await screen.findByRole('link', { name: 'lead' });
    const socket = socketFor(sockets, root.id);
    socket.open();
    socket.event(1, event('kid-1', { kind: 'run_created', agent: 'kid', parent: root.id }, 1));
    socket.event(2, event(root.id, { kind: 'status_changed', from: 'Running', to: 'Completed' }, 2));
    await vi.advanceTimersByTimeAsync(0);
    const u = user();
    await u.click(screen.getByRole('checkbox', { name: 'Roots only' }));
    expect(card('kid')).toHaveTextContent('limit not known yet');
    expect(card('lead')).toHaveTextContent('ran 2m 05s');
  });

  it('the Finished chip shows only finished runs', async () => {
    show([run({ agent: 'done', status: 'Completed' }), run({ agent: 'live' })]);
    await screen.findByRole('link', { name: 'live' });
    await user().click(chip('Finished'));
    expect(cards()).toHaveLength(1);
    expect(card('done')).toBeInTheDocument();
  });

  it('says when no run matches the filters', async () => {
    show([run({ agent: 'live' })]);
    await screen.findByRole('link', { name: 'live' });
    await user().click(chip('Finished'));
    expect(screen.getByText('No run matches these filters.')).toBeInTheDocument();
  });

  // 8
  it('a run that completes leaves the active view 5 seconds after it was seen, and stays under Finished', async () => {
    const a = run({ agent: 'alpha' });
    const { sockets } = show([a, run({ agent: 'beta' })]);
    await screen.findByRole('link', { name: 'alpha' });
    const socket = sockets.made.find((s) => s.url.includes(a.id));
    if (!socket) throw new Error('no socket');
    socket.open();
    socket.event(1, {
      ...event(a.id, { kind: 'status_changed', from: 'Running', to: 'Completed' }, 1),
      ts: new Date(NOW - 60_000).toISOString(),
    });
    await vi.advanceTimersByTimeAsync(0);
    expect(card('alpha')).toHaveTextContent('Completed');
    await vi.advanceTimersByTimeAsync(4_000);
    expect(screen.queryByRole('link', { name: 'alpha' })).toBeInTheDocument();
    await vi.advanceTimersByTimeAsync(1_500);
    expect(screen.queryByRole('link', { name: 'alpha' })).not.toBeInTheDocument();
    await user().click(chip('Finished'));
    expect(screen.getByRole('link', { name: 'alpha' })).toBeInTheDocument();
  });

  it('runs that had ended before the view loaded show only under Finished', async () => {
    show([run({ agent: 'done', status: 'Completed' }), run({ agent: 'live' })]);
    await screen.findByRole('link', { name: 'live' });
    expect(screen.queryByRole('link', { name: 'done' })).not.toBeInTheDocument();
    await user().click(chip('Finished'));
    expect(screen.getByRole('link', { name: 'done' })).toBeInTheDocument();
  });

  // 9. Renders 200 cards and walks pages and tabs with user-event: about 0.4 s alone, but it took
  //    7.2 s on Windows in the full suite under coverage, past the 5 s default. Its own timeout
  //    keeps a slow host from failing it.
  it('pages 200 runs 50 at a time and keeps them reachable by keyboard', { timeout: 30_000 }, async () => {
    show(Array.from({ length: 200 }, (_, i) => run({ agent: `run-${i}` })));
    await screen.findAllByRole('article');
    expect(cards()).toHaveLength(50);
    expect(screen.getByText('Showing 1 to 50 of 200')).toBeInTheDocument();
    const u = user();
    await u.click(screen.getByRole('button', { name: 'Next' }));
    expect(cards()).toHaveLength(50);
    expect(screen.getByText('Showing 51 to 100 of 200')).toBeInTheDocument();
    await u.click(screen.getByRole('button', { name: 'Previous' }));
    expect(screen.getByText('Showing 1 to 50 of 200')).toBeInTheDocument();
    // At an end the button stays focusable, so keyboard focus is not dropped.
    const previous = screen.getByRole('button', { name: 'Previous' });
    expect(previous).toHaveAttribute('aria-disabled', 'true');
    previous.focus();
    await u.keyboard('{Enter}');
    expect(document.activeElement).toBe(previous);
    expect(screen.getByText('Showing 1 to 50 of 200')).toBeInTheDocument();
    // A filter change starts again at the first page.
    await u.click(screen.getByRole('button', { name: 'Next' }));
    await u.click(screen.getByRole('button', { name: 'Next' }));
    await u.click(chip('Running'));
    expect(screen.getByText('Showing 1 to 50 of 200')).toBeInTheDocument();
    // Tab order: the filters, then the cards' links in order.
    const links = screen.getAllByRole('link');
    for (let i = 0; i < 12 && document.activeElement !== links[0]; i++) await u.tab();
    expect(document.activeElement).toBe(links[0]);
    await u.tab();
    expect(document.activeElement).toBe(links[1]);
  });

  // 10
  it('Enter on a card opens the Run view', async () => {
    const a = run({ agent: 'alpha' });
    show([a]);
    const link = await screen.findByRole('link', { name: 'alpha' });
    link.focus();
    await user().keyboard('{Enter}');
    expect(window.location.hash).toBe(`#/runs/${a.id}`);
  });

  // 11
  it('has no serious or critical axe violations', async () => {
    const root = run({ agent: 'lead' });
    const { container } = show([root, run({ agent: 'kid', parent: root.id, root: root.id, depth: 1 })]);
    await screen.findByRole('link', { name: 'lead' });
    vi.useRealTimers();
    expect(ids(await seriousViolations(container))).toEqual([]);
  });

  // 12
  it('shows a tiny cost without rounding it to zero', async () => {
    show([run({ agent: 'alpha', spent: 1, limit: 2_000_000 })]);
    expect(await screen.findByText('$0.000001 of $2.00')).toBeInTheDocument();
  });

  it('opens one tree socket per live root, from the start of its tree', async () => {
    const root = run({ agent: 'lead' });
    const { sockets } = show([
      root,
      run({ agent: 'kid', parent: root.id, root: root.id, depth: 1 }),
      run({ agent: 'old', status: { Failed: 'x' } }),
    ]);
    await screen.findByRole('link', { name: 'lead' });
    expect(sockets.made.map((s) => s.url)).toEqual([`ws://kestrel.test/ws/tree/${root.id}?after=0`]);
  });

  it('a child spawned later is fetched and counted on its root', async () => {
    const root = run({ agent: 'lead' });
    const child = run({ agent: 'kid', parent: root.id, root: root.id, depth: 1 });
    const { api } = serve([root, child]);
    const sockets = fakeSockets();
    // The list was taken before the child existed.
    const listOnly = { ...api, listRuns: async () => [root] };
    render(Fleet, { api: listOnly, onUnauthorized: () => {}, sockets: sockets.factory, base: 'ws://kestrel.test' });
    await screen.findByRole('link', { name: 'lead' });
    const socket = sockets.made[0];
    if (!socket) throw new Error('no socket');
    socket.open();
    socket.event(1, event(child.id, { kind: 'run_created', agent: 'kid', parent: root.id }, 1));
    expect(await screen.findByText('1 sub-agent, 1 running')).toBeInTheDocument();
  });

  it('ignores replayed status changes that are older than what the list showed', async () => {
    const a = run({ agent: 'alpha', status: 'Running' });
    const { sockets } = show([a]);
    await screen.findByRole('link', { name: 'alpha' });
    const socket = sockets.made[0];
    if (!socket) throw new Error('no socket');
    socket.open();
    const old: RunStatus = 'Queued';
    socket.event(1, event(a.id, { kind: 'status_changed', from: old, to: 'Provisioning' }, 1));
    await vi.advanceTimersByTimeAsync(0);
    expect(card('alpha')).toHaveTextContent('Running');
  });
});
