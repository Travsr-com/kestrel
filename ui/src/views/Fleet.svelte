<script lang="ts">
  import type { Api } from '../lib/api';
  import { FleetState, type FleetRun } from '../lib/fleet.svelte';
  import { statusName } from '../lib/format';
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
  import { untrack } from 'svelte';
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
  /** Open tree sockets at most: each one is polled by the server. */
  const MAX_SOCKETS = 50;

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

  /** Trees whose socket dropped and is not open again yet. */
  let down: Record<RunId, true> = $state({});
  // Bookkeeping only, never rendered: reactive ones would re-run the effects that fill them.
  // eslint-disable-next-line svelte/prefer-svelte-reactivity
  const streams = new Map<RunId, EventStream>();
  // eslint-disable-next-line svelte/prefer-svelte-reactivity
  const asking = new Set<RunId>();
  // eslint-disable-next-line svelte/prefer-svelte-reactivity
  const askAgain = new Set<RunId>();
  let mounted = true;

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

  /**
   * Fetches a run's limit, tier and cost. One request per run at a time: whatever asks while one is
   * out gets a single request after it, so a replay of many spawns costs two requests, and answers
   * never arrive out of order.
   */
  function refresh(id: RunId) {
    if (asking.has(id)) {
      askAgain.add(id);
      return;
    }
    asking.add(id);
    api
      .getRun(id)
      .then(
        (summary) => mounted && fleet.update(summary, Date.now()),
        // A 401 here is how a refused token shows: a browser reports a socket refused at the
        // upgrade as an ordinary failed connection, so the sockets alone would retry forever.
        (e: unknown) => mounted && failureText(e, onUnauthorized),
      )
      .finally(() => {
        asking.delete(id);
        if (askAgain.delete(id) && mounted) refresh(id);
      });
  }

  function follow(root: RunId) {
    streams.set(
      root,
      connectEvents({
        base,
        path: `/ws/tree/${encodeURIComponent(root)}`,
        after: 0,
        getToken,
        socketFactory: sockets,
        onEvent: (event) => fleet.apply(event, Date.now(), refresh, root),
        onState: (state: ConnectionState) => {
          if (state === 'reconnecting') down[root] = true;
          else if (state !== 'connecting') delete down[root];
          if (state === 'unauthorized') onUnauthorized(getToken());
        },
      }),
    );
  }

  function unfollow(root: RunId) {
    streams.get(root)?.close();
    streams.delete(root);
  }

  $effect(() => () => {
    mounted = false;
    for (const root of [...streams.keys()]) unfollow(root);
  });

  const reconnecting = $derived(Object.keys(down).length > 0);
  const agents = $derived([...new Set(Object.values(fleet.runs).map((r) => r.agent))].sort());
  const children = $derived(fleet.children());
  /** A filter on a finished status shows those runs however long ago they ended. */
  const finishedFilter = $derived(['completed', 'failed', 'cancelled'].includes(status));

  function shown(run: FleetRun): boolean {
    if (rootsOnly && run.parent !== null) return false;
    if (agent && run.agent !== agent) return false;
    if (status && statusName(run.status) !== status) return false;
    if (!showFinished && !finishedFilter && run.endedAt !== null && now - run.endedAt >= LINGER_MS) return false;
    return true;
  }

  const firstPage = () => (page = 0);

  const visible = $derived(
    Object.values(fleet.runs)
      .filter(shown)
      .sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at)),
  );
  const pages = $derived(Math.max(1, Math.ceil(visible.length / PAGE)));
  const current = $derived(Math.min(page, pages - 1));
  const onPage = $derived(visible.slice(current * PAGE, current * PAGE + PAGE));

  // A page that no longer exists is forgotten, so the view does not jump back to it later.
  $effect(() => {
    if (page > current) page = current;
  });

  /** What the pager last announced: a page change, never the list changing under it. */
  let announce = $state('');
  function goTo(next: number) {
    page = Math.min(pages - 1, Math.max(0, next));
    announce = `Page ${page + 1} of ${pages}`;
  }

  // Tree sockets, once the list has loaded: one per tree that is not over (a tree is over once all
  // of it ended and lingered, so its last events still arrive), at most MAX_SOCKETS, the trees on
  // this page first. A tree followed again replays from the start, so it counts its events afresh.
  $effect(() => {
    if (!loaded) return;
    const live = fleet.liveRoots(now, LINGER_MS);
    const first = onPage.map((run) => run.root).filter((root) => live.includes(root));
    const wanted = [...first, ...live].filter((root, i, all) => all.indexOf(root) === i).slice(0, MAX_SOCKETS);
    for (const root of [...streams.keys()]) {
      if (!wanted.includes(root)) unfollow(root);
    }
    untrack(() => {
      for (const root of wanted) {
        if (streams.has(root)) continue;
        fleet.rebuild(root);
        follow(root);
      }
    });
  });
</script>

<h1>Fleet</h1>

<!-- Always in the page, so screen readers announce the text when it appears. -->
<p class:banner={reconnecting} role="status">{reconnecting ? 'Reconnecting to live updates.' : ''}</p>

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
        <select id="fleet-agent" bind:value={agent} onchange={firstPage}>
          <option value="">All</option>
          {#each agents as name (name)}
            <option value={name}>{name}</option>
          {/each}
        </select>
        <label for="fleet-status">Status</label>
        <select id="fleet-status" bind:value={status} onchange={firstPage}>
          <option value="">All</option>
          {#each STATUSES as name (name)}
            <option value={name}>{name[0]?.toUpperCase()}{name.slice(1)}</option>
          {/each}
        </select>
        <input id="fleet-roots" type="checkbox" bind:checked={rootsOnly} onchange={firstPage} />
        <label for="fleet-roots">Roots only</label>
        <input id="fleet-finished" type="checkbox" bind:checked={showFinished} onchange={firstPage} />
        <label for="fleet-finished">Show finished</label>
      </div>

      {#if visible.length === 0}
        <p>No run matches these filters.</p>
      {:else}
        <ul class="cards">
          {#each onPage as run (run.id)}
            <li>
              <RunCard
                {run}
                {now}
                children={children[run.id]}
                parentAgent={run.parent === null ? undefined : fleet.runs[run.parent]?.agent}
              />
            </li>
          {/each}
        </ul>
        <nav class="pager" aria-label="Pages">
          <span>
            Showing {current * PAGE + 1} to {Math.min(visible.length, (current + 1) * PAGE)} of {visible.length}
          </span>
          {#if pages > 1}
            <!-- aria-disabled, not disabled: a disabled button would drop the keyboard focus on it. -->
            <button type="button" aria-disabled={current === 0} onclick={() => goTo(current - 1)}>Previous</button>
            <button type="button" aria-disabled={current >= pages - 1} onclick={() => goTo(current + 1)}>Next</button>
          {/if}
          <p class="hidden" role="status">{announce}</p>
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
  .hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
  button[aria-disabled='true'] {
    opacity: 0.5;
    cursor: default;
  }
</style>
