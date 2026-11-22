<script lang="ts">
  // One pending approval in the list: a button that selects it, with who asks, what, and how long
  // it has waited, or how its decision is going. Deciding happens in the detail panel.
  import type { ApprovalRow, DecisionState } from '../lib/approvals.svelte';
  import { firstLine, formatElapsed } from '../lib/format';
  import type { RunSummary } from '../lib/types';

  let {
    row,
    run,
    now,
    selected,
    decision = null,
    onSelect,
  }: {
    row: ApprovalRow;
    run: RunSummary | undefined;
    now: number;
    selected: boolean;
    /** How a decision sent from its panel is going: said here too, as the panel may be hidden. */
    decision?: DecisionState;
    onSelect: () => void;
  } = $props();

  const DECISION: Record<NonNullable<DecisionState>, string> = {
    approve: 'Approving',
    reject: 'Rejecting',
    answer: 'Sending answer',
    failed: 'Failed, open it to retry',
  };

  /** Once its run ended, that is what the item says, whatever was being sent. */
  const shown = $derived(row.ended === null ? decision : null);
  const approval = $derived(row.approval);
  const waited = $derived(now - Date.parse(approval.requested_at));
  const kind = $derived(approval.question ? 'Question' : 'Command');
  const state = $derived(
    row.ended === null ? `waiting ${formatElapsed(waited)}` : row.ended === 'cancelled' ? 'run cancelled' : 'run ended',
  );

  /** The wait in the corner: whole minutes once past one, hours once past sixty. */
  function short(ms: number): string {
    const seconds = Math.max(0, Math.floor(ms / 1000));
    if (seconds < 60) return `${seconds}s`;
    const minutes = Math.floor(seconds / 60);
    return minutes < 60 ? `${minutes} min` : `${Math.floor(minutes / 60)} h`;
  }
</script>

<li data-id={approval.id}>
  <button type="button" class="item" class:selected aria-pressed={selected} onclick={onSelect}>
    <span class="top">
      <strong class="agent">{run?.agent ?? 'agent not known yet'}</strong>
      <!-- The muted line says the wait in words; this is the same at a glance. -->
      {#if row.ended === null}
        <span class="wait" aria-hidden="true">
          <svg viewBox="0 0 10 10" width="10" height="10"><rect x="2" y="1.5" width="2" height="7" /><rect x="6" y="1.5" width="2" height="7" /></svg>
          {short(waited)}
        </span>
      {/if}
    </span>
    <!-- Also the button's accessible name, so only the first line, capped. -->
    <span class="action">{firstLine(approval.action)}</span>
    <span class="meta" class:failed={shown === 'failed'}>{shown === null ? `${kind}, ${state}` : DECISION[shown]}</span>
  </button>
</li>

<style>
  li {
    display: block;
  }
  .item {
    display: grid;
    gap: var(--space-1);
    width: 100%;
    padding: var(--space-3) var(--space-4);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-lg);
    background: var(--color-surface);
    font-weight: 400;
    text-align: left;
  }
  .item.selected {
    border-color: var(--color-approval-border);
    background: var(--color-approval-soft);
  }
  .item.selected:hover:not(:disabled) {
    background: var(--color-approval-soft);
  }
  .top {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .agent {
    font-weight: 700;
    overflow-wrap: anywhere;
  }
  .wait {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    flex: none;
    color: var(--color-approval);
    font-size: 0.9rem;
  }
  .wait svg {
    fill: currentColor;
  }
  .action {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .meta {
    color: var(--color-muted);
    font-size: 0.9rem;
  }
  .meta.failed {
    color: var(--color-danger);
  }
</style>
