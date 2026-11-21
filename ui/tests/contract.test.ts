// The shared fixture (which the Rust types round-trip exactly) must match the UI's types: the same
// keys on every object, and only values the TypeScript unions allow. JSON imports widen string
// literals to `string`, so this is checked at run time against key lists that the compiler holds
// to the types (`satisfies Record<keyof T, true>` fails on a missing or an extra key).

import { describe, expect, it } from 'vitest';
import contract from './fixtures/contract.json';
import type {
  Approval,
  ApprovalStatus,
  Event,
  RunStatus,
  RunSummary,
  SandboxTier,
  SuspendReason,
  TreeNode,
} from '../src/lib/types';
import { formatUsd, isTerminal, statusLabel } from '../src/lib/format';

const RUN_KEYS = {
  id: true,
  agent: true,
  status: true,
  parent: true,
  root: true,
  depth: true,
  spent: true,
  limit: true,
  sandbox_tier: true,
  created_at: true,
} satisfies Record<keyof RunSummary, true>;
const TREE_KEYS = {
  id: true,
  parent: true,
  agent: true,
  status: true,
  spent: true,
  limit: true,
} satisfies Record<keyof TreeNode, true>;
const APPROVAL_KEYS = {
  id: true,
  run_id: true,
  action: true,
  status: true,
  requested_at: true,
} satisfies Record<keyof Approval, true>;
const EVENT_KEYS = { run_id: true, seq: true, ts: true, kind: true } satisfies Record<keyof Event, true>;

const SANDBOX_TIERS = { auto: true, gvisor: true, 'apple-container': true, 'docker-runc': true } satisfies Record<
  SandboxTier,
  true
>;
const APPROVAL_STATUSES = { pending: true, approved: true, rejected: true } satisfies Record<ApprovalStatus, true>;
const PLAIN_STATUSES = new Set(['Queued', 'Provisioning', 'Running', 'Completed', 'Cancelled']);

function keys(o: object): string[] {
  return Object.keys(o).sort();
}

function isSuspendReason(r: unknown): r is SuspendReason {
  if (r === 'Budget' || r === 'Manual') return true;
  if (typeof r !== 'object' || r === null) return false;
  const [k, v] = Object.entries(r)[0] ?? [];
  return Object.keys(r).length === 1 && (k === 'Approval' || k === 'Error') && typeof v === 'string';
}

function isRunStatus(s: unknown): s is RunStatus {
  if (typeof s === 'string') return PLAIN_STATUSES.has(s);
  if (typeof s !== 'object' || s === null || Object.keys(s).length !== 1) return false;
  if ('Failed' in s) return typeof s.Failed === 'string';
  return 'Suspended' in s && isSuspendReason(s.Suspended);
}

describe('contract fixture', () => {
  it('runs have exactly the RunSummary keys and valid values', () => {
    expect(contract.runs).toHaveLength(3);
    for (const run of contract.runs) {
      expect(keys(run)).toEqual(keys(RUN_KEYS));
      expect(isRunStatus(run.status)).toBe(true);
      expect(run.sandbox_tier === null || run.sandbox_tier in SANDBOX_TIERS).toBe(true);
    }
  });

  it('tree nodes have exactly the TreeNode keys and valid statuses', () => {
    for (const node of contract.tree) {
      expect(keys(node)).toEqual(keys(TREE_KEYS));
      expect(isRunStatus(node.status)).toBe(true);
    }
  });

  it('approvals have exactly the Approval keys and valid statuses', () => {
    for (const a of contract.approvals) {
      expect(keys(a)).toEqual(keys(APPROVAL_KEYS));
      expect(a.status in APPROVAL_STATUSES).toBe(true);
    }
  });

  it('events and socket frames have the Event shape', () => {
    const frame = contract.frames[0];
    const events: object[] = [...contract.events];
    if (frame?.event) events.push(frame.event);
    expect(events).toHaveLength(4);
    for (const e of events) expect(keys(e)).toEqual(keys(EVENT_KEYS));
    expect(contract.frames.map((f) => f.type)).toEqual(['event', 'heartbeat']);
  });

  it('rejects shapes outside the unions', () => {
    expect(isRunStatus('Paused')).toBe(false);
    expect(isRunStatus(null)).toBe(false);
    expect(isRunStatus({ Failed: 1 })).toBe(false);
    expect(isRunStatus({ Suspended: 'Later' })).toBe(false);
    expect(isRunStatus({ Suspended: { Approval: 'a', Error: 'b' } })).toBe(false);
    expect(isRunStatus({ Suspended: { Other: 'x' } })).toBe(false);
    expect(isRunStatus({ Running: true, Failed: 'x' })).toBe(false);
  });
});

describe('statusLabel', () => {
  const cases: [RunStatus, string][] = [
    ['Queued', 'Queued'],
    ['Provisioning', 'Provisioning'],
    ['Running', 'Running'],
    ['Completed', 'Completed'],
    ['Cancelled', 'Cancelled'],
    [{ Suspended: 'Budget' }, 'Suspended: budget'],
    [{ Suspended: 'Manual' }, 'Suspended: paused'],
    [{ Suspended: { Approval: 'a1' } }, 'Suspended: waiting for approval'],
    [{ Suspended: { Error: 'disk full' } }, 'Suspended: disk full'],
    [{ Failed: 'exit 1' }, 'Failed: exit 1'],
  ];
  it.each(cases)('labels %j as %s', (status, label) => {
    expect(statusLabel(status)).toBe(label);
  });
});

describe('isTerminal', () => {
  it('is true only for finished runs', () => {
    expect(isTerminal('Completed')).toBe(true);
    expect(isTerminal('Cancelled')).toBe(true);
    expect(isTerminal({ Failed: 'x' })).toBe(true);
    expect(isTerminal('Running')).toBe(false);
    expect(isTerminal({ Suspended: 'Budget' })).toBe(false);
  });
});

describe('formatUsd', () => {
  it('turns micro-dollars into dollars with cents', () => {
    expect(formatUsd(0)).toBe('$0.00');
    expect(formatUsd(420_000)).toBe('$0.42');
    expect(formatUsd(2_000_000)).toBe('$2.00');
    expect(formatUsd(1_234_567)).toBe('$1.23');
  });
  it('shows "no limit" for a missing limit', () => {
    expect(formatUsd(null)).toBe('no limit');
  });
});
