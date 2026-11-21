<script lang="ts">
  // Pending approvals across all runs, newest first, with enough context to decide without
  // opening the run. The list and its live updates come from the app (`ApprovalsState`), which
  // also counts them in the nav; this view adds each run's agent and root.
  import type { Api } from '../lib/api';
  import type { ApprovalsState } from '../lib/approvals.svelte';
  import { newestFirst } from '../lib/format';
  import type { OnUnauthorized } from '../lib/load';
  import type { RunId, RunSummary } from '../lib/types';
  import ApprovalRow from './ApprovalRow.svelte';

  let {
    approvals,
    api,
    onUnauthorized,
    onRetry,
  }: { approvals: ApprovalsState; api: Api; onUnauthorized: OnUnauthorized; onRetry: () => void } = $props();

  let now = $state(Date.now());
  $effect(() => {
    const tick = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(tick);
  });

  /** Each approval's run, for its agent and root. */
  let runs: Record<RunId, RunSummary> = $state({});
  /** Runs asked for already: bookkeeping only, never rendered. */
  const asked: Record<RunId, true> = {};
  let mounted = true;
  $effect(() => () => {
    mounted = false;
  });
  $effect(() => {
    for (const row of Object.values(approvals.rows)) {
      const id = row.approval.run_id;
      if (asked[id]) continue;
      asked[id] = true;
      api.getRun(id).then(
        (summary) => {
          if (mounted) runs[id] = summary;
        },
        () => {},
      );
    }
  });

  /** Said once after a row is taken away because someone else decided it first. */
  let notice = $state('');

  const list = $derived(
    Object.values(approvals.rows).sort((a, b) => newestFirst(a.approval.requested_at, b.approval.requested_at)),
  );
</script>

<h1>Approvals</h1>

<p class="notice" role="status">{notice}</p>

{#if approvals.error !== null && !approvals.loaded}
  <div class="banner" role="alert" data-testid="ready">
    <p>{approvals.error}</p>
    <button type="button" onclick={onRetry}>Retry</button>
  </div>
{:else if !approvals.loaded}
  <p>Loading approvals.</p>
{:else}
  <div data-testid="ready">
    {#if list.length === 0}
      <p>Nothing waiting for you</p>
    {:else}
      <ul class="approvals" aria-label="Pending approvals">
        {#each list as row (row.approval.id)}
          <ApprovalRow
            {row}
            run={runs[row.approval.run_id]}
            {now}
            {api}
            {onUnauthorized}
            onGone={() => {
              approvals.remove(row.approval.id);
              notice = 'Already resolved by someone else';
            }}
            onDismiss={() => approvals.dismiss(row.approval.id)}
          />
        {/each}
      </ul>
    {/if}
  </div>
{/if}

<style>
  .approvals {
    display: grid;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .notice:empty {
    display: none;
  }
  .banner {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-danger);
    border-radius: var(--radius);
  }
  .banner p {
    margin: 0;
  }
  button {
    font: inherit;
  }
</style>
