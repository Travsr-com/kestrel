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

/** True once the run can no longer change state. */
export function isTerminal(status: RunStatus): boolean {
  return status === 'Completed' || status === 'Cancelled' || (typeof status === 'object' && 'Failed' in status);
}

/** Micro-dollars as dollars and cents, rounded down, e.g. `$0.42`. `null` means no limit. */
export function formatUsd(micro: MicroUsd | null): string {
  if (micro === null) return 'no limit';
  const cents = Math.floor(micro / 10_000);
  return `$${Math.floor(cents / 100)}.${String(cents % 100).padStart(2, '0')}`;
}
