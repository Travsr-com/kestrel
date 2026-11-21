<script lang="ts">
  import type { Api } from '../lib/api';
  import { failureText } from '../lib/load';
  import { href } from '../lib/router';
  import type { Approval } from '../lib/types';

  let { api, onUnauthorized }: { api: Api; onUnauthorized: () => void } = $props();

  let pending: Approval[] | null = $state(null);
  let error: string | null = $state(null);

  $effect(() => {
    api.approvals('pending').then(
      (list) => (pending = list),
      (e: unknown) => (error = failureText(e, onUnauthorized)),
    );
  });
</script>

<h1>Approvals</h1>
{#if error !== null}
  <p role="alert" data-testid="ready">{error}</p>
{:else if pending === null}
  <p>Loading approvals.</p>
{:else}
  <div data-testid="ready">
    {#if pending.length === 0}
      <p>No pending approvals.</p>
    {:else}
      <ul class="approvals">
        {#each pending as approval (approval.id)}
          <li>
            <code>{approval.action}</code>
            <a href={href({ name: 'run', id: approval.run_id })}>Run {approval.run_id}</a>
          </li>
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
  li {
    display: grid;
    gap: var(--space-1);
    padding: var(--space-3);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    background: var(--color-surface);
    overflow-wrap: anywhere;
  }
</style>
