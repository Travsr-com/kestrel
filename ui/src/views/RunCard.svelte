<script lang="ts">
  import { formatElapsed, formatUsd, statusLabel, statusName } from '../lib/format';
  import type { ChildCount, FleetRun } from '../lib/fleet.svelte';
  import { href } from '../lib/router';

  let { run, now, children }: { run: FleetRun; now: number; children?: ChildCount } = $props();

  const plural = (n: number) => (n === 1 ? '1 child' : `${n} children`);
</script>

<article class="card">
  <h2><a href={href({ name: 'run', id: run.id })}>{run.agent}</a></h2>
  <span class="badge status-{statusName(run.status)}">{statusLabel(run.status)}</span>
  <span class="elapsed">
    {run.endedAt === null ? formatElapsed(now - Date.parse(run.created_at)) : 'ended'}
  </span>
  <span class="cost">{formatUsd(run.spent)} / {formatUsd(run.limit)}</span>
  <span class="tier">{run.sandbox_tier ?? 'tier not known yet'}</span>
  {#if run.parent}
    <a class="parent" href={href({ name: 'run', id: run.parent })}>Parent run</a>
  {:else if children && children.total > 0}
    <span class="children">{plural(children.total)}, {children.running} running</span>
  {/if}
</article>

<style>
  .card {
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
