// Issue #39: the Fleet view. Component tests against a fake API and fake tree sockets.

import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import Fleet from '../src/views/Fleet.svelte';
import { createApi } from '../src/lib/api';
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
    expect(alpha).toHaveTextContent('$0.42 / $2.00');
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
    const others: MutationRecord[] = [];
    const watch = new MutationObserver((m) => others.push(...m));
    watch.observe(card('beta'), { subtree: true, childList: true, characterData: true, attributes: true });
    socket.event(2, event(a.id, {
      kind: 'model_call', run_id: a.id, provider: 'anthropic', model: 'm', tokens_in: 1, tokens_out: 1,
      tokens_cache_read: 0, tokens_cache_write: 0, cost: 50_000, latency_ms: 1, status: 200,
    }, 2));
    await vi.advanceTimersByTimeAsync(0);
    expect(card('alpha')).toHaveTextContent('$0.05 / $2.00');
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
    expect(card('lead')).toHaveTextContent('5 children, 3 running');
    await user().click(screen.getByRole('checkbox', { name: 'Roots only' }));
    expect(cards()).toHaveLength(6);
    const child = card('child-0');
    expect(within(child).getByRole('link', { name: 'Parent: lead' })).toHaveAttribute('href', `#/runs/${root.id}`);
  });

  // 4
  it('filters by status and agent, together, and clears', async () => {
    show([
      run({ agent: 'alpha', status: { Suspended: 'Budget' } }),
      run({ agent: 'beta', status: { Suspended: 'Manual' } }),
      run({ agent: 'alpha', status: 'Running' }),
    ]);
    await screen.findAllByRole('article');
    const u = user();
    await u.selectOptions(screen.getByRole('combobox', { name: 'Status' }), 'suspended');
    expect(cards()).toHaveLength(2);
    await u.selectOptions(screen.getByRole('combobox', { name: 'Agent' }), 'alpha');
    expect(cards()).toHaveLength(1);
    await u.selectOptions(screen.getByRole('combobox', { name: 'Status' }), '');
    await u.selectOptions(screen.getByRole('combobox', { name: 'Agent' }), '');
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
    expect(card('alpha')).toHaveTextContent('$0.07 / $2.00');
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
    expect(card('kid')).toHaveTextContent('$0.03 / $2.00');
  });

  it('asks for a token when a socket is refused', async () => {
    const a = run({ agent: 'alpha' });
    const { sockets, onUnauthorized } = show([a]);
    await screen.findByRole('link', { name: 'alpha' });
    socketFor(sockets, a.id).drop(4401);
    expect(onUnauthorized).toHaveBeenCalledWith(null);
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

  it('a finished status filter shows finished runs without "Show finished"', async () => {
    show([run({ agent: 'done', status: 'Completed' }), run({ agent: 'live' })]);
    await screen.findByRole('link', { name: 'live' });
    await user().selectOptions(screen.getByRole('combobox', { name: 'Status' }), 'completed');
    expect(cards()).toHaveLength(1);
    expect(card('done')).toBeInTheDocument();
  });

  it('says when no run matches the filters', async () => {
    show([run({ agent: 'live' })]);
    await screen.findByRole('link', { name: 'live' });
    await user().selectOptions(screen.getByRole('combobox', { name: 'Status' }), 'failed');
    expect(screen.getByText('No run matches these filters.')).toBeInTheDocument();
  });

  // 8
  it('a run that completes leaves the active view 5 seconds after it was seen, unless finished runs are shown', async () => {
    const a = run({ agent: 'alpha' });
    const { sockets } = show([a, run({ agent: 'beta' })]);
    await screen.findByRole('link', { name: 'alpha' });
    const socket = sockets.made.find((s) => s.url.includes(a.id));
    if (!socket) throw new Error('no socket');
    socket.open();
    socket.event(1, event(a.id, { kind: 'status_changed', from: 'Running', to: 'Completed' }, 1));
    await vi.advanceTimersByTimeAsync(0);
    expect(card('alpha')).toHaveTextContent('Completed');
    await vi.advanceTimersByTimeAsync(4_000);
    expect(screen.queryByRole('link', { name: 'alpha' })).toBeInTheDocument();
    await vi.advanceTimersByTimeAsync(1_500);
    expect(screen.queryByRole('link', { name: 'alpha' })).not.toBeInTheDocument();
    await user().click(screen.getByRole('checkbox', { name: 'Show finished' }));
    expect(screen.getByRole('link', { name: 'alpha' })).toBeInTheDocument();
  });

  it('runs that had ended before the view loaded show only with finished runs', async () => {
    show([run({ agent: 'done', status: 'Completed' }), run({ agent: 'live' })]);
    await screen.findByRole('link', { name: 'live' });
    expect(screen.queryByRole('link', { name: 'done' })).not.toBeInTheDocument();
    await user().click(screen.getByRole('checkbox', { name: 'Show finished' }));
    expect(screen.getByRole('link', { name: 'done' })).toBeInTheDocument();
  });

  // 9
  it('pages 200 runs 50 at a time and keeps them reachable by keyboard', async () => {
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
    await u.selectOptions(screen.getByRole('combobox', { name: 'Status' }), 'running');
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
    expect(await screen.findByText('$0.000001 / $2.00')).toBeInTheDocument();
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
    expect(await screen.findByText('1 child, 1 running')).toBeInTheDocument();
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
