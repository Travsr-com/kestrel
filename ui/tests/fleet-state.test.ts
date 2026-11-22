// The Fleet view's state on its own: how events change runs, and what it asks to refetch.

import { describe, expect, it } from 'vitest';
import { FleetState } from '../src/lib/fleet.svelte';
import type { Event, RunSummary } from '../src/lib/types';

function summary(id: string, over: Partial<RunSummary> = {}): RunSummary {
  return {
    id,
    agent: 'a',
    status: 'Running',
    parent: null,
    root: id,
    depth: 0,
    spent: 100,
    limit: 1_000,
    sandbox_tier: null,
    created_at: '2026-11-21T10:00:00Z',
    ...over,
  };
}

function ev(runId: string, kind: Event['kind']): Event {
  return { run_id: runId, seq: 1, ts: '2026-11-21T10:00:05Z', kind };
}

const call = (run: string, cost: number | null): Event['kind'] => ({
  kind: 'model_call', run_id: run, provider: 'x', model: 'm', tokens_in: 1, tokens_out: 1,
  tokens_cache_read: 0, tokens_cache_write: 0, cost, latency_ms: 1, status: 200,
});

describe('FleetState', () => {
  it('a spawn and a child that ends both ask for the parent again (its limit moved)', () => {
    const fleet = new FleetState();
    fleet.load([summary('p'), summary('c', { parent: 'p', root: 'p', depth: 1 })]);
    const asked: string[] = [];
    fleet.apply(ev('p', { kind: 'child_spawned', child: 'c', agent: 'a', budget: 10 }), 0, (id) => asked.push(id), 'p');
    fleet.apply(ev('c', { kind: 'status_changed', from: 'Running', to: 'Completed' }), 5, (id) => asked.push(id), 'p');
    // The child's own limit shrinks to what it spent when it ends.
    expect(asked).toEqual(['p', 'c', 'p']);
    expect(fleet.runs.c?.endedAt).toBe(5);
  });

  it('creates a run from its run_created event, so its cost events count at once', () => {
    const fleet = new FleetState();
    fleet.load([summary('p', { depth: 2, root: 'r', parent: 'q' })]);
    const asked: string[] = [];
    fleet.apply(ev('c', { kind: 'run_created', agent: 'kid', parent: 'p' }), 0, (id) => asked.push(id), 'r');
    fleet.apply(ev('c', call('c', 30)), 0, () => {}, 'r');
    expect(asked).toEqual(['c']);
    expect(fleet.runs.c).toMatchObject({
      agent: 'kid', parent: 'p', root: 'r', depth: 3, status: 'Queued', spent: 30, limit: null,
      created_at: '2026-11-21T10:00:05Z', endedAt: null,
    });
  });

  it('a run_created for an unknown parent or no parent takes the socket root and depth from it', () => {
    const fleet = new FleetState();
    fleet.apply(ev('c', { kind: 'run_created', agent: 'kid', parent: 'gone' }), 0, () => {}, 'r');
    fleet.apply(ev('r', { kind: 'run_created', agent: 'lead' }), 0, () => {}, 'r');
    expect(fleet.runs.c).toMatchObject({ root: 'r', depth: 1, parent: 'gone' });
    expect(fleet.runs.r).toMatchObject({ root: 'r', depth: 0, parent: null });
  });

  it('a run_created for a run it has asks for nothing and changes nothing', () => {
    const fleet = new FleetState();
    fleet.load([summary('p')]);
    const asked: string[] = [];
    fleet.apply(ev('p', { kind: 'run_created', agent: 'other' }), 0, (id) => asked.push(id), 'p');
    expect(asked).toEqual([]);
    expect(fleet.runs.p?.agent).toBe('a');
  });

  it('a run is fetched once the server has answered for it, not before', () => {
    const fleet = new FleetState();
    fleet.load([summary('p')]);
    expect(fleet.runs.p?.fetched).toBe(true);
    fleet.apply(ev('c', { kind: 'run_created', agent: 'kid', parent: 'p' }), 0, () => {}, 'p');
    fleet.apply(ev('c', { kind: 'status_changed', from: 'Queued', to: 'Provisioning' }), 0, () => {}, 'p');
    expect(fleet.runs.c).toMatchObject({ provisional: false, fetched: false });
    fleet.update(summary('c'), 0);
    expect(fleet.runs.c?.fetched).toBe(true);
  });

  it('a spawn shows the child at once with its budget, and asks for it and its parent', () => {
    const fleet = new FleetState();
    fleet.load([summary('p', { root: 'p' })]);
    const asked: string[] = [];
    fleet.apply(ev('p', { kind: 'child_spawned', child: 'c', agent: 'kid', budget: 300 }), 0, (id) => asked.push(id), 'p');
    expect(asked).toEqual(['c', 'p']);
    expect(fleet.runs.c).toMatchObject({ parent: 'p', root: 'p', depth: 1, limit: 300, provisional: true, fetched: false });
    // Its own creation event later changes nothing.
    fleet.apply(ev('c', { kind: 'run_created', agent: 'kid', parent: 'p' }), 0, (id) => asked.push(id), 'p');
    expect(asked).toEqual(['c', 'p']);
  });

  it('asks for a run first seen through `lookUp`, and for a known run whose limit moved through `refresh` (PR #85 review)', () => {
    const fleet = new FleetState();
    fleet.load([summary('p', { root: 'p' })]);
    const refreshed: string[] = [];
    const lookedUp: string[] = [];
    const refresh = (id: string) => refreshed.push(id);
    const lookUp = (id: string) => lookedUp.push(id);
    fleet.apply(ev('p', { kind: 'child_spawned', child: 'c', agent: 'kid', budget: 300 }), 0, refresh, 'p', lookUp);
    fleet.apply(ev('d', { kind: 'run_created', agent: 'kid', parent: 'p' }), 0, refresh, 'p', lookUp);
    expect(lookedUp).toEqual(['c', 'd']);
    expect(refreshed).toEqual(['p']);
  });

  it('takes the status from a fetch when told the answer is newer', () => {
    const fleet = new FleetState();
    fleet.load([summary('p')]);
    fleet.update(summary('p', { status: 'Completed' }), 4);
    expect(fleet.runs.p?.status).toBe('Running');
    fleet.update(summary('p', { status: 'Completed' }), 4, true);
    expect(fleet.runs.p).toMatchObject({ status: 'Completed', endedAt: 4 });
  });

  it('a fetch fills in limit, tier and start time, and never lowers the cost', () => {
    const fleet = new FleetState();
    fleet.apply(ev('c', { kind: 'run_created', agent: 'kid', parent: 'p' }), 0, () => {}, 'p');
    fleet.apply(ev('c', call('c', 30)), 0, () => {}, 'p');
    fleet.update(summary('c', { spent: 0, limit: 500, sandbox_tier: 'gvisor', created_at: '2026-11-21T09:00:00Z' }), 0);
    expect(fleet.runs.c).toMatchObject({ spent: 30, limit: 500, sandbox_tier: 'gvisor', created_at: '2026-11-21T09:00:00Z' });
    fleet.update(summary('c', { spent: 80 }), 0);
    expect(fleet.runs.c?.spent).toBe(80);
    fleet.update(summary('ghost'), 0);
    expect(fleet.runs.ghost).toBeUndefined();
  });

  it('a fetch sets the status of a run only known from its run_created event', () => {
    const fleet = new FleetState();
    fleet.apply(ev('c', { kind: 'run_created', agent: 'kid', parent: 'p' }), 0, () => {}, 'p');
    fleet.update(summary('c', { status: 'Completed' }), 9);
    expect(fleet.runs.c).toMatchObject({ status: 'Completed', endedAt: 9 });
    // Once a status is known, a fetch (which may be older than the events) does not move it.
    fleet.update(summary('c', { status: 'Running' }), 10);
    expect(fleet.runs.c?.status).toBe('Completed');
  });

  it('after a status event, a fetch does not move the status of an event-made run', () => {
    const fleet = new FleetState();
    fleet.apply(ev('c', { kind: 'run_created', agent: 'kid', parent: 'p' }), 0, () => {}, 'p');
    fleet.apply(ev('c', { kind: 'status_changed', from: 'Queued', to: 'Running' }), 0, () => {}, 'p');
    fleet.update(summary('c', { status: 'Provisioning' }), 0);
    expect(fleet.runs.c?.status).toBe('Running');
    fleet.apply(ev('d', { kind: 'run_created', agent: 'kid', parent: 'p' }), 0, () => {}, 'p');
    fleet.update(summary('d', { status: 'Running' }), 0);
    expect(fleet.runs.d).toMatchObject({ status: 'Running', endedAt: null });
  });

  it('counts unpriced calls as nothing and ignores events for runs it does not have', () => {
    const fleet = new FleetState();
    fleet.load([summary('p')]);
    fleet.apply(ev('p', call('p', null)), 0, () => {}, 'p');
    fleet.apply(ev('ghost', call('ghost', 5)), 0, () => {}, 'p');
    fleet.apply(ev('p', { kind: 'agent_log', line: 'x' }), 0, () => {}, 'p');
    expect(fleet.runs.p?.spent).toBe(100);
  });

  it('shows the larger of the reported cost and the cost counted from events, so a replay never drops it', () => {
    const fleet = new FleetState();
    fleet.load([summary('p', { spent: 100 })]);
    fleet.apply(ev('p', call('p', 60)), 0, () => {}, 'p');
    expect(fleet.runs.p?.spent).toBe(100);
    fleet.apply(ev('p', call('p', 60)), 0, () => {}, 'p');
    expect(fleet.runs.p?.spent).toBe(120);
  });

  it('a root that ends asks for nothing, and an ended run keeps the time it was first seen to end', () => {
    const fleet = new FleetState();
    fleet.load([summary('p'), summary('done', { status: 'Completed' })]);
    const asked: string[] = [];
    fleet.apply(ev('p', { kind: 'status_changed', from: 'Running', to: 'Cancelled' }), 7, (id) => asked.push(id), 'p');
    expect(asked).toEqual([]);
    expect(fleet.runs.p?.endedAt).toBe(7);
    expect(fleet.runs.done?.endedAt).toBe(0);
  });

  it('counts each run\'s children and how many of them run', () => {
    const fleet = new FleetState();
    fleet.load([
      summary('p'),
      summary('a', { parent: 'p' }),
      summary('b', { parent: 'p', status: 'Queued' }),
      summary('g', { parent: 'a' }),
    ]);
    expect(fleet.children()).toEqual({ p: { total: 2, running: 1 }, a: { total: 1, running: 1 } });
  });

  it('a tree is over once every run in it ended at least `linger` ago', () => {
    const fleet = new FleetState();
    fleet.load([summary('r'), summary('c', { parent: 'r', root: 'r', status: 'Completed' })]);
    expect(fleet.liveRoots(100, 10)).toEqual(['r']);
    fleet.apply(ev('r', { kind: 'status_changed', from: 'Running', to: 'Completed' }), 95, () => {}, 'r');
    expect(fleet.liveRoots(100, 10)).toEqual(['r']);
    expect(fleet.liveRoots(105, 10)).toEqual([]);
  });

  it('lists the roots of trees not over yet, including a live child under an ended root', () => {
    const fleet = new FleetState();
    fleet.load([
      summary('r', { status: 'Cancelled' }),
      summary('c', { parent: 'r', root: 'r' }),
      summary('s', { status: { Failed: 'boom' } }),
      summary('t'),
    ]);
    expect(fleet.liveRoots(100, 10).sort()).toEqual(['r', 't']);
    fleet.apply(ev('t', { kind: 'status_changed', from: 'Running', to: 'Completed' }), 95, () => {}, 't');
    expect(fleet.liveRoots(100, 10).sort()).toEqual(['r', 't']);
    expect(fleet.liveRoots(105, 10).sort()).toEqual(['r']);
  });

  it('finds live roots in one pass, fast enough for every tick with thousands of runs', () => {
    const fleet = new FleetState();
    const list = Array.from({ length: 5_000 }, (_, i) =>
      summary(`r${i}`, i % 100 === 0 ? {} : { status: 'Completed' }),
    );
    fleet.load(list);
    fleet.liveRoots(100, 10);
    const start = performance.now();
    expect(fleet.liveRoots(100, 10)).toHaveLength(50);
    // A quadratic walk took over a minute here; a loaded CI runner stays far below this bound.
    expect(performance.now() - start).toBeLessThan(2_000);
  });

  it('rebuilds many trees in one pass', () => {
    const fleet = new FleetState();
    fleet.load(Array.from({ length: 5_000 }, (_, i) => summary(`r${i}`)));
    const roots = Array.from({ length: 1_000 }, (_, i) => `r${i}`);
    fleet.rebuild(['warm']);
    const start = performance.now();
    fleet.rebuild(roots);
    // One pass per root would walk 5,000 runs 1,000 times (seconds); one pass is milliseconds.
    expect(performance.now() - start).toBeLessThan(1_000);
    expect(fleet.runs.r0).toMatchObject({ reported: 100, counted: 0 });
  });

  it('a budget warning carries the current limit', () => {
    const fleet = new FleetState();
    fleet.load([summary('r')]);
    fleet.apply(ev('r', { kind: 'budget_warning', spent: 800, limit: 900 }), 0, () => {}, 'r');
    fleet.apply(ev('ghost', { kind: 'budget_warning', spent: 1, limit: 2 }), 0, () => {}, 'r');
    expect(fleet.runs.r?.limit).toBe(900);
  });

  it('a replay after a rebuild keeps what was shown as the floor', () => {
    const fleet = new FleetState();
    fleet.load([summary('r', { spent: 0 })]);
    fleet.apply(ev('r', call('r', 20)), 0, () => {}, 'r');
    fleet.apply(ev('r', call('r', 20)), 0, () => {}, 'r');
    fleet.rebuild(['r']);
    // The replay has reached only the first call so far.
    fleet.apply(ev('r', call('r', 20)), 0, () => {}, 'r');
    expect(fleet.runs.r?.spent).toBe(40);
  });

  it('a child that ends asks for every ancestor, since unspent budget goes to the nearest live one', () => {
    const fleet = new FleetState();
    fleet.load([
      summary('r'),
      summary('p', { parent: 'r', root: 'r', depth: 1, status: 'Completed' }),
      summary('c', { parent: 'p', root: 'r', depth: 2 }),
    ]);
    const asked: string[] = [];
    fleet.apply(ev('c', { kind: 'status_changed', from: 'Running', to: 'Completed' }), 0, (id) => asked.push(id), 'r');
    expect(asked).toEqual(['c', 'p', 'r']);
  });

  it('remembers when a run ended by its event time, for the run time', () => {
    const fleet = new FleetState();
    fleet.load([summary('r')]);
    fleet.apply(ev('r', { kind: 'status_changed', from: 'Running', to: 'Completed' }), 999_999_999_999, () => {}, 'r');
    expect(fleet.runs.r?.finishedAt).toBe(Date.parse('2026-11-21T10:00:05Z'));
    expect(fleet.runs.r?.endedAt).toBe(999_999_999_999);
  });

  it('a replay from the start counts a tree again without dropping what it shows', () => {
    const fleet = new FleetState();
    fleet.load([summary('r', { spent: 0 }), summary('x', { spent: 0 })]);
    fleet.apply(ev('r', call('r', 40)), 0, () => {}, 'r');
    fleet.apply(ev('x', call('x', 40)), 0, () => {}, 'x');
    fleet.rebuild(['r']);
    expect(fleet.runs.r?.spent).toBe(40);
    fleet.apply(ev('r', call('r', 40)), 0, () => {}, 'r');
    expect(fleet.runs.r?.spent).toBe(40);
    fleet.apply(ev('r', call('r', 5)), 0, () => {}, 'r');
    expect(fleet.runs.r?.spent).toBe(45);
    expect(fleet.runs.x).toMatchObject({ spent: 40, counted: 40 });
  });
});
