// Rules for the Run view: what a status allows, budget input, tree sums and token totals.

import { isTerminal } from './format';
import type { Event, MicroUsd, RunId, RunStatus } from './types';

export interface Allowed {
  pause: boolean;
  resume: boolean;
  cancel: boolean;
  budget: boolean;
  message: boolean;
}

/**
 * The controls a status allows. A run waiting on an approval resumes through its decision, not
 * through Resume. An ended run allows only Cancel, and only while `live` runs below it have not
 * ended (a run that kept its children): the server cancels that tree.
 */
export function allowed(status: RunStatus, live = 0): Allowed {
  if (isTerminal(status)) return { pause: false, resume: false, cancel: live > 0, budget: false, message: false };
  const suspended = typeof status === 'object' && 'Suspended' in status ? status.Suspended : null;
  const waitsOnApproval = typeof suspended === 'object' && suspended !== null && 'Approval' in suspended;
  return {
    pause: status === 'Running',
    resume: suspended !== null && !waitsOnApproval,
    cancel: true,
    budget: true,
    message: status === 'Running',
  };
}

/** The most a message to an agent may hold: 16 KiB. The server counts bytes. */
export const MAX_MESSAGE_BYTES = 16 * 1024;

/**
 * Why a run cannot take a message from a person now, or `null` when it can: it must be running,
 * and its agent must take messages (its spec grants `kestrel.messages`).
 */
export function messageRefusal(status: RunStatus, takesMessages: boolean): string | null {
  if (isTerminal(status)) return 'The run has ended, so it takes no more messages.';
  if (!takesMessages) return 'This agent does not take messages: its spec does not grant kestrel.messages.';
  if (status !== 'Running') return 'Only a running run takes messages.';
  return null;
}

/**
 * Dollars typed by a person as micro-dollars, with no float math: digits, optionally a point and
 * one to six more digits. `null` for anything else (a sign, a comma, an exponent, too many places).
 */
export function parseUsd(text: string): MicroUsd | null {
  const match = /^(\d+)(?:\.(\d{1,6}))?$/.exec(text.trim());
  if (!match) return null;
  const whole = Number(match[1]);
  const fraction = Number((match[2] ?? '').padEnd(6, '0'));
  return whole * 1_000_000 + fraction;
}

/** A byte count as `512 B` or `100.0 kB` (1 kB is 1000 bytes). */
export function formatSize(bytes: number): string {
  if (bytes < 1000) return `${bytes} B`;
  if (bytes < 1_000_000) return `${(bytes / 1000).toFixed(1)} kB`;
  return `${(bytes / 1_000_000).toFixed(1)} MB`;
}

interface Node {
  id: RunId;
  parent: RunId | null;
  status: RunStatus;
  limit: MicroUsd | null;
}

/** The runs below `id` that have not ended. */
export function liveBelow(runs: Node[], id: RunId): number {
  const kids: Record<RunId, Node[]> = {};
  for (const run of runs) if (run.parent !== null) (kids[run.parent] ??= []).push(run);
  let count = 0;
  const stack = [...(kids[id] ?? [])];
  for (let run = stack.pop(); run; run = stack.pop()) {
    if (!isTerminal(run.status)) count += 1;
    stack.push(...(kids[run.id] ?? []));
  }
  return count;
}

/** What `id` has carved out to its direct children: the sum of their limits. */
export function carved(runs: Node[], id: RunId): MicroUsd {
  return runs.reduce((sum, run) => (run.parent === id ? sum + (run.limit ?? 0) : sum), 0);
}

export interface Tokens {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

/** No tokens yet. */
export const NO_TOKENS: Tokens = Object.freeze({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0 });

/** `totals` plus the tokens of the model calls among `events`, as a new object. */
export function addTokens(totals: Tokens, events: Event[]): Tokens {
  const sum: Tokens = { ...totals };
  for (const { kind } of events) {
    if (kind.kind !== 'model_call') continue;
    sum.input += kind.tokens_in;
    sum.output += kind.tokens_out;
    sum.cacheRead += kind.tokens_cache_read;
    sum.cacheWrite += kind.tokens_cache_write;
  }
  return sum;
}
