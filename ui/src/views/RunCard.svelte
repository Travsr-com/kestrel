<script lang="ts">
  import { formatElapsed, formatUsd, statusLabel, statusName } from '../lib/format';
  import type { ChildCount, FleetRun } from '../lib/fleet.svelte';
  import { href } from '../lib/router';

  let {
    run,
    now,
    children,
    parentAgent,
  }: { run: FleetRun; now: number; children?: ChildCount; parentAgent?: string } = $props();

  const plural = (n: number) => (n === 1 ? '1 child' : `${n} children`);

  // A run seen to end shows how long it ran; one that had ended before the view loaded, just that.
  const elapsed = $derived(
    run.endedAt === null
      ? formatElapsed(now - Date.parse(run.created_at))
      : run.endedAt > 0
        ? `ran ${formatElapsed(run.endedAt - Date.parse(run.created_at))}`
        : 'ended',
  );
</script>

<article class="card">
  <!-- The agent link covers the whole card, so a click anywhere on it opens the run. -->
  <!-- Described by its status, so cards of the same agent are told apart in a list of links. -->
  <h2>
    <a class="open" href={href({ name: 'run', id: run.id })} aria-describedby="status-{run.id}">{run.agent}</a>
  </h2>
  <span class="badge status-{statusName(run.status)}" id="status-{run.id}">{statusLabel(run.status)}</span>
  <span class="label">Elapsed</span>
  <span class="elapsed">{elapsed}</span>
  <span class="label">Cost</span>
  <span class="cost">
    {formatUsd(run.spent)} / {run.provisional ? 'limit not known yet' : formatUsd(run.limit)}
  </span>
  <span class="label">Sandbox</span>
  <span class="tier">{run.sandbox_tier ?? 'tier not known yet'}</span>
  {#if run.parent}
    <a class="parent" href={href({ name: 'run', id: run.parent })}>Parent: {parentAgent ?? 'run'}</a>
  {:else if children && children.total > 0}
    <span class="children">{plural(children.total)}, {children.running} running</span>
  {/if}
</article>

<style>
  .card {
    position: relative;
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-1) var(--space-3);
    padding: var(--space-3);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    background: var(--color-surface);
  }
  h2 {
    flex: 1 1 100%;
    margin: 0;
    font-size: 1rem;
  }
  .open::after {
    content: '';
    position: absolute;
    inset: 0;
  }
  .parent {
    position: relative;
    z-index: 1;
  }
  /* Read by screen readers, not shown: the values are clear on screen. */
  .label {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
  .badge {
    padding: 0 var(--space-2);
    border-radius: var(--radius);
    border: 1px solid currentColor;
    font-size: 0.875rem;
  }
  .status-running,
  .status-provisioning {
    color: var(--color-status-running);
  }
  .status-suspended {
    color: var(--color-status-suspended);
  }
  .status-failed {
    color: var(--color-danger);
  }
  .status-queued,
  .status-completed,
  .status-cancelled {
    color: var(--color-muted);
  }
  .elapsed,
  .cost,
  .tier,
  .children {
    color: var(--color-muted);
    font-variant-numeric: tabular-nums;
  }
</style>
