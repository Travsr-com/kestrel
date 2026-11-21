<script lang="ts">
  import type { Api } from '../lib/api';
  import { formatUsd, statusLabel } from '../lib/format';
  import { failureText } from '../lib/load';
  import { href } from '../lib/router';
  import type { RunSummary } from '../lib/types';

  let { api, onUnauthorized }: { api: Api; onUnauthorized: () => void } = $props();

  let runs: RunSummary[] | null = $state(null);
  let error: string | null = $state(null);

  $effect(() => {
    api.listRuns().then(
      (list) => (runs = list),
      (e: unknown) => (error = failureText(e, onUnauthorized)),
    );
  });
</script>

<h1>Fleet</h1>
{#if error !== null}
  <p role="alert" data-testid="ready">{error}</p>
{:else if runs === null}
  <p>Loading runs.</p>
{:else}
  <div data-testid="ready">
    {#if runs.length === 0}
      <p>No runs yet.</p>
    {:else}
      <ul class="runs">
        {#each runs as run (run.id)}
          <li>
            <a href={href({ name: 'run', id: run.id })}>
              <span class="agent">{run.agent}</span>
              <span class="status">{statusLabel(run.status)}</span>
              <span class="spend">{formatUsd(run.spent)} of {formatUsd(run.limit)}</span>
            </a>
          </li>
        {/each}
      </ul>
    {/if}
  </div>
{/if}

<style>
  .runs {
    display: grid;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .runs a {
    display: grid;
    grid-template-columns: 1fr auto auto;
    gap: var(--space-3);
    padding: var(--space-3);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    background: var(--color-surface);
    color: var(--color-text);
    text-decoration: none;
  }
  .agent {
    font-weight: 600;
  }
  .status,
  .spend {
    color: var(--color-muted);
  }
</style>
