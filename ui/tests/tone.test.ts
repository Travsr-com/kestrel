// Issue #84: the short status label and colour tone every restyled view shares.

import { describe, expect, it } from 'vitest';
import { percentUsed, statusPill, statusTone } from '../src/lib/format';
import type { RunStatus } from '../src/lib/types';

describe('statusPill and statusTone', () => {
  it.each<[RunStatus, string, string]>([
    ['Queued', 'Queued', 'queued'],
    ['Provisioning', 'Provisioning', 'running'],
    ['Running', 'Running', 'running'],
    ['Completed', 'Completed', 'done'],
    ['Cancelled', 'Cancelled', 'queued'],
    [{ Suspended: { Approval: 'a' } }, 'Needs approval', 'approval'],
    [{ Suspended: 'Budget' }, 'Suspended', 'suspended'],
    [{ Suspended: 'Manual' }, 'Paused', 'suspended'],
    [{ Suspended: { Error: 'boom' } }, 'Suspended', 'suspended'],
    [{ Failed: 'exit 1' }, 'Failed', 'failed'],
  ])('%j is "%s" in tone %s', (status, label, tone) => {
    expect(statusPill(status)).toBe(label);
    expect(statusTone(status)).toBe(tone);
  });
});

describe('percentUsed', () => {
  it('is whole percent of the limit, capped at 100, and 100 for a zero limit', () => {
    expect(percentUsed(680_000, 1_000_000)).toBe(68);
    expect(percentUsed(999_999, 1_000_000)).toBe(99);
    expect(percentUsed(3_000_000, 1_000_000)).toBe(100);
    expect(percentUsed(0, 0)).toBe(100);
    expect(percentUsed(5, null)).toBeNull();
  });
});
