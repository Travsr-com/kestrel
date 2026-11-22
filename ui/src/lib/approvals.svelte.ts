// The pending approvals across all runs: what the inbox lists and the nav counts.
//
// The list comes from `GET /api/approvals?status=pending`, and live changes from the `/ws/events`
// socket (requests, resolutions, and runs that end). The socket starts at the newest event, so
// the list is fetched again each time it opens: that covers whatever happened while it was not
// connected. Two rules keep the two sources from fighting:
//
// * An approval seen resolved is never added back by a list that was fetched before the
//   resolution was seen, and an approval of a run seen to end comes back only as ended.
// * A list only removes rows that were known before it was asked for, so a request that arrived
//   while the list was on its way is kept.
// * A list older than one already taken is dropped (two can be on their way at once).

import type { ActiveRuns } from './active.svelte';
import type { Api } from './api';
import { isTerminal } from './format';
import { failureText, type OnUnauthorized } from './load';
import { getToken } from './token';
import type { Approval, ApprovalId, Event, RunId } from './types';
import { connectEvents, type SocketFactory } from './ws';

export interface ApprovalRow {
  approval: Approval;
  /** The run ended while the approval waited: it can no longer be decided, only dismissed. */
  ended: 'cancelled' | 'ended' | null;
  /** The sync generation it was added in. */
  gen: number;
}

export class ApprovalsState {
  rows: Record<ApprovalId, ApprovalRow> = $state({});
  loaded = $state(false);
  error: string | null = $state(null);
  private gen = 0;
  /** The newest sync whose list was taken. */
  private loadedToken = 0;
  private readonly resolved: Record<ApprovalId, true> = {};
  private readonly endedRuns: Record<RunId, 'cancelled' | 'ended'> = {};

  /** Approvals that can still be decided. */
  get count(): number {
    return Object.values(this.rows).filter((row) => row.ended === null).length;
  }

  /** Starts a sync; pass the token to `load` with the list it fetched. */
  beginSync(): number {
    this.gen += 1;
    return this.gen;
  }

  /** Takes the pending list fetched for the sync `token`. */
  load(list: Approval[], token: number): void {
    if (token < this.loadedToken) return;
    this.loadedToken = token;
    const listed: Record<ApprovalId, true> = {};
    for (const approval of list) {
      listed[approval.id] = true;
      if (this.resolved[approval.id] || this.rows[approval.id]) continue;
      this.rows[approval.id] = { approval, ended: this.endedRuns[approval.run_id] ?? null, gen: token };
    }
    for (const [id, row] of Object.entries(this.rows)) {
      // Known before this list was asked for, and not on it any more: resolved meanwhile.
      if (!listed[id] && row.gen < token && row.ended === null) delete this.rows[id];
    }
    this.loaded = true;
    this.error = null;
  }

  apply(event: Event): void {
    const kind = event.kind;
    switch (kind.kind) {
      case 'approval_requested':
        if (this.resolved[kind.id] || this.rows[kind.id]) return;
        this.rows[kind.id] = {
          approval: {
            id: kind.id,
            run_id: event.run_id,
            action: kind.action,
            status: 'pending',
            requested_at: event.ts,
            question: kind.question ?? false,
          },
          ended: null,
          gen: this.gen,
        };
        return;
      case 'approval_resolved':
        this.remove(kind.id);
        return;
      case 'status_changed': {
        if (!isTerminal(kind.to)) return;
        const ended = kind.to === 'Cancelled' ? 'cancelled' : 'ended';
        this.endedRuns[event.run_id] = ended;
        for (const row of Object.values(this.rows)) {
          if (row.approval.run_id === event.run_id) row.ended = ended;
        }
        return;
      }
      default:
        return;
    }
  }

  /** The approval is resolved (here, elsewhere, or by someone else). */
  remove(id: ApprovalId): void {
    this.resolved[id] = true;
    delete this.rows[id];
  }

  /** The run stopped waiting on this approval (the server said so before its event came). */
  endRow(id: ApprovalId): void {
    const row = this.rows[id];
    if (row) row.ended = 'ended';
  }

  /** Takes away a row the user has seen (one whose run ended). */
  dismiss(id: ApprovalId): void {
    delete this.rows[id];
  }
}

export interface FollowOptions {
  api: Api;
  state: ApprovalsState;
  onUnauthorized: OnUnauthorized;
  base: string;
  sockets?: SocketFactory;
  /** Also kept current from the same socket: the runs that have not ended. */
  runs?: ActiveRuns;
}

/** Keeps `state` (and `runs`) current: a sync now and on every socket open, and live events between. */
export function followApprovals({ api, state, onUnauthorized, base, sockets, runs }: FollowOptions) {
  let alive = true;
  function sync() {
    if (runs) {
      const asked = runs.begin();
      // The count is a hint: on a failure it stays as it was. A refused token is found by the
      // approvals request made at the same time.
      api.listRuns().then(
        (list) => {
          if (alive) runs.load(list, asked);
        },
        () => {},
      );
    }
    const token = state.beginSync();
    api.approvals('pending').then(
      (list) => {
        if (alive) state.load(list, token);
      },
      (e: unknown) => {
        if (!alive) return;
        const text = failureText(e, onUnauthorized);
        if (text !== null) state.error = text;
      },
    );
  }
  sync();
  const stream = connectEvents({
    base,
    path: '/ws/events',
    query: { kinds: `approval_requested,approval_resolved,status_changed${runs ? ',run_created' : ''}` },
    after: null,
    getToken,
    socketFactory: sockets,
    onEvent: (event) => {
      state.apply(event);
      runs?.apply(event);
    },
    onState: (s) => {
      if (s === 'open') sync();
      if (s === 'unauthorized') onUnauthorized(getToken());
    },
  });
  return {
    retry: sync,
    stop() {
      alive = false;
      stream.close();
    },
  };
}
