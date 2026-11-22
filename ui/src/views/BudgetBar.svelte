<script lang="ts">
  // The Budget panel: spent of the limit with a meter (warning colour from 80%), the token totals of
  // the run's model calls, what is carved to children and what is left to spend. Money comes in
  // micro-dollar integers.
  import { formatUsd, percentUsed } from '../lib/format';
  import type { Tokens } from '../lib/run';
  import type { MicroUsd } from '../lib/types';

  let {
    spent,
    limit,
    carved,
    tokens,
  }: { spent: MicroUsd; limit: MicroUsd | null; carved: MicroUsd; tokens: Tokens } = $props();

  const percent = $derived(percentUsed(spent, limit));
  const count = (n: number) => n.toLocaleString('en-US');
</script>

<section class="panel budget" aria-labelledby="budget-title">
  <h2 id="budget-title" class="eyebrow">Budget</h2>
  <p class="spent mono">{formatUsd(spent)}</p>
  {#if percent === null}
    <p class="of">no limit</p>
  {:else}
    <p class="of">of {formatUsd(limit)} limit, {percent}% used</p>
    <div
      class="track"
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
  {/if}
  <dl>
    <dt>Input tokens</dt>
    <dd>{count(tokens.input)}</dd>
    <dt>Output tokens</dt>
    <dd>{count(tokens.output)}</dd>
    <dt>Cache reads</dt>
    <dd>{count(tokens.cacheRead)}</dd>
    <dt>Cache writes</dt>
    <dd>{count(tokens.cacheWrite)}</dd>
    <dt>Carved to children</dt>
    <dd>{formatUsd(carved)}</dd>
    <dt>Left to spend</dt>
    <dd>{formatUsd(limit === null ? null : Math.max(0, limit - spent))}</dd>
  </dl>
</section>

<style>
  .spent {
    margin: 0;
    font-size: 2.25rem;
    font-weight: 700;
    line-height: 1.2;
  }
  .of {
    margin: 0 0 var(--space-3);
    color: var(--color-muted);
  }
  .warn {
    --tone: var(--color-approval);
  }
  dl {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: var(--space-1) var(--space-4);
    margin: var(--space-4) 0 0;
  }
  dt {
    color: var(--color-muted);
  }
  dd {
    margin: 0;
    font-family: var(--font-mono);
    text-align: right;
  }
</style>
