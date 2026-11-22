// Fetches runs a view needs again (a new run's limit, a parent's limit after a spawn), politely.
//
// One request per run at a time: whatever asks while one is out gets a single request after it, so
// a replay of many spawns costs two requests, and answers never arrive out of order. A failure the
// caller wants retried is tried again after 5 s, doubling up to 60 s; asks while a retry waits wait
// with it. `lookUp` is for a run just seen: any answer will do, so it asks for nothing more while
// one is out. `stop` ends it all: no answer is handed on and no retry runs after it.

import type { RunId, RunSummary } from './types';

const RETRY_MS = 5_000;
const MAX_RETRY_MS = 60_000;

export interface RefresherOptions {
  fetch: (id: RunId) => Promise<RunSummary>;
  answered: (summary: RunSummary) => void;
  failed: (error: unknown) => void;
  /** Whether a failed fetch of `id` should be tried again. */
  retry: (id: RunId, error: unknown) => boolean;
}

export class Refresher {
  private readonly asking = new Set<RunId>();
  private readonly again = new Set<RunId>();
  private readonly timers = new Map<RunId, ReturnType<typeof setTimeout>>();
  private readonly delay = new Map<RunId, number>();
  private stopped = false;

  constructor(private readonly opts: RefresherOptions) {}

  refresh(id: RunId): void {
    if (this.stopped || this.timers.has(id)) return;
    if (this.asking.has(id)) {
      this.again.add(id);
      return;
    }
    this.asking.add(id);
    this.opts
      .fetch(id)
      .then(
        (summary) => {
          this.delay.delete(id);
          if (!this.stopped) this.opts.answered(summary);
        },
        (error: unknown) => {
          if (this.stopped) return;
          this.opts.failed(error);
          if (this.opts.retry(id, error)) this.schedule(id);
          else this.delay.delete(id);
        },
      )
      .finally(() => {
        this.asking.delete(id);
        if (this.again.delete(id)) this.refresh(id);
      });
  }

  /** Fetches `id` unless a fetch of it is out or waiting to be tried again. */
  lookUp(id: RunId): void {
    if (!this.asking.has(id)) this.refresh(id);
  }

  stop(): void {
    this.stopped = true;
    for (const timer of this.timers.values()) clearTimeout(timer);
    this.timers.clear();
  }

  private schedule(id: RunId): void {
    const wait = this.delay.get(id) ?? RETRY_MS;
    this.delay.set(id, Math.min(wait * 2, MAX_RETRY_MS));
    this.timers.set(
      id,
      setTimeout(() => {
        this.timers.delete(id);
        this.refresh(id);
      }, wait),
    );
  }
}
