<script lang="ts">
  import type { Api } from '../lib/api';
  import { FleetState, type FleetRun } from '../lib/fleet.svelte';
  import { isTerminal, statusName } from '../lib/format';
  import { failureText, whileMounted, type OnUnauthorized } from '../lib/load';
  import { getToken } from '../lib/token';
  import type { RunId } from '../lib/types';
  import {
    browserSocket,
    connectEvents,
    socketBase,
    type ConnectionState,
    type EventStream,
    type SocketFactory,
  } from '../lib/ws';
  import RunCard from './RunCard.svelte';

  let {
    api,
    onUnauthorized,
    sockets = browserSocket,
    base = socketBase(window.location),
  }: {
    api: Api;
    onUnauthorized: OnUnauthorized;
    sockets?: SocketFactory;
    base?: string;
  } = $props();

  /** Cards per page: the list never holds more at once. */
  const PAGE = 50;
  /** How long a run that ended stays in the active view after it was seen to end. */
  const LINGER_MS = 5_000;
  const STATUSES = ['queued', 'provisioning', 'running', 'suspended', 'completed', 'failed', 'cancelled'];

  const fleet = new FleetState();
  let loaded = $state(false);
  let error: string | null = $state(null);
  let attempt = $state(0);
  let now = $state(Date.now());

  let agent = $state('');
  let status = $state('');
  let rootsOnly = $state(true);
  let showFinished = $state(false);
  let page = $state(0);

  /** The connection state of each root's tree socket. */
  let connections: Record<RunId, ConnectionState> = $state({});
  // Bookkeeping only, never rendered: a reactive map would re-run the effect that fills it.
  // eslint-disable-next-line svelte/prefer-svelte-reactivity
  const streams = new Map<RunId, EventStream>();

  $effect(() => {
    // Read so that Retry, which bumps it, loads again.
    void attempt;
    return whileMounted(
      () => api.listRuns(),
      (list) => {
        fleet.load(list);
        error = null;
        loaded = true;
      },
      (e) => (error = failureText(e, onUnauthorized)),
    );
  });

  $effect(() => {
    const tick = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(tick);
  });

  /** Fetches a run the view needs: a new one is added, a known one gets its fresh limit. */
  function refresh(id: RunId) {
    api.getRun(id).then(
      (summary) => (fleet.runs[id] ? fleet.setLimit(id, summary.limit) : fleet.add(summary)),
      () => {},
    );
  }

  function follow(root: RunId) {
    let cursor = 0;
    streams.set(
      root,
      connectEvents({
        base,
        path: `/ws/tree/${encodeURIComponent(root)}`,
        after: 0,
        getToken,
        socketFactory: sockets,
        onEvent: (event, at) => {
          cursor = at;
          fleet.apply(event, Date.now(), refresh);
        },
        onState: (state) => {
          connections[root] = state;
          // Only a replay from the start rebuilds costs; a reconnect resumes from its cursor.
          if (state === 'open' && cursor === 0) fleet.rebuild(root);
        },
      }),
    );
  }

  // One tree socket per live root, opened once the list has loaded.
  $effect(() => {
    if (!loaded) return;
    for (const run of Object.values(fleet.runs)) {
      if (run.parent === null && !isTerminal(run.status) && !streams.has(run.id)) follow(run.id);
    }
  });

  $effect(() => () => {
    for (const stream of streams.values()) stream.close();
    streams.clear();
  });

  const reconnecting = $derived(Object.values(connections).includes('reconnecting'));
  const agents = $derived([...new Set(Object.values(fleet.runs).map((r) => r.agent))].sort());
  const children = $derived(fleet.children());

  function shown(run: FleetRun): boolean {
    if (rootsOnly && run.parent !== null) return false;
    if (agent && run.agent !== agent) return false;
    if (status && statusName(run.status) !== status) return false;
    if (!showFinished && run.endedAt !== null && now - run.endedAt >= LINGER_MS) return false;
    return true;
  }

  const visible = $derived(
    Object.values(fleet.runs)
      .filter(shown)
      .sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at)),
  );
  const pages = $derived(Math.max(1, Math.ceil(visible.length / PAGE)));
  const current = $derived(Math.min(page, pages - 1));
  const onPage = $derived(visible.slice(current * PAGE, current * PAGE + PAGE));
</script>

<h1>Fleet</h1>

{#if reconnecting}
  <p class="banner" role="status">Reconnecting to live updates.</p>
{/if}

{#if error !== null}
  <div class="banner error" role="alert" data-testid="ready">
    <p>{error}</p>
    <button type="button" onclick={() => (attempt += 1)}>Retry</button>
  </div>
{:else if !loaded}
  <p>Loading runs.</p>
{:else}
  <div data-testid="ready">
    {#if Object.keys(fleet.runs).length === 0}
      <p>No runs yet.</p>
      <p>Start one with <code>kestrel run &lt;agent.yaml&gt;</code>.</p>
    {:else}
      <div class="filters" role="group" aria-label="Filters">
        <label for="fleet-agent">Agent</label>
        <select id="fleet-agent" bind:value={agent}>
          <option value="">All</option>
          {#each agents as name (name)}
            <option value={name}>{name}</option>
          {/each}
        </select>
        <label for="fleet-status">Status</label>
        <select id="fleet-status" bind:value={status}>
          <option value="">All</option>
          {#each STATUSES as name (name)}
            <option value={name}>{name}</option>
          {/each}
        </select>
        <input id="fleet-roots" type="checkbox" bind:checked={rootsOnly} />
        <label for="fleet-roots">Roots only</label>
        <input id="fleet-finished" type="checkbox" bind:checked={showFinished} />
        <label for="fleet-finished">Show finished</label>
      </div>

      {#if visible.length === 0}
        <p>No run matches these filters.</p>
      {:else}
        <ul class="cards">
          {#each onPage as run (run.id)}
            <li><RunCard {run} {now} children={children[run.id]} /></li>
          {/each}
        </ul>
        <nav class="pager" aria-label="Pages">
          <span>
            Showing {current * PAGE + 1} to {Math.min(visible.length, (current + 1) * PAGE)} of {visible.length}
          </span>
          {#if pages > 1}
            <button type="button" disabled={current === 0} onclick={() => (page = current - 1)}>Previous</button>
            <button type="button" disabled={current >= pages - 1} onclick={() => (page = current + 1)}>Next</button>
          {/if}
        </nav>
      {/if}
    {/if}
  </div>
{/if}

<style>
  .banner {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    background: var(--color-surface);
  }
  .error {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    border-color: var(--color-danger);
  }
  .error p {
    margin: 0;
  }
  .filters {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    margin-bottom: var(--space-3);
  }
  .cards {
    display: grid;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .pager {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin-top: var(--space-3);
    color: var(--color-muted);
  }
  button,
  select {
    font: inherit;
  }
</style>
