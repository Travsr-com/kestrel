<script lang="ts">
  // Spent over limit as a meter (warning style from 80%), with what is carved to children and what
  // is left to spend, all from micro-dollar integers.
  import { formatUsd } from '../lib/format';
  import type { MicroUsd } from '../lib/types';

  let { spent, limit, carved }: { spent: MicroUsd; limit: MicroUsd | null; carved: MicroUsd } = $props();

  const percent = $derived(limit === null ? 0 : limit === 0 ? 100 : Math.min(100, Math.floor((spent * 100) / limit)));
</script>

<div class="budget">
  {#if limit === null}
    <p>{formatUsd(spent)} spent, no limit</p>
  {:else}
    <div
      class="meter"
      class:warn={percent >= 80}
      role="meter"
      aria-label="Budget used"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={percent}
      aria-valuetext="{percent}%"
    >
      <div class="fill" style:width="{percent}%"></div>
    </div>
    <p>{formatUsd(spent)} of {formatUsd(limit)}, {formatUsd(carved)} carved to children</p>
    <p>{formatUsd(Math.max(0, limit - spent))} left to spend</p>
  {/if}
</div>

<style>
  .budget p {
    margin: var(--space-1) 0 0;
    font-variant-numeric: tabular-nums;
  }
  .meter {
    height: 0.5rem;
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    background: var(--color-surface);
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: var(--color-status-running);
  }
  .warn .fill {
    background: var(--color-status-suspended);
  }
</style>
