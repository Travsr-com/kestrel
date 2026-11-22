<script lang="ts">
  // One event in a run's stream: its time, a short kind label in the kind's colour, and its text.
  // Tool calls and results start collapsed: a collapsed one renders only its summary, so a large
  // output costs nothing until it is opened.
  import { formatUsd, statusLabel } from '../lib/format';
  import { href } from '../lib/router';
  import { formatSize } from '../lib/run';
  import type { Event, EventKind } from '../lib/types';

  let { event }: { event: Event } = $props();

  const LABELS: Record<EventKind['kind'], string> = {
    run_created: 'created',
    status_changed: 'status',
    sandbox_provisioned: 'sandbox',
    agent_message: 'message',
    tool_call: 'tool',
    tool_result: 'result',
    model_call: 'model',
    budget_warning: 'budget',
    approval_requested: 'approval',
    approval_resolved: 'approval',
    agent_log: 'log',
    run_finished: 'done',
    child_spawned: 'spawn',
    capabilities_dropped: 'limits',
  };

  function colour(kind: EventKind): string {
    switch (kind.kind) {
      case 'child_spawned':
        return 'suspended';
      case 'model_call':
        return 'accent';
      case 'tool_call':
        return 'running';
      case 'tool_result':
        return kind.ok ? 'running' : 'danger';
      case 'run_finished':
        return 'done';
      case 'approval_requested':
      case 'approval_resolved':
      case 'budget_warning':
        return 'approval';
      default:
        return 'muted';
    }
  }

  let open = $state(false);
  const kind = $derived(event.kind);
  // The viewer's local time; the full UTC time is in `datetime` and the tooltip.
  const time = $derived(new Date(event.ts).toLocaleTimeString([], { hour12: false }));
  const bytes = (text: string) => new TextEncoder().encode(text).length;
</script>

<li class="event">
  <time datetime={event.ts} title={event.ts}>{time}</time>
  <span class="kind c-{colour(kind)}">{LABELS[kind.kind]}</span>
  <div class="text">
    {#if kind.kind === 'run_created'}
      <span>Run created: {kind.agent}</span>
    {:else if kind.kind === 'status_changed'}
      <span>Status changed from {statusLabel(kind.from)} to {statusLabel(kind.to)}</span>
    {:else if kind.kind === 'sandbox_provisioned'}
      <span>Sandbox ready: {kind.driver} ({kind.sandbox_id})</span>
    {:else if kind.kind === 'agent_message'}
      <p class="message">{kind.text}</p>
    {:else if kind.kind === 'tool_call'}
      <button type="button" class="fold" aria-expanded={open} onclick={() => (open = !open)}>Tool call: {kind.name}</button>
      {#if open}
        <pre>{JSON.stringify(kind.input, null, 2)}</pre>
      {/if}
    {:else if kind.kind === 'tool_result'}
      <button type="button" class="fold" aria-expanded={open} onclick={() => (open = !open)}>
        Tool result: {kind.name}, {kind.ok ? 'ok' : 'failed'}, {formatSize(bytes(kind.output))}
      </button>
      {#if open}
        <pre>{kind.output}</pre>
      {/if}
    {:else if kind.kind === 'model_call'}
      <span>
        Model call: {kind.model}, {kind.tokens_in} in, {kind.tokens_out} out, {kind.cost === null
          ? 'cost not known'
          : formatUsd(kind.cost)}
      </span>
    {:else if kind.kind === 'budget_warning'}
      <span>Budget warning: {formatUsd(kind.spent)} of {formatUsd(kind.limit)}</span>
    {:else if kind.kind === 'approval_requested'}
      <span>Approval requested: {kind.action}</span>
      <a href={href({ name: 'approvals' })}>Open approvals</a>
    {:else if kind.kind === 'approval_resolved'}
      <span>{kind.approved ? 'Approved' : 'Rejected'}{kind.payload === undefined ? '' : ', with an edited action'}</span>
    {:else if kind.kind === 'agent_log'}
      <code>{kind.line}</code>
    {:else if kind.kind === 'run_finished'}
      <span>Finished{kind.summary === null ? '' : `: ${kind.summary}`}</span>
    {:else if kind.kind === 'child_spawned'}
      <span>Spawned <a href={href({ name: 'run', id: kind.child })}>{kind.agent}</a> with {formatUsd(kind.budget)}</span>
    {:else}
      <span>Capabilities dropped: {[...kind.tools, ...kind.hosts, ...kind.secrets, ...kind.agents].join(', ')}</span>
    {/if}
  </div>
</li>

<style>
  .event {
    display: grid;
    grid-template-columns: auto 5.5rem minmax(0, 1fr);
    align-items: baseline;
    gap: var(--space-3);
    padding: var(--space-2) 0;
    border-bottom: 1px solid var(--color-border);
    font-family: var(--font-mono);
    font-size: 0.875rem;
  }
  time {
    color: var(--color-faint);
    font-variant-numeric: tabular-nums;
  }
  .c-suspended {
    color: var(--color-suspended);
  }
  .c-accent {
    color: var(--color-accent);
  }
  .c-running {
    color: var(--color-running);
  }
  .c-danger {
    color: var(--color-danger);
  }
  .c-done {
    color: var(--color-done);
  }
  .c-approval {
    color: var(--color-approval);
  }
  .c-muted {
    color: var(--color-muted);
  }
  .text {
    overflow-wrap: anywhere;
  }
  .text a {
    margin-left: var(--space-2);
  }
  .text span a {
    margin-left: 0;
  }
  .message {
    margin: 0;
    white-space: pre-wrap;
  }
  /* A fold reads as the event's text, not as a raised button. */
  .fold {
    padding: 0;
    border: 0;
    background: none;
    color: var(--color-text);
    font: inherit;
    text-align: left;
    text-decoration: underline dotted var(--color-faint);
    text-underline-offset: 3px;
  }
  .fold:hover:not(:disabled) {
    background: none;
    color: var(--color-link);
  }
  pre {
    max-height: 24rem;
    overflow: auto;
    margin: var(--space-2) 0 0;
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius);
    background: var(--color-sunken);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  code {
    white-space: pre-wrap;
  }
  @media (max-width: 30rem) {
    .event {
      grid-template-columns: auto 1fr;
      gap: var(--space-1) var(--space-3);
    }
    .text {
      grid-column: 1 / -1;
    }
  }
</style>
