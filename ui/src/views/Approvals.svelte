<script lang="ts">
  // Pending approvals across all runs, newest first: a list to pick one from and the detail panel
  // of the one picked, with enough context to decide without opening the run. Under the list, the
  // approvals decided most recently. The pending list and its live updates come from the app
  // (`ApprovalsState`), which also counts them in the nav; this view adds each run's agent and root.
  import { tick } from 'svelte';
  import type { Api } from '../lib/api';
  import type { ApprovalsState } from '../lib/approvals.svelte';
  import { newestFirst } from '../lib/format';
  import { failureText, type OnUnauthorized } from '../lib/load';
  import type { Approval, ApprovalId, RunId, RunSummary } from '../lib/types';
  import ApprovalItem from './ApprovalItem.svelte';
  import ApprovalRow from './ApprovalRow.svelte';
  import DecidedList from './DecidedList.svelte';

  let {
    approvals,
    api,
    onUnauthorized,
    onRetry,
  }: { approvals: ApprovalsState; api: Api; onUnauthorized: OnUnauthorized; onRetry: () => void } = $props();

  /** How many decided approvals are shown. */
  const DECIDED = 10;

  let now = $state(Date.now());
  $effect(() => {
    const tick = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(tick);
  });

  /** The recently decided approvals, newest first; `null` until loaded. */
  let decided: Approval[] | null = $state(null);
  let decidedFailed = $state(false);
  $effect(() => {
    let alive = true;
    Promise.all([api.approvals('approved'), api.approvals('rejected'), api.approvals('expired')]).then(
      (lists) => {
        if (!alive) return;
        // By id, so an approval whose status changed between the requests is listed once.
        const byId = new Map(lists.flat().map((a) => [a.id, a]));
        decided = [...byId.values()].sort((a, b) => newestFirst(a.requested_at, b.requested_at)).slice(0, DECIDED);
      },
      (e: unknown) => {
        if (!alive) return;
        // A refused token is handed on; the pending list asked at the same time says the rest.
        failureText(e, onUnauthorized);
        decidedFailed = true;
      },
    );
    return () => {
      alive = false;
    };
  });

  /** Each approval's run, for its agent, root and budget. */
  let runs: Record<RunId, RunSummary> = $state({});
  /** Runs asked for already: bookkeeping only, never rendered. */
  const asked: Record<RunId, true> = {};
  /** Bumped a while after a run's details failed to load, so they are asked for again. */
  let again = $state(0);
  const timers: ReturnType<typeof setTimeout>[] = [];
  $effect(() => () => timers.forEach(clearTimeout));
  $effect(() => {
    void again;
    const wanted = [...Object.values(approvals.rows).map((row) => row.approval), ...(decided ?? [])];
    for (const { run_id: id } of wanted) {
      if (asked[id]) continue;
      asked[id] = true;
      api.getRun(id).then(
        (summary) => {
          runs[id] = summary;
        },
        () => {
          delete asked[id];
          timers.push(setTimeout(() => (again += 1), 5_000));
        },
      );
    }
  });

  /** Said after an approval is taken away because someone else decided it first. */
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

  // The selected approval. The newest is selected at first; a live-added one never takes the
  // selection; when the selected one goes away, the one now at its place (or the last) is.
  let chosen: ApprovalId | null = $state(null);
  /** Where `chosen` was in the list last seen. */
  let chosenAt = 0;
  $effect.pre(() => {
    const at = list.findIndex((row) => row.approval.id === chosen);
    if (at >= 0) {
      chosenAt = at;
      return;
    }
    chosen = list[Math.min(chosenAt, list.length - 1)]?.approval.id ?? null;
  });

  // When the focused element goes away with its approval (decided, dismissed, resolved elsewhere),
  // focus moves to the selected item, or the item now at the place of the one focused, or to the
  // heading when none is left, instead of falling to the top of the page.
  let ul: HTMLUListElement | undefined = $state();
  let details: HTMLElement | undefined = $state();
  let heading: HTMLHeadingElement | undefined = $state();
  let focusedAt = -1;
  let focusedDetail = false;
  $effect.pre(() => {
    void list;
    const active = document.activeElement;
    const li = active instanceof HTMLElement ? active.closest('li[data-id]') : null;
    focusedAt = li && ul?.contains(li) ? Array.from(ul.children).indexOf(li) : -1;
    focusedDetail = details?.contains(active) ?? false;
  });
  $effect(() => {
    void list;
    void chosen;
    if (focusedAt < 0 && !focusedDetail) return;
    const at = focusedDetail ? list.findIndex((row) => row.approval.id === chosen) : focusedAt;
    focusedAt = -1;
    focusedDetail = false;
    const active = document.activeElement;
    if (active && active !== document.body && document.contains(active)) return;
    const items = ul ? Array.from(ul.children) : [];
    const target = items[Math.min(at, items.length - 1)];
    (target?.querySelector<HTMLElement>('button') ?? heading)?.focus();
  });
</script>

<header>
  <h1 tabindex="-1" bind:this={heading}>Approvals</h1>
  <p class="lede">Agents stop and ask before risky steps. Nothing runs until you decide.</p>
</header>

<!-- Always in the page (hidden while empty), so screen readers announce what appears in it. -->
<p class="notice" class:visually-hidden={notice === ''} role="status">{notice}</p>

{#if approvals.error !== null && !approvals.loaded}
  <div class="banner" role="alert" data-testid="ready">
    <p>{approvals.error}</p>
    <button type="button" onclick={onRetry}>Retry</button>
  </div>
{:else if !approvals.loaded}
  <p>Loading approvals.</p>
{:else}
  <div class="layout" data-testid="ready">
    <div class="pending">
      <h2 class="eyebrow">Pending ({list.length})</h2>
      {#if list.length === 0}
        <p class="muted">Nothing waiting for you</p>
      {:else}
        <ul class="items" aria-label="Pending approvals" bind:this={ul}>
          {#each list as row (row.approval.id)}
            <ApprovalItem
              {row}
              run={runs[row.approval.run_id]}
              {now}
              selected={row.approval.id === chosen}
              onSelect={() => (chosen = row.approval.id)}
            />
          {/each}
        </ul>
      {/if}
    </div>

    {#if list.length > 0}
      <div class="details" bind:this={details}>
        {#each list as row (row.approval.id)}
          <ApprovalRow
            {row}
            run={runs[row.approval.run_id]}
            {now}
            selected={row.approval.id === chosen}
            {api}
            {onUnauthorized}
            onGone={(why) => gone(row.approval.id, why)}
            onDismiss={() => approvals.dismiss(row.approval.id)}
          />
        {/each}
      </div>
    {/if}

    <div class="decided">
      <DecidedList {decided} failed={decidedFailed} {runs} />
    </div>
  </div>
{/if}

<style>
  header {
    margin-bottom: var(--space-5);
  }
  h1 {
    margin: 0;
    font-size: 2rem;
  }
  h1:focus {
    outline: none;
  }
  .lede {
    margin: var(--space-1) 0 0;
    color: var(--color-muted);
  }
  .notice {
    margin: 0 0 var(--space-4);
  }
  /* One column on narrow screens: pending, then the detail, then what was decided. */
  .layout {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    grid-template-areas: 'pending' 'details' 'decided';
    gap: var(--space-5);
    align-items: start;
  }
  @media (min-width: 60rem) {
    .layout {
      grid-template-columns: minmax(0, 2fr) minmax(0, 3fr);
      grid-template-rows: auto 1fr;
      grid-template-areas: 'pending details' 'decided details';
    }
  }
  .pending {
    grid-area: pending;
  }
  .details {
    grid-area: details;
  }
  .decided {
    grid-area: decided;
  }
  .items {
    display: grid;
    gap: var(--space-3);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .muted {
    margin: 0;
    color: var(--color-muted);
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
</style>
