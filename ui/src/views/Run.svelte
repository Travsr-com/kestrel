<script lang="ts">
  import { ApiFailure, type Api } from '../lib/api';
  import { formatUsd, statusLabel } from '../lib/format';
  import { failureText } from '../lib/load';
  import { href } from '../lib/router';
  import type { RunSummary } from '../lib/types';

  let { id, api, onUnauthorized }: { id: string; api: Api; onUnauthorized: () => void } = $props();

  let run: RunSummary | null = $state(null);
  let missing = $state(false);
  let error: string | null = $state(null);

  $effect(() => {
    api.getRun(id).then(
      (r) => (run = r),
      (e: unknown) => {
        if (e instanceof ApiFailure && e.status === 404) missing = true;
        else error = failureText(e, onUnauthorized);
      },
    );
  });
</script>

<h1>Run {id}</h1>
{#if error !== null}
  <p role="alert" data-testid="ready">{error}</p>
{:else if missing}
  <p data-testid="ready">No run has this id. <a href={href({ name: 'fleet' })}>Back to Fleet</a></p>
{:else if run === null}
  <p>Loading run.</p>
{:else}
  <dl class="facts" data-testid="ready">
    <dt>Agent</dt>
    <dd>{run.agent}</dd>
    <dt>Status</dt>
    <dd>{statusLabel(run.status)}</dd>
    <dt>Spent</dt>
    <dd>{formatUsd(run.spent)} of {formatUsd(run.limit)}</dd>
    {#if run.parent !== null}
      <dt>Parent</dt>
      <dd><a href={href({ name: 'run', id: run.parent })}>{run.parent}</a></dd>
    {/if}
  </dl>
{/if}

<style>
  h1 {
    overflow-wrap: anywhere;
  }
  .facts {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: var(--space-2) var(--space-4);
  }
  dt {
    color: var(--color-muted);
  }
  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
</style>
