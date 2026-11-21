// The JSON shapes of the Kestrel API. They mirror the Rust types in `kestrel-types` (`api.rs`,
// `event.rs`, `status.rs`); `tests/fixtures/contract.json` is checked against both.

export type RunId = string;
export type ApprovalId = string;
/** Millionths of a US dollar, as an integer. */
export type MicroUsd = number;

export type SuspendReason = 'Budget' | 'Manual' | { Approval: ApprovalId } | { Error: string };

export type RunStatus =
  | 'Queued'
  | 'Provisioning'
  | 'Running'
  | 'Completed'
  | 'Cancelled'
  | { Suspended: SuspendReason }
  | { Failed: string };

export type SandboxTier = 'auto' | 'gvisor' | 'apple-container' | 'docker-runc';

export interface RunSummary {
  id: RunId;
  agent: string;
  status: RunStatus;
  parent: RunId | null;
  root: RunId;
  depth: number;
  spent: MicroUsd;
  limit: MicroUsd | null;
  sandbox_tier: SandboxTier | null;
  /** RFC 3339. */
  created_at: string;
}

export interface TreeNode {
  id: RunId;
  parent: RunId | null;
  agent: string;
  status: RunStatus;
  spent: MicroUsd;
  limit: MicroUsd | null;
}

/** `expired`: undecided, and its run is not waiting on it, so it cannot be decided now. */
export type ApprovalStatus = 'pending' | 'approved' | 'rejected' | 'expired';

export interface Approval {
  id: ApprovalId;
  run_id: RunId;
  action: string;
  status: ApprovalStatus;
  /** RFC 3339. */
  requested_at: string;
  /** A question for a person (`ask_human`): `action` is the question, answered with text. */
  question: boolean;
}

export interface ModelCallFields {
  run_id: RunId;
  provider: string;
  model: string;
  tokens_in: number;
  tokens_out: number;
  tokens_cache_read: number;
  tokens_cache_write: number;
  cost: MicroUsd | null;
  latency_ms: number;
  status: number;
}

export type EventKind =
  | { kind: 'run_created'; agent: string; parent?: RunId }
  | { kind: 'status_changed'; from: RunStatus; to: RunStatus }
  | { kind: 'sandbox_provisioned'; driver: string; sandbox_id: string }
  | { kind: 'agent_message'; text: string }
  | { kind: 'tool_call'; name: string; input: unknown }
  | { kind: 'tool_result'; name: string; ok: boolean; output: string }
  | ({ kind: 'model_call' } & ModelCallFields)
  | { kind: 'budget_warning'; spent: MicroUsd; limit: MicroUsd }
  | { kind: 'approval_requested'; id: ApprovalId; action: string; question?: boolean }
  | { kind: 'approval_resolved'; id: ApprovalId; approved: boolean; payload?: string }
  | { kind: 'agent_log'; line: string }
  | { kind: 'run_finished'; summary: string | null }
  | { kind: 'child_spawned'; child: RunId; agent: string; budget: MicroUsd }
  | { kind: 'capabilities_dropped'; tools: string[]; hosts: string[]; secrets: string[]; agents: string[] };

export interface Event {
  run_id: RunId;
  /** Position in its run, from 1. */
  seq: number;
  /** RFC 3339. */
  ts: string;
  kind: EventKind;
}

/** One message on an event socket. `cursor` is what a reconnect sends back as `after=`. */
export type WsFrame = { type: 'event'; cursor: number; event: Event } | { type: 'heartbeat' };

export interface ApiError {
  error: string;
}
