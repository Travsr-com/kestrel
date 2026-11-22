// The runs that have not ended, for the count beside Fleet in the sidebar.
//
// The list comes from `GET /api/runs?status=`, once per live status, and live changes from the app's `/ws/events` socket
// (`run_created` and `status_changed`). Two rules keep the sources from fighting:
//
// * A run seen to end is never counted again, so a list fetched before it ended cannot bring it back.
// * A list only removes runs known before it was asked for, so a run created meanwhile is kept.

import { isTerminal } from './format';
import type { Event, RunId, RunSummary } from './types';

export class ActiveRuns {
  /** Each live run, with the sync generation it was seen in. */
  private live: Record<RunId, number> = $state({});
  loaded = $state(false);
  private readonly ended: Record<RunId, true> = {};
  private gen = 0;
  private loadedToken = 0;

  get count(): number {
    return Object.keys(this.live).length;
  }

  /** The runs that have not ended, so a view can look up one it has not seen yet. */
  get ids(): RunId[] {
    return Object.keys(this.live);
  }

  /** Starts a sync; pass the token to `load` with the list it fetched. */
  begin(): number {
    this.gen += 1;
    return this.gen;
  }

  load(list: RunSummary[], token = this.begin()): void {
    if (token < this.loadedToken) return;
    this.loadedToken = token;
    const next: Record<RunId, number> = {};
    for (const run of list) {
      if (!isTerminal(run.status) && !this.ended[run.id]) next[run.id] = token;
    }
    // Seen live after this list was asked for: the list may not have it yet.
    for (const [id, seen] of Object.entries(this.live)) {
      if (seen >= token) next[id] = seen;
    }
    this.live = next;
    this.loaded = true;
  }

  apply(event: Event): void {
    const kind = event.kind;
    if (kind.kind === 'run_created') {
      if (!this.ended[event.run_id]) this.live[event.run_id] = this.gen;
    } else if (kind.kind === 'status_changed') {
      if (isTerminal(kind.to)) {
        this.ended[event.run_id] = true;
        delete this.live[event.run_id];
      } else if (!this.ended[event.run_id]) {
        this.live[event.run_id] = this.gen;
      }
    }
  }
}
