<script lang="ts">
  // One run in detail, and its tree. Header: breadcrumb, agent, status pill, Pause or Resume and
  // Cancel run. Left column: the spawn tree, the budget, Change limit and Message. Right column:
  // the run's live event stream.
  //
  // The tree comes from `GET /api/runs?root=` and a tree socket that replays the tree and stays
  // open, so new children, statuses and costs show live (the same state the Fleet view keeps). The
  // stream comes from the run's own socket, replayed from seq 0; a reconnect resumes after the
  // last seq. Selecting another run of the same tree changes only the stream: the view stays
  // mounted, so the tree keeps what is collapsed.
  import { untrack } from 'svelte';
  import { ApiFailure, Unauthorized, type Api } from '../lib/api';
  import { FleetState } from '../lib/fleet.svelte';
  import { statusPill, statusTone } from '../lib/format';
  import { failureText, whileMounted, type OnUnauthorized } from '../lib/load';
  import { Refresher } from '../lib/refresher';
  import { href } from '../lib/router';
  import { addTokens, carved, liveBelow, NO_TOKENS, type Tokens } from '../lib/run';
  import { getToken } from '../lib/token';
  import type { Event, RunId, RunStatus } from '../lib/types';
  import { browserSocket, connectEvents, socketBase, type ConnectionState, type SocketFactory } from '../lib/ws';
  import BudgetBar from './BudgetBar.svelte';
  import Controls from './Controls.svelte';
  import EventItem from './EventItem.svelte';
  import SpawnTree from './SpawnTree.svelte';

  let {
    id,
    api,
    onUnauthorized,
    sockets = browserSocket,
    base = socketBase(window.location),
  }: { id: string; api: Api; onUnauthorized: OnUnauthorized; sockets?: SocketFactory; base?: string } = $props();

  const fleet = new FleetState();
  /** The root of the tree on screen, once loaded. */
  let root: RunId | null = $state(null);
  /** The run whose stream is on screen, once the server confirmed it exists. */
  let shown: RunId | null = $state(null);
  let missing = $state(false);
  let error: string | null = $state(null);
  /** Bumped by Retry, so the run loads again. */
  let attempt = $state(0);
  // Raw: events are never changed in place, so they need no deep proxy (outputs can be large).
  let events: Event[] = $state.raw([]);
  /** Events that arrived since the last render; added in one go. */
  let pending: Event[] = [];
  /** The tokens of the stream's model calls, kept up to date with each batch of new events. */
  let tokens: Tokens = $state.raw(NO_TOKENS);
  /** Sockets that dropped and are not open again yet. */
  let down: Record<string, true> = $state({});
  /** Sockets that have not opened yet since they were made. */
  let opening: Record<string, true> = $state({});
  /** A socket dropped since the run on screen was shown: its recovery is announced. */
  let dropped = $state(false);

  let box: HTMLElement | undefined = $state();
  /** The stream follows new events while the user has not scrolled up. */
  let stick = $state(true);

  const refresher = new Refresher({
    fetch: (rid) => api.getRun(rid),
    answered: (summary) => fleet.update(summary, Date.now()),
    failed: (e) => failureText(e, onUnauthorized),
    retry: (rid, e) => !(e instanceof Unauthorized) && fleet.runs[rid]?.fetched === false,
  });
  const refresh = (rid: RunId) => refresher.refresh(rid);
  $effect(() => () => refresher.stop());

  // The run, and its tree when it is another tree than the one on screen.
  $effect(() => {
    const want = id;
    void attempt;
    const known = untrack(() => root);
    missing = false;
    error = null;
    return whileMounted(
      async () => {
        const summary = await api.getRun(want);
        const list = summary.root === known ? null : await api.listRuns({ root: summary.root });
        return { summary, list };
      },
      ({ summary, list }) => {
        if (list !== null) {
          const tree = list.filter((r) => r.root === summary.root);
          if (!tree.some((r) => r.id === summary.id)) tree.push(summary);
          fleet.load(tree);
          root = summary.root;
        } else if (fleet.runs[summary.id]) {
          fleet.update(summary, Date.now());
        } else {
          // A run of this tree that the tree socket has not delivered yet.
          fleet.add(summary);
        }
        shown = summary.id;
      },
      (e) => {
        // Nothing of the run shown before stays on screen, and its sockets close.
        shown = null;
        root = null;
        if (e instanceof ApiFailure && e.status === 404) missing = true;
        else error = failureText(e, onUnauthorized);
      },
    );
  });

  function track(name: string, state: ConnectionState) {
    if (state === 'reconnecting') {
      down[name] = true;
      dropped = true;
    } else if (state !== 'connecting') delete down[name];
    if (state === 'connecting') opening[name] = true;
    else delete opening[name];
    if (state === 'unauthorized') onUnauthorized(getToken());
  }

  // The tree socket: new children, statuses and costs of every run in the tree.
  $effect(() => {
    const tree = root;
    if (tree === null) return;
    const stream = connectEvents({
      base,
      path: `/ws/tree/${encodeURIComponent(tree)}`,
      after: 0,
      getToken,
      socketFactory: sockets,
      onEvent: (event) => fleet.apply(event, Date.now(), refresh, tree),
      onState: (state) => track('tree', state),
    });
    return () => stream.close();
  });

  // The stream socket of the run on screen, from its first event.
  $effect(() => {
    const run = shown;
    if (run === null) return;
    events = [];
    pending = [];
    tokens = NO_TOKENS;
    stick = true;
    // A recovery is announced only after a drop seen while this run is on screen, unless a socket
    // is still down.
    dropped = untrack(() => Object.keys(down).length > 0);
    let queued = false;
    let open = true;
    const flush = () => {
      queued = false;
      if (!open) return;
      events = events.concat(pending);
      // Only the new events: the totals so far are kept, not summed again over the whole stream.
      tokens = addTokens(tokens, pending);
      pending = [];
    };
    const stream = connectEvents({
      base,
      path: `/ws/runs/${encodeURIComponent(run)}`,
      after: 0,
      getToken,
      socketFactory: sockets,
      onEvent: (event) => {
        pending.push(event);
        if (!queued) {
          queued = true;
          queueMicrotask(flush);
        }
      },
      onState: (state) => track('run', state),
    });
    return () => {
      open = false;
      stream.close();
    };
  });

  // Follow the newest event unless the user scrolled up.
  $effect(() => {
    void events.length;
    if (box && untrack(() => stick)) box.scrollTop = box.scrollHeight;
  });

  function onScroll() {
    if (box) stick = box.scrollTop + box.clientHeight >= box.scrollHeight - 8;
  }

  function jump() {
    if (!box) return;
    box.scrollTop = box.scrollHeight;
    stick = true;
  }

  // A new budget is applied by the engine a moment after the command is accepted, and writes no
  // event: the run and its parent (whose limit pays for it) are fetched again shortly after.
  const budgetTimers: ReturnType<typeof setTimeout>[] = [];
  function afterBudget(rid: RunId) {
    for (const wait of [500, 1_500]) {
      budgetTimers.push(
        setTimeout(() => {
          refresh(rid);
          const parent = fleet.runs[rid]?.parent;
          if (parent) refresh(parent);
        }, wait),
      );
    }
  }
  $effect(() => () => budgetTimers.forEach(clearTimeout));

  /** After a refused command, the run's status from the server: it is newer than what is shown. */
  function reload(rid: RunId) {
    api.getRun(rid).then(
      (summary) => fleet.update(summary, Date.now(), true),
      () => {},
    );
  }

  const run = $derived(shown === null ? undefined : fleet.runs[shown]);
  const all = $derived(Object.values(fleet.runs));
  const reconnecting = $derived(Object.keys(down).length > 0);
  /** What the Live events indicator says: a drop wins over a first connection still on its way. */
  const link = $derived(reconnecting ? 'Reconnecting' : Object.keys(opening).length > 0 ? 'Connecting' : 'Connected');
  /**
   * What screen readers hear: nothing on a first connection (opening a run or switching runs), only
   * that updates stopped and that they are back.
   */
  const announced = $derived(reconnecting ? 'Reconnecting to live updates.' : dropped ? 'Live updates are back.' : '');
  const failedWith = (status: RunStatus) => (typeof status === 'object' && 'Failed' in status ? status.Failed : null);
</script>

{#if error === null && !missing && run !== undefined && root !== null}
  {@const why = failedWith(run.status)}
  <div class="page" data-testid="ready">
    <header class="top">
      <div class="title">
        <nav aria-label="Breadcrumb" class="crumbs">
          <a href={href({ name: 'fleet' })}>Fleet</a>
          <span aria-hidden="true">/</span>
          <span aria-current="page">{run.agent}</span>
        </nav>
        <div class="heading">
          <h1>{run.agent}</h1>
          <span class="pill tone-{statusTone(run.status)}">{statusPill(run.status)}</span>
        </div>
        <p class="facts">
          <span class="mono">run {run.id.slice(-8)}</span>
          {#if run.sandbox_tier !== null}
            <span>{run.sandbox_tier}</span>
          {/if}
          {#if run.parent !== null}
            <a href={href({ name: 'run', id: run.parent })}>Parent: {fleet.runs[run.parent]?.agent ?? 'run'}</a>
          {/if}
          {#if why !== null}
            <span class="why">{why}</span>
          {/if}
        </p>
      </div>
      <!-- Keyed by run: nothing typed, refused or open for one run carries over to another. -->
      {#key run.id}
        <Controls
          part="buttons"
          {run}
          live={liveBelow(all, run.id)}
          {api}
          {onUnauthorized}
          onRefused={() => reload(run.id)}
        />
      {/key}
    </header>

    <div class="layout">
      <div class="side">
        <section class="panel" aria-labelledby="tree-title">
          <h2 id="tree-title" class="eyebrow">Spawn tree</h2>
          <SpawnTree runs={fleet.runs} {root} selected={run.id} />
        </section>
        <BudgetBar spent={run.spent} limit={run.limit} carved={carved(all, run.id)} {tokens} />
        {#key run.id}
          <Controls
            part="limit"
            {run}
            {api}
            {onUnauthorized}
            onRefused={() => reload(run.id)}
            onBudget={() => afterBudget(run.id)}
          />
          <Controls part="message" {run} {api} {onUnauthorized} onRefused={() => reload(run.id)} />
        {/key}
      </div>

      <section class="panel stream" aria-labelledby="events-title">
        <div class="stream-head">
          <h2 id="events-title" class="eyebrow">Live events</h2>
          <!-- Not a live region: its word changes on every connection. The region after it says
               only when updates stop and start again. -->
          <p class="link" class:down={reconnecting}>
            <span class="dot" aria-hidden="true"></span>{link}
          </p>
          <p class="visually-hidden" role="status">{announced}</p>
        </div>
        <!-- A named region, focusable so keyboard users can scroll it. -->
        <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
        <div class="scroll" role="region" aria-label="Event stream" bind:this={box} onscroll={onScroll} tabindex="0">
          <ul aria-label="Events">
            {#each events as event (event.seq)}
              <EventItem {event} />
            {/each}
          </ul>
          {#if events.length === 0}
            <p class="empty">No events yet.</p>
          {/if}
        </div>
        {#if !stick}
          <button type="button" class="jump primary" onclick={jump}>Jump to latest</button>
        {/if}
      </section>
    </div>
  </div>
{:else}
  <h1>Run {id}</h1>
  {#if error !== null}
    <div class="banner error" role="alert" data-testid="ready">
      <p>{error}</p>
      <button type="button" onclick={() => (attempt += 1)}>Retry</button>
    </div>
  {:else if missing}
    <div data-testid="ready">
      <p>Run not found.</p>
      <p><a href={href({ name: 'fleet' })}>Back to Fleet</a></p>
    </div>
  {:else}
    <p>Loading run.</p>
  {/if}
{/if}

<style>
  h1 {
    margin: 0;
    font-size: 2rem;
    line-height: 1.2;
    overflow-wrap: anywhere;
  }
  .banner {
    padding: var(--space-3) var(--space-4);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    background: var(--color-surface);
  }
  .error {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    border-color: var(--color-danger-border);
  }
  .error p {
    margin: 0;
  }
  .page {
    display: grid;
    gap: var(--space-5);
  }
  .top {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--space-4);
  }
  .title {
    display: grid;
    gap: var(--space-1);
    min-width: 0;
  }
  .crumbs {
    display: flex;
    gap: var(--space-2);
    color: var(--color-muted);
  }
  .crumbs a {
    color: inherit;
    text-decoration: none;
  }
  .crumbs a:hover {
    color: var(--color-text);
    text-decoration: underline;
  }
  .heading {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-3);
  }
  .facts {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1) var(--space-4);
    margin: 0;
    color: var(--color-muted);
  }
  .why {
    color: var(--color-danger);
  }
  .layout {
    display: grid;
    grid-template-columns: minmax(20rem, 30rem) 1fr;
    align-items: start;
    gap: var(--space-5);
  }
  .side {
    display: grid;
    gap: var(--space-5);
    min-width: 0;
  }
  .stream {
    position: relative;
    min-width: 0;
  }
  .stream-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .link {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0;
    color: var(--color-done);
    font-size: 0.9rem;
  }
  .link.down {
    color: var(--color-approval);
  }
  .dot {
    width: 0.55rem;
    height: 0.55rem;
    border-radius: 50%;
    background: currentColor;
  }
  .scroll {
    max-height: 75vh;
    overflow: auto;
  }
  .scroll ul {
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .empty {
    margin: 0;
    color: var(--color-muted);
  }
  .jump {
    position: absolute;
    right: var(--space-5);
    bottom: var(--space-5);
  }
  @media (max-width: 60rem) {
    .layout {
      grid-template-columns: 1fr;
    }
  }
</style>
