// The Fleet view's state: every run it knows, updated from tree socket events.
//
// The list comes from `GET /api/runs` once. Each live root then gets a tree socket that replays
// its tree from the start and stays open for new events. Cost is rebuilt from those events (the
// `model_call` costs), so a reconnect that resumes from its cursor neither misses nor counts an
// event twice. A status change applies only when its `from` is the status shown, so an old change
// replayed after the list was taken never moves a run backwards.

import { isTerminal } from './format';
import type { Event, RunId, RunStatus, RunSummary } from './types';

export interface FleetRun extends RunSummary {
  /**
   * When this view saw the run end (ms since the epoch); 0 for a run that had ended before the
   * view loaded; `null` while it runs.
   */
  endedAt: number | null;
}

/** Runs below a root, as its card shows them. */
export interface ChildCount {
  total: number;
  running: number;
}

const same = (a: RunStatus, b: RunStatus) => JSON.stringify(a) === JSON.stringify(b);

export class FleetState {
  runs: Record<RunId, FleetRun> = $state({});

  /** Replaces everything with `list`. */
  load(list: RunSummary[]): void {
    const next: Record<RunId, FleetRun> = {};
    for (const r of list) next[r.id] = { ...r, endedAt: isTerminal(r.status) ? 0 : null };
    this.runs = next;
  }

  /** Adds a run first seen through an event. Its cost starts at 0: its own events bring it. */
  add(summary: RunSummary): void {
    if (this.runs[summary.id]) return;
    this.runs[summary.id] = {
      ...summary,
      spent: 0,
      endedAt: isTerminal(summary.status) ? 0 : null,
    };
  }

  /** Takes a fresh limit for a known run (a spawn lowers a parent's, a child's end raises it). */
  setLimit(id: RunId, limit: RunSummary['limit']): void {
    const run = this.runs[id];
    if (run) run.limit = limit;
  }

  /** A tree socket starts a replay from the beginning: its runs' costs are rebuilt from it. */
  rebuild(root: RunId): void {
    for (const run of Object.values(this.runs)) {
      if (run.root === root) run.spent = 0;
    }
  }

  /**
   * Applies one event. `refresh` is asked to fetch a run the view does not have yet, or whose
   * limit changed.
   */
  apply(event: Event, now: number, refresh: (id: RunId) => void): void {
    const run = this.runs[event.run_id];
    const kind = event.kind;
    switch (kind.kind) {
      case 'run_created':
        if (!run) refresh(event.run_id);
        break;
      case 'status_changed':
        if (run && same(run.status, kind.from)) {
          run.status = kind.to;
          if (isTerminal(kind.to) && run.endedAt === null) {
            run.endedAt = now;
            // A child's end gives its unspent budget back to its parent.
            if (run.parent) refresh(run.parent);
          }
        }
        break;
      case 'model_call':
        if (run && kind.cost !== null) run.spent += kind.cost;
        break;
      case 'child_spawned':
        // The parent's limit went down by the child's budget.
        refresh(event.run_id);
        break;
      default:
        break;
    }
  }

  /** Children of each run, and how many of them run. */
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
}
