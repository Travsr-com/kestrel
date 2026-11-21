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
  return { run_id: runId, seq: 1, ts: '2026-11-21T10:00:00Z', kind };
}

describe('FleetState', () => {
  it('a spawn and a child that ends both ask for the parent again (its limit moved)', () => {
    const fleet = new FleetState();
    fleet.load([summary('p'), summary('c', { parent: 'p', root: 'p', depth: 1 })]);
    const asked: string[] = [];
    fleet.apply(ev('p', { kind: 'child_spawned', child: 'c', agent: 'a', budget: 10 }), 0, (id) => asked.push(id));
    fleet.apply(ev('c', { kind: 'status_changed', from: 'Running', to: 'Completed' }), 5, (id) => asked.push(id));
    expect(asked).toEqual(['p', 'p']);
    expect(fleet.runs.c?.endedAt).toBe(5);
  });

  it('takes a fresh limit only for a run it knows', () => {
    const fleet = new FleetState();
    fleet.load([summary('p')]);
    fleet.setLimit('p', 500);
    fleet.setLimit('ghost', 1);
    expect(fleet.runs.p?.limit).toBe(500);
    expect(fleet.runs.ghost).toBeUndefined();
  });

  it('adds a run once, with its cost starting at 0, and asks only for unknown runs', () => {
    const fleet = new FleetState();
    fleet.load([summary('p')]);
    fleet.add(summary('c', { parent: 'p', spent: 999 }));
    fleet.add(summary('c', { parent: 'p', spent: 5 }));
    expect(fleet.runs.c?.spent).toBe(0);
    const asked: string[] = [];
    fleet.apply(ev('c', { kind: 'run_created', agent: 'a', parent: 'p' }), 0, (id) => asked.push(id));
    fleet.apply(ev('n', { kind: 'run_created', agent: 'a', parent: 'p' }), 0, (id) => asked.push(id));
    expect(asked).toEqual(['n']);
  });

  it('counts unpriced calls as nothing and ignores events for runs it does not have', () => {
    const fleet = new FleetState();
    fleet.load([summary('p')]);
    const call = (run: string, cost: number | null): Event['kind'] => ({
      kind: 'model_call', run_id: run, provider: 'x', model: 'm', tokens_in: 1, tokens_out: 1,
      tokens_cache_read: 0, tokens_cache_write: 0, cost, latency_ms: 1, status: 200,
    });
    fleet.apply(ev('p', call('p', null)), 0, () => {});
    fleet.apply(ev('ghost', call('ghost', 5)), 0, () => {});
    fleet.apply(ev('p', { kind: 'agent_log', line: 'x' }), 0, () => {});
    expect(fleet.runs.p?.spent).toBe(100);
  });

  it('a root that ends asks for nothing, and an ended run keeps the time it was first seen to end', () => {
    const fleet = new FleetState();
    fleet.load([summary('p'), summary('done', { status: 'Completed' })]);
    const asked: string[] = [];
    fleet.apply(ev('p', { kind: 'status_changed', from: 'Running', to: 'Cancelled' }), 7, (id) => asked.push(id));
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
});
