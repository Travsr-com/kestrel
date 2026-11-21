<script lang="ts">
  // One event in a run's stream. Tool calls and results start collapsed: a collapsed one renders
  // only its summary, so a large output costs nothing until it is opened.
  import { formatUsd, statusLabel } from '../lib/format';
  import { href } from '../lib/router';
  import { formatSize } from '../lib/run';
  import type { Event } from '../lib/types';

  let { event }: { event: Event } = $props();

  let open = $state(false);
  const kind = $derived(event.kind);
  const time = $derived(event.ts.slice(11, 19));
  const bytes = (text: string) => new TextEncoder().encode(text).length;
</script>

<li class="event">
  <time datetime={event.ts}>{time}</time>
  {#if kind.kind === 'run_created'}
    <span>Run created: {kind.agent}</span>
  {:else if kind.kind === 'status_changed'}
    <span>Status changed from {statusLabel(kind.from)} to {statusLabel(kind.to)}</span>
  {:else if kind.kind === 'sandbox_provisioned'}
    <span>Sandbox ready: {kind.driver} ({kind.sandbox_id})</span>
  {:else if kind.kind === 'agent_message'}
    <p class="message">{kind.text}</p>
  {:else if kind.kind === 'tool_call'}
    <div class="fold">
      <button type="button" aria-expanded={open} onclick={() => (open = !open)}>Tool call: {kind.name}</button>
      {#if open}
        <pre>{JSON.stringify(kind.input, null, 2)}</pre>
      {/if}
    </div>
  {:else if kind.kind === 'tool_result'}
    <div class="fold">
      <button type="button" aria-expanded={open} onclick={() => (open = !open)}>
        Tool result: {kind.name}, {kind.ok ? 'ok' : 'failed'}, {formatSize(bytes(kind.output))}
      </button>
      {#if open}
        <pre>{kind.output}</pre>
      {/if}
    </div>
  {:else if kind.kind === 'model_call'}
    <span>
      Model call: {kind.model}, {kind.tokens_in} in, {kind.tokens_out} out, {kind.cost === null
        ? 'cost not known'
        : formatUsd(kind.cost)}
    </span>
  {:else if kind.kind === 'budget_warning'}
    <span class="warn">Budget warning: {formatUsd(kind.spent)} of {formatUsd(kind.limit)}</span>
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
</li>

<style>
  .event {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-1) var(--space-2);
    padding: var(--space-1) 0;
    border-bottom: 1px solid var(--color-border);
  }
  time {
    color: var(--color-muted);
    font-variant-numeric: tabular-nums;
  }
  .message {
    flex: 1 1 100%;
    margin: 0;
    white-space: pre-wrap;
  }
  .fold {
    flex: 1 1 100%;
  }
  button {
    font: inherit;
  }
  pre {
    max-height: 24rem;
    overflow: auto;
    margin: var(--space-1) 0 0;
    padding: var(--space-2);
    background: var(--color-surface);
    border-radius: var(--radius);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  code {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .warn {
    color: var(--color-status-suspended);
  }
</style>
