<script lang="ts">
  // One run in detail, and its tree. Top: status, budget bar and controls. Left: the spawn tree.
  // Center: the run's event stream.
  //
  // The tree comes from `GET /api/runs?root=` and a tree socket that replays the tree and stays
  // open, so new children, statuses and costs show live (the same state the Fleet view keeps). The
  // stream comes from the run's own socket, replayed from seq 0; a reconnect resumes after the
  // last seq. Selecting another run of the same tree changes only the stream: the view stays
  // mounted, so the tree keeps what is collapsed.
  import { untrack } from 'svelte';
  import { ApiFailure, Unauthorized, type Api } from '../lib/api';
  import { FleetState } from '../lib/fleet.svelte';
  import { statusLabel, statusName } from '../lib/format';
  import { failureText, whileMounted, type OnUnauthorized } from '../lib/load';
  import { Refresher } from '../lib/refresher';
  import { href } from '../lib/router';
  import { carved, liveBelow } from '../lib/run';
  import { getToken } from '../lib/token';
  import type { Event, RunId } from '../lib/types';
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
  // Raw: events are never changed in place, so they need no deep proxy (outputs can be large).
  let events: Event[] = $state.raw([]);
  /** Events that arrived since the last render; added in one go. */
  let pending: Event[] = [];
  /** Sockets that dropped and are not open again yet. */
  let down: Record<string, true> = $state({});

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
    if (state === 'reconnecting') down[name] = true;
    else if (state !== 'connecting') delete down[name];
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
    stick = true;
    let queued = false;
    let open = true;
    const flush = () => {
      queued = false;
      if (!open) return;
      events = events.concat(pending);
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
  function reload() {
    api.getRun(id).then(
      (summary) => fleet.update(summary, Date.now(), true),
      () => {},
    );
  }

  const run = $derived(shown === null ? undefined : fleet.runs[shown]);
  const all = $derived(Object.values(fleet.runs));
  const reconnecting = $derived(Object.keys(down).length > 0);
</script>

<h1>{run ? run.agent : `Run ${id}`}</h1>

<!-- Always in the page, so screen readers announce the text when it appears. -->
<p class:banner={reconnecting} role="status">{reconnecting ? 'Reconnecting to live updates.' : ''}</p>

{#if error !== null}
  <p class="banner error" role="alert" data-testid="ready">{error}</p>
{:else if missing}
  <div data-testid="ready">
    <p>Run not found.</p>
    <p><a href={href({ name: 'fleet' })}>Back to Fleet</a></p>
  </div>
{:else if run === undefined || root === null}
  <p>Loading run.</p>
{:else}
  <div class="layout" data-testid="ready">
    <header class="top">
      <p class="facts">
        <span class="badge status-{statusName(run.status)}">{statusLabel(run.status)}</span>
        {#if run.parent !== null}
          <a href={href({ name: 'run', id: run.parent })}>Parent: {fleet.runs[run.parent]?.agent ?? 'run'}</a>
        {/if}
      </p>
      <BudgetBar spent={run.spent} limit={run.limit} carved={carved(all, run.id)} />
      <!-- Keyed by run: nothing typed, refused or open for one run carries over to another. -->
      {#key run.id}
        <Controls
          {run}
          live={liveBelow(all, run.id)}
          {api}
          {onUnauthorized}
          onRefused={reload}
          onBudget={() => afterBudget(run.id)}
        />
      {/key}
    </header>
    <SpawnTree runs={fleet.runs} {root} selected={run.id} />
    <section class="stream">
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
        <button type="button" class="jump" onclick={jump}>Jump to latest</button>
      {/if}
    </section>
  </div>
{/if}

<style>
  h1 {
    overflow-wrap: anywhere;
  }
  .banner {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    background: var(--color-surface);
  }
  .error {
    border-color: var(--color-danger);
  }
  .layout {
    display: grid;
    grid-template-columns: minmax(12rem, 18rem) 1fr;
    grid-template-areas:
      'top top'
      'tree stream';
    gap: var(--space-3);
  }
  .top {
    grid-area: top;
    display: grid;
    gap: var(--space-2);
  }
  .facts {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-3);
    margin: 0;
  }
  .badge {
    padding: 0 var(--space-2);
    border: 1px solid currentColor;
    border-radius: var(--radius);
  }
  .status-running,
  .status-provisioning {
    color: var(--color-status-running);
  }
  .status-suspended {
    color: var(--color-status-suspended);
  }
  .status-failed {
    color: var(--color-danger);
  }
  .status-queued,
  .status-completed,
  .status-cancelled {
    color: var(--color-muted);
  }
  .layout > :global(.tree) {
    grid-area: tree;
  }
  .stream {
    grid-area: stream;
    position: relative;
    min-width: 0;
  }
  .scroll {
    max-height: 70vh;
    overflow: auto;
    padding: 0 var(--space-2);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
  }
  .scroll ul {
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .empty {
    color: var(--color-muted);
  }
  .jump {
    position: absolute;
    right: var(--space-3);
    bottom: var(--space-3);
    font: inherit;
  }
  @media (max-width: 48rem) {
    .layout {
      grid-template-columns: 1fr;
      grid-template-areas:
        'top'
        'tree'
        'stream';
    }
  }
</style>
