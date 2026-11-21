// The Fleet view's state: every run it knows, updated from tree socket events.
//
// The list comes from `GET /api/runs` once. Each tree with a live run then gets a tree socket that
// replays the tree from the start and stays open for new events; a reconnect resumes after its
// cursor, so every event arrives once.
//
// Cost: a card shows the larger of the cost the server reported (the list, or a later fetch) and the
// cost counted from this view's `model_call` events. Cost only grows, so the larger one is the newer
// one: a replay climbing back from zero never shows a drop, and a call the server reports first
// is not lost either.
//
// Status: a change applies only when its `from` is the status shown, so an old change replayed after
// the list was taken never moves a run backwards.
//
// A run created after the list was taken is made from its `run_created` event at once, so its cost
// events that follow count straight away; a fetch then fills in its limit, tier and start time.

import { isTerminal } from './format';
import type { Event, MicroUsd, RunId, RunSummary } from './types';

export interface FleetRun extends RunSummary {
  /**
   * When this view saw the run end (ms since the epoch); 0 for a run that had ended before the
   * view loaded; `null` while it runs.
   */
  endedAt: number | null;
  /** When the run ended by its own event (ms since the epoch), for its run time; `null` if not seen. */
  finishedAt: number | null;
  /** The cost the server last reported. */
  reported: MicroUsd;
  /** The cost counted from this view's `model_call` events. */
  counted: MicroUsd;
  /** Known only from its `run_created` event: no status from the server or an event yet. */
  provisional: boolean;
  /** The server has answered for this run (list or fetch), so its limit and tier are real. */
  fetched: boolean;
}

/** A run's direct children, as its card shows them. */
export interface ChildCount {
  total: number;
  running: number;
}

const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

export class FleetState {
  runs: Record<RunId, FleetRun> = $state({});

  /** Replaces everything with `list`. */
  load(list: RunSummary[]): void {
    const next: Record<RunId, FleetRun> = {};
    for (const r of list) {
      next[r.id] = {
        ...r,
        endedAt: isTerminal(r.status) ? 0 : null,
        finishedAt: null,
        reported: r.spent,
        counted: 0,
        provisional: false,
        fetched: true,
      };
    }
    this.runs = next;
  }

  /**
   * Takes what a fetch says about a known run: its limit (a spawn lowers a parent's, a child's end
   * raises it), tier, start time and a cost that is never lower than what is shown. The status is
   * taken only for a run known from its creation event alone: for any other, the events are newer.
   */
  update(summary: RunSummary, now: number): void {
    const run = this.runs[summary.id];
    if (!run) return;
    run.fetched = true;
    run.limit = summary.limit;
    run.sandbox_tier = summary.sandbox_tier;
    run.created_at = summary.created_at;
    run.reported = Math.max(run.reported, summary.spent);
    run.spent = Math.max(run.reported, run.counted);
    if (run.provisional) {
      run.provisional = false;
      run.status = summary.status;
      if (isTerminal(summary.status)) run.endedAt = now;
    }
  }

  /**
   * Applies one event from the tree socket of `root`. `refresh` is asked to fetch a run that is
   * new, or whose limit changed.
   */
  apply(event: Event, now: number, refresh: (id: RunId) => void, root: RunId): void {
    const run = this.runs[event.run_id];
    const kind = event.kind;
    switch (kind.kind) {
      case 'run_created':
        if (!run) {
          const parent = kind.parent ?? null;
          const above = parent === null ? undefined : this.runs[parent];
          this.runs[event.run_id] = {
            id: event.run_id,
            agent: kind.agent,
            status: 'Queued',
            parent,
            root,
            depth: above ? above.depth + 1 : parent === null ? 0 : 1,
            spent: 0,
            limit: null,
            sandbox_tier: null,
            created_at: event.ts,
            endedAt: null,
            finishedAt: null,
            reported: 0,
            counted: 0,
            provisional: true,
            fetched: false,
          };
          refresh(event.run_id);
        }
        break;
      case 'status_changed':
        if (run && same(run.status, kind.from)) {
          run.provisional = false;
          run.status = kind.to;
          if (isTerminal(kind.to) && run.endedAt === null) {
            run.endedAt = now;
            run.finishedAt = Date.parse(event.ts);
            // A child's end shrinks its own limit to what it spent and gives the rest to the nearest
            // live ancestor, which may be above an ended parent: it and every known ancestor are
            // asked for again.
            if (run.parent) refresh(run.id);
            for (let up = run.parent; up; up = this.runs[up]?.parent ?? null) refresh(up);
          }
        }
        break;
      case 'model_call':
        if (run && kind.cost !== null) {
          run.counted += kind.cost;
          run.spent = Math.max(run.reported, run.counted);
        }
        break;
      case 'budget_warning':
        // The only event that carries a limit: a budget set by hand writes none.
        if (run) run.limit = kind.limit;
        break;
      case 'child_spawned':
        // The parent's limit went down by the child's budget.
        refresh(event.run_id);
        break;
      default:
        break;
    }
  }

  /** Direct children of each run, and how many of them run. */
  children(): Record<RunId, ChildCount> {
    const counts: Record<RunId, ChildCount> = {};
    for (const run of Object.values(this.runs)) {
      if (!run.parent) continue;
      const count = (counts[run.parent] ??= { total: 0, running: 0 });
      count.total += 1;
      if (run.status === 'Running') count.running += 1;
    }
    return counts;
  }

  /**
   * The roots of every tree that is not over at `now`, including the tree of a live child under an
   * ended root. A tree is over once every run in it ended at least `linger` ms ago, so its last
   * events still arrive.
   */
  liveRoots(now: number, linger: number): RunId[] {
    // One pass: this runs on every clock tick, over every run the server ever stored.
    const live: Record<RunId, boolean> = {};
    for (const run of Object.values(this.runs)) {
      const done = run.endedAt !== null && now - run.endedAt >= linger;
      live[run.root] = live[run.root] === true || !done;
    }
    return Object.keys(live).filter((root) => live[root]);
  }

  /**
   * Tree sockets are about to replay the trees of `roots` from the start: their runs count their
   * events again from zero. What a card shows becomes the reported floor, so it never drops
   * meanwhile. One pass for all of them.
   */
  rebuild(roots: RunId[]): void {
    const replay: Record<RunId, true> = {};
    for (const root of roots) replay[root] = true;
    for (const run of Object.values(this.runs)) {
      if (!replay[run.root]) continue;
      run.reported = run.spent;
      run.counted = 0;
    }
  }

}
