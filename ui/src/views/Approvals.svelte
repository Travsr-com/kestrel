<script lang="ts">
  // Pending approvals across all runs, newest first, with enough context to decide without
  // opening the run. The list and its live updates come from the app (`ApprovalsState`), which
  // also counts them in the nav; this view adds each run's agent and root.
  import { tick } from 'svelte';
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
  /** Bumped a while after a run's details failed to load, so they are asked for again. */
  let again = $state(0);
  const timers: ReturnType<typeof setTimeout>[] = [];
  $effect(() => () => timers.forEach(clearTimeout));
  $effect(() => {
    void again;
    for (const row of Object.values(approvals.rows)) {
      const id = row.approval.run_id;
      if (asked[id]) continue;
      asked[id] = true;
      api.getRun(id).then(
        (summary) => (runs[id] = summary),
        () => {
          delete asked[id];
          timers.push(setTimeout(() => (again += 1), 5_000));
        },
      );
    }
  });

  /** Said after a row is taken away because someone else decided it first. */
  let notice = $state('');

  /** Clears the notice first, so the same words are announced again. */
  async function say(text: string) {
    notice = '';
    await tick();
    notice = text;
  }

  function gone(id: string, why: 'resolved' | 'not-waiting') {
    if (why === 'not-waiting') {
      approvals.endRow(id);
      return;
    }
    approvals.remove(id);
    void say('Already resolved by someone else');
  }

  const list = $derived(
    Object.values(approvals.rows).sort((a, b) => newestFirst(a.approval.requested_at, b.approval.requested_at)),
  );

  // When the focused row goes away (decided, dismissed), focus moves to the row now in its place,
  // or to the heading when none is left, instead of falling to the top of the page.
  let ul: HTMLUListElement | undefined = $state();
  let heading: HTMLHeadingElement | undefined = $state();
  let focusedAt = -1;
  $effect.pre(() => {
    void list;
    const active = document.activeElement;
    const li = active instanceof HTMLElement ? active.closest('li[data-id]') : null;
    focusedAt = li && ul?.contains(li) ? Array.from(ul.children).indexOf(li) : -1;
  });
  $effect(() => {
    void list;
    if (focusedAt < 0) return;
    const at = focusedAt;
    focusedAt = -1;
    const active = document.activeElement;
    if (active && active !== document.body && document.contains(active)) return;
    const items = ul ? Array.from(ul.children) : [];
    const target = items[Math.min(at, items.length - 1)];
    (target?.querySelector<HTMLElement>('button') ?? heading)?.focus();
  });
</script>

<h1 tabindex="-1" bind:this={heading}>Approvals</h1>

<!-- Always in the page (hidden while empty), so screen readers announce what appears in it. -->
<p class="notice" class:hidden={notice === ''} role="status">{notice}</p>

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
      <ul class="approvals" aria-label="Pending approvals" bind:this={ul}>
        {#each list as row (row.approval.id)}
          <ApprovalRow
            {row}
            run={runs[row.approval.run_id]}
            {now}
            {api}
            {onUnauthorized}
            onGone={(why) => gone(row.approval.id, why)}
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
  h1:focus {
    outline: none;
  }
  .hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
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
