import type { MicroUsd, RunStatus, SuspendReason } from './types';

function suspendLabel(reason: SuspendReason): string {
  if (reason === 'Budget') return 'budget';
  if (reason === 'Manual') return 'paused';
  if ('Approval' in reason) return 'waiting for approval';
  return reason.Error;
}

/** A short, human label for a run status. */
export function statusLabel(status: RunStatus): string {
  if (typeof status === 'string') return status;
  if ('Suspended' in status) return `Suspended: ${suspendLabel(status.Suspended)}`;
  return `Failed: ${status.Failed}`;
}

/** The lower-case name of a status, as the API's `?status=` filter takes it. */
export function statusName(status: RunStatus): string {
  if (typeof status === 'string') return status.toLowerCase();
  return 'Suspended' in status ? 'suspended' : 'failed';
}

/** True once the run can no longer change state. */
export function isTerminal(status: RunStatus): boolean {
  return status === 'Completed' || status === 'Cancelled' || (typeof status === 'object' && 'Failed' in status);
}

/**
 * Micro-dollars as dollars, from the integer, with no float math: at least cents, and as many
 * more places as it takes to show the exact amount, e.g. `$0.42`, `$0.125`, `$0.000001` or
 * `-$1.234567`. `null` means no limit.
 */
export function formatUsd(micro: MicroUsd | null): string {
  if (micro === null) return 'no limit';
  const abs = Math.abs(micro);
  const whole = Math.floor(abs / 1_000_000);
  let fraction = String(abs % 1_000_000).padStart(6, '0');
  while (fraction.length > 2 && fraction.endsWith('0')) fraction = fraction.slice(0, -1);
  return `${micro < 0 ? '-' : ''}$${whole}.${fraction}`;
}

const two = (n: number) => String(n).padStart(2, '0');

/**
 * Newest first, for RFC 3339 UTC times. The server writes fractions of varying length (`...09Z`,
 * `...09.5Z`), so whole seconds compare as text and only a tie is parsed.
 */
export function newestFirst(a: string, b: string): number {
  const sa = a.slice(0, 19);
  const sb = b.slice(0, 19);
  if (sa !== sb) return sa < sb ? 1 : -1;
  return Date.parse(b) - Date.parse(a);
}

/** A duration in milliseconds as `45s`, `2m 05s` or `1h 02m`. */
export function formatElapsed(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${two(seconds % 60)}s`;
  return `${Math.floor(minutes / 60)}h ${two(minutes % 60)}m`;
}
