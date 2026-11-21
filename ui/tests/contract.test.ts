// The shared fixture (which the Rust types round-trip exactly) must match the UI's types: the same
// keys on every object, and only values the TypeScript unions allow. JSON imports widen string
// literals to `string`, so this is checked at run time against key lists that the compiler holds
// to the types (`satisfies Record<keyof T, true>` fails on a missing or an extra key).

import { describe, expect, it } from 'vitest';
import contract from './fixtures/contract.json';
import { WS_CLOSE_UNAUTHORIZED, WS_PROTOCOL, WS_TOKEN_PREFIX } from '../src/lib/ws';
import type {
  Approval,
  ApprovalStatus,
  Event,
  EventKind,
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

type Variant<K extends EventKind['kind']> = Extract<EventKind, { kind: K }>;
const EVENT_KIND_KEYS = {
  run_created: { kind: true, agent: true, parent: true },
  status_changed: { kind: true, from: true, to: true },
  sandbox_provisioned: { kind: true, driver: true, sandbox_id: true },
  agent_message: { kind: true, text: true },
  tool_call: { kind: true, name: true, input: true },
  tool_result: { kind: true, name: true, ok: true, output: true },
  model_call: {
    kind: true,
    run_id: true,
    provider: true,
    model: true,
    tokens_in: true,
    tokens_out: true,
    tokens_cache_read: true,
    tokens_cache_write: true,
    cost: true,
    latency_ms: true,
    status: true,
  },
  budget_warning: { kind: true, spent: true, limit: true },
  approval_requested: { kind: true, id: true, action: true },
  approval_resolved: { kind: true, id: true, approved: true, payload: true },
  agent_log: { kind: true, line: true },
  run_finished: { kind: true, summary: true },
  child_spawned: { kind: true, child: true, agent: true, budget: true },
  capabilities_dropped: { kind: true, tools: true, hosts: true, secrets: true, agents: true },
} satisfies { [K in EventKind['kind']]: Record<keyof Variant<K>, true> };

type ValueKind = 'string' | 'number' | 'string|null' | 'number|null' | 'status' | 'tier|null';
const RUN_VALUE_KINDS = {
  id: 'string',
  agent: 'string',
  status: 'status',
  parent: 'string|null',
  root: 'string',
  depth: 'number',
  spent: 'number',
  limit: 'number|null',
  sandbox_tier: 'tier|null',
  created_at: 'string',
} satisfies Record<keyof RunSummary, ValueKind>;

function hasKind(v: unknown, kind: ValueKind): boolean {
  switch (kind) {
    case 'string':
      return typeof v === 'string';
    case 'number':
      return Number.isInteger(v);
    case 'string|null':
      return v === null || typeof v === 'string';
    case 'number|null':
      return v === null || Number.isInteger(v);
    case 'status':
      return isRunStatus(v);
    case 'tier|null':
      return v === null || (typeof v === 'string' && v in SANDBOX_TIERS);
  }
}

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

  it('every event kind has exactly the keys of its EventKind variant', () => {
    const seen = new Set<string>();
    for (const k of contract.event_kinds) {
      const expected = EVENT_KIND_KEYS[k.kind as EventKind['kind']];
      expect(expected, `unknown kind ${k.kind}`).toBeDefined();
      // `parent` is the one optional field: Rust leaves it out of a root's `run_created`.
      const required = Object.keys(expected).filter((key) => !(k.kind === 'run_created' && key === 'parent'));
      expect(keys(k).filter((key) => !required.includes(key) && !(key in expected))).toEqual([]);
      for (const key of required) expect(k).toHaveProperty(key);
      seen.add(k.kind);
    }
    expect(seen.size).toBe(Object.keys(EVENT_KIND_KEYS).length);
  });

  it('run values have the types RunSummary declares', () => {
    for (const run of contract.runs) {
      for (const [key, kind] of Object.entries(RUN_VALUE_KINDS)) {
        expect(hasKind((run as Record<string, unknown>)[key], kind), `${key} of ${run.id}`).toBe(true);
      }
    }
  });

  it('the socket conventions match the client', () => {
    expect(contract.socket).toEqual({
      protocol: WS_PROTOCOL,
      token_prefix: WS_TOKEN_PREFIX,
      close_unauthorized: WS_CLOSE_UNAUTHORIZED,
    });
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
  it('puts the sign in front of a negative amount', () => {
    expect(formatUsd(-1)).toBe('-$0.00');
    expect(formatUsd(-1_234_567)).toBe('-$1.23');
  });
  it('shows "no limit" for a missing limit', () => {
    expect(formatUsd(null)).toBe('no limit');
  });
});
