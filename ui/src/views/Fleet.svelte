<script lang="ts">
  import { FleetState, type FleetRun } from '../lib/fleet.svelte';
  import type { ActiveRuns } from '../lib/active.svelte';
  import type { ApprovalsState } from '../lib/approvals.svelte';
  import { formatUsd, newestFirst, statusTone } from '../lib/format';
  import { ApiFailure, Unauthorized, type Api } from '../lib/api';
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
  import { Refresher } from '../lib/refresher';
  import RunCard from './RunCard.svelte';

  let {
    api,
    onUnauthorized,
    sockets = browserSocket,
    base = socketBase(window.location),
    approvals,
    liveRuns,
  }: {
    api: Api;
    onUnauthorized: OnUnauthorized;
    sockets?: SocketFactory;
    base?: string;
    /** The pending approvals App.svelte keeps; without it, Needs you counts the runs waiting. */
    approvals?: ApprovalsState;
    /** The live runs App.svelte keeps from its events socket: how a run started later is found. */
    liveRuns?: ActiveRuns;
  } = $props();

  /** Cards per page: the list never holds more at once. */
  const PAGE = 50;
  /** How long a run that ended stays in the active view after it was seen to end. */
  const LINGER_MS = 5_000;
  /** The filter chips. Running, Needs you and Suspended are the status tones of the same names. */
  type Chip = 'all' | 'running' | 'approval' | 'suspended' | 'finished';
  const CHIPS: { key: Chip; label: string }[] = [
    { key: 'all', label: 'All' },
    { key: 'running', label: 'Running' },
    { key: 'approval', label: 'Needs you' },
    { key: 'suspended', label: 'Suspended' },
    { key: 'finished', label: 'Finished' },
  ];
  /** Open tree sockets at most: each one is polled by the server. */
  const MAX_SOCKETS = 50;

  const fleet = new FleetState();
  let loaded = $state(false);
  let error: string | null = $state(null);
  let attempt = $state(0);
  /** The clock for elapsed times on the cards; ticks every second. */
  let now = $state(Date.now());
  /**
   * The clock for what has lingered long enough to leave: it moves only when a linger runs out, so
   * the list and the sockets are not worked out again every second.
   */
  let lingered = $state(Date.now());

  let search = $state('');
  let chip: Chip = $state('all');
  let rootsOnly = $state(true);
  let page = $state(0);
  /** What the pager last announced: a page change, never the list changing under it. */
  let announce = $state('');

  /** Trees whose socket dropped and is not open again yet. */
  let down: Record<RunId, true> = $state({});
  // Bookkeeping only, never rendered: reactive ones would re-run the effects that fill them.
  // eslint-disable-next-line svelte/prefer-svelte-reactivity
  const streams = new Map<RunId, EventStream>();
  /** Trees whose socket is down and whose root was fetched once for this outage. */
  const outage: Record<RunId, true> = {};

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

  // Moves `lingered` when the next run that ended has lingered: re-run when a run ends (or is added).
  $effect(() => {
    let next = Infinity;
    for (const run of Object.values(fleet.runs)) {
      if (run.endedAt !== null && run.endedAt + LINGER_MS > lingered) next = Math.min(next, run.endedAt + LINGER_MS);
    }
    if (next === Infinity) return;
    const timer = setTimeout(() => (lingered = Date.now()), Math.max(0, next - Date.now()));
    return () => clearTimeout(timer);
  });

  // Every fetch of one run goes through here: those the tree sockets ask for (a new run's limit, a
  // parent's after a spawn) and the look-ups of runs the events socket brings. So a new run is
  // fetched once, whichever socket brings it first. A run the server has not answered for yet (not
  // known, or known only from its events) still needs its limit: its failed fetch is retried. A run
  // not known here is retried only while it is live, and never after a 4xx: the server does not
  // have it, or will not show it.
  const refresher = new Refresher({
    fetch: (id) => api.getRun(id),
    answered: (summary) => (fleet.runs[summary.id] ? fleet.update(summary, Date.now()) : fleet.add(summary)),
    failed: (e) => failureText(e, onUnauthorized),
    retry: (id, e) => {
      if (e instanceof Unauthorized) return false;
      const known = fleet.runs[id];
      if (known) return !known.fetched;
      const refused = e instanceof ApiFailure && e.status >= 400 && e.status < 500;
      return !refused && (liveRuns?.ids.includes(id) ?? false);
    },
  });
  const refresh = (id: RunId) => refresher.refresh(id);
  const lookUp = (id: RunId) => refresher.lookUp(id);

  // A run started after the list loaded, in a tree not followed yet, reaches this view only through
  // the app's events socket. Each one not known here is looked up (a failure is tried again by
  // itself), and then shows and is followed like the rest.
  $effect(() => {
    if (!loaded || !liveRuns) return;
    const ids = liveRuns.ids;
    untrack(() => {
      for (const id of ids) if (!fleet.runs[id]) lookUp(id);
    });
  });

  function follow(root: RunId) {
    streams.set(
      root,
      connectEvents({
        base,
        path: `/ws/tree/${encodeURIComponent(root)}`,
        after: 0,
        getToken,
        socketFactory: sockets,
        onEvent: (event) => fleet.apply(event, Date.now(), refresh, root, lookUp),
        onState: (state: ConnectionState) => {
          if (state === 'reconnecting') {
            down[root] = true;
            // A browser reports a token refused at the upgrade as an ordinary failed connection,
            // so a REST call is what finds the 401 (and it refreshes the root's cost and limit).
            // Once per outage: the socket's own retries are enough after that.
            if (!outage[root]) refresh(root);
            outage[root] = true;
          } else if (state !== 'connecting') {
            delete down[root];
            delete outage[root];
          }
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
    refresher.stop();
    for (const root of [...streams.keys()]) unfollow(root);
  });

  const reconnecting = $derived(Object.keys(down).length > 0);
  const children = $derived(fleet.children());

  // The tiles: over every run that has not ended, whatever the filters.
  const active = $derived(Object.values(fleet.runs).filter((run) => run.endedAt === null));
  const onBudget = $derived(
    active.filter((run) => typeof run.status === 'object' && 'Suspended' in run.status && run.status.Suspended === 'Budget')
      .length,
  );
  const spend = $derived(active.reduce((sum, run) => sum + run.spent, 0));
  const needsYou = $derived(approvals ? approvals.count : active.filter((run) => statusTone(run.status) === 'approval').length);
  const subAgents = $derived(active.filter((run) => run.parent !== null).length);

  /**
   * What the approval a run waits on asks, picked by the approval's id in the run's status (a run
   * can have more than one pending); `undefined` when the pending list does not have it.
   */
  function askOf(run: FleetRun): string | undefined {
    const status = run.status;
    if (typeof status !== 'object' || !('Suspended' in status)) return undefined;
    const reason = status.Suspended;
    if (typeof reason !== 'object' || !('Approval' in reason)) return undefined;
    const row = approvals?.rows[reason.Approval];
    return row?.ended === null ? row.approval.action : undefined;
  }

  const needle = $derived(search.trim().toLowerCase());

  function matches(run: FleetRun): boolean {
    if (rootsOnly && run.parent !== null) return false;
    return needle === '' || run.agent.toLowerCase().includes(needle) || run.id.toLowerCase().includes(needle);
  }

  function inChip(run: FleetRun, key: Chip): boolean {
    if (key === 'all') return run.endedAt === null || lingered - run.endedAt < LINGER_MS;
    // However long ago they ended.
    if (key === 'finished') return run.endedAt !== null;
    return statusTone(run.status) === key;
  }

  function firstPage() {
    page = 0;
    announce = '';
  }

  function choose(key: Chip) {
    chip = key;
    firstPage();
  }

  /** The runs the search and Roots only keep: the chips count these. */
  const matching = $derived(Object.values(fleet.runs).filter(matches));
  const counts = $derived(
    Object.fromEntries(CHIPS.map(({ key }) => [key, matching.filter((run) => inChip(run, key)).length])) as Record<
      Chip,
      number
    >,
  );
  const visible = $derived(
    matching.filter((run) => inChip(run, chip)).sort((a, b) => newestFirst(a.created_at, b.created_at)),
  );
  const pages = $derived(Math.max(1, Math.ceil(visible.length / PAGE)));
  const current = $derived(Math.min(page, pages - 1));
  const onPage = $derived(visible.slice(current * PAGE, current * PAGE + PAGE));

  // A page that no longer exists is forgotten, so the view does not jump back to it later.
  $effect(() => {
    if (page > current) {
      page = current;
      announce = '';
    }
  });

  function goTo(next: number) {
    page = Math.min(pages - 1, Math.max(0, next));
    announce = `Page ${page + 1} of ${pages}`;
  }

  // Tree sockets, once the list has loaded: one per tree that is not over (a tree is over once all
  // of it ended and lingered, so its last events still arrive), at most MAX_SOCKETS, the trees on
  // this page first. A tree followed again replays from the start, so it counts its events afresh.
  $effect(() => {
    if (!loaded) return;
    const live = fleet.liveRoots(lingered, LINGER_MS);
    const isLive: Record<RunId, true> = {};
    for (const root of live) isLive[root] = true;
    // The trees on this page first, then the rest, each once.
    const wanted: Record<RunId, true> = {};
    for (const root of [...onPage.map((run) => run.root), ...live]) {
      if (Object.keys(wanted).length >= MAX_SOCKETS) break;
      if (isLive[root]) wanted[root] = true;
    }
    for (const root of [...streams.keys()]) {
      if (!wanted[root]) unfollow(root);
    }
    const fresh = Object.keys(wanted).filter((root) => !streams.has(root));
    untrack(() => {
      // One pass over the runs for all of them, then a socket each.
      if (fresh.length === 0) return;
      fleet.rebuild(fresh);
      for (const root of fresh) follow(root);
    });
  });
</script>

<header class="head">
  <div>
    <h1>Fleet</h1>
    <p class="sub">Every active run on this machine, live.</p>
  </div>
  <label class="visually-hidden" for="fleet-search">Search runs and agents</label>
  <input
    id="fleet-search"
    class="search"
    type="search"
    placeholder="Search runs and agents"
    bind:value={search}
    oninput={firstPage}
  />
</header>

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
      <dl class="tiles">
        <div class="panel tile">
          <dt class="eyebrow">Active runs</dt>
          <dd class="big mono">{active.length}</dd>
          <dd class="small">{onBudget} suspended on budget</dd>
        </div>
        <div class="panel tile">
          <dt class="eyebrow">Spend</dt>
          <dd class="big mono">{formatUsd(spend)}</dd>
          <dd class="small">across {active.length} active runs</dd>
        </div>
        <div class="panel tile">
          <dt class="eyebrow">Needs you</dt>
          <!-- Not 0 before the approvals list has loaded: that would say nothing waits. -->
          <dd class="big mono">
            {#if approvals && !approvals.loaded && approvals.error !== null}<span aria-hidden="true">?</span><span
                class="visually-hidden">approvals could not be loaded</span
              >{:else if approvals && !approvals.loaded}<span aria-hidden="true">...</span><span class="visually-hidden"
                >loading</span
              >{:else}{needsYou}{/if}
          </dd>
          <dd class="small">approvals waiting</dd>
        </div>
        <div class="panel tile">
          <dt class="eyebrow">Sub-agents</dt>
          <dd class="big mono">{subAgents}</dd>
          <dd class="small">live child runs</dd>
        </div>
      </dl>

      <div class="filters" role="group" aria-label="Filters">
        {#each CHIPS as { key, label } (key)}
          <button type="button" class="chip" aria-pressed={chip === key} onclick={() => choose(key)}>
            {label} <span class="count">{counts[key]}</span>
          </button>
        {/each}
        <span class="roots">
          <input id="fleet-roots" type="checkbox" bind:checked={rootsOnly} onchange={firstPage} />
          <label for="fleet-roots">Roots only</label>
        </span>
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
                ask={askOf(run)}
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
          <p class="visually-hidden" role="status">{announce}</p>
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
  .head {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--space-3) var(--space-5);
    margin-bottom: var(--space-5);
  }
  h1 {
    margin: 0;
    font-size: 2rem;
  }
  .sub {
    margin: var(--space-1) 0 0;
    color: var(--color-muted);
  }
  .search {
    flex: 0 1 22rem;
    min-width: 0;
  }
  .tiles {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(12rem, calc(50% - var(--space-4))), 1fr));
    gap: var(--space-4);
    margin: 0 0 var(--space-5);
  }
  @media (max-width: 48rem) {
    .tiles :global(.panel) {
      padding: var(--space-3) var(--space-4);
    }
    .big {
      font-size: 1.5rem;
    }
  }
  .tile dd {
    margin: 0;
  }
  .big {
    font-size: 2rem;
    font-weight: 700;
    line-height: 1.2;
  }
  .small {
    color: var(--color-muted);
    font-size: 0.9rem;
  }
  .filters {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    margin-bottom: var(--space-5);
  }
  .chip {
    border-radius: 999px;
    background: transparent;
  }
  .chip[aria-pressed='true'] {
    border-color: var(--color-accent);
    background: var(--color-accent);
    color: var(--color-on-accent);
  }
  .count {
    margin-left: var(--space-1);
    font-weight: 500;
  }
  .roots {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    margin-left: var(--space-2);
    color: var(--color-muted);
  }
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(min(20rem, 100%), 1fr));
    gap: var(--space-4);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .pager {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin-top: var(--space-4);
    color: var(--color-muted);
  }
</style>
