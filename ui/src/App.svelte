<script lang="ts">
  import { createApi } from './lib/api';
  import { ActiveRuns } from './lib/active.svelte';
  import { ApprovalsState, followApprovals } from './lib/approvals.svelte';
  import { href, parseRoute, type Route } from './lib/router';
  import { clearToken, getToken, isCurrent, setToken } from './lib/token';
  import Approvals from './views/Approvals.svelte';
  import Fleet from './views/Fleet.svelte';
  import NotFound from './views/NotFound.svelte';
  import Run from './views/Run.svelte';
  import TokenRequired from './views/TokenRequired.svelte';
  import { socketBase, type SocketFactory } from './lib/ws';

  let {
    fetch: fetchImpl = (...args: Parameters<typeof fetch>) => globalThis.fetch(...args),
    sockets,
  }: { fetch?: typeof fetch; sockets?: SocketFactory } = $props();

  const api = $derived(createApi({ fetch: fetchImpl, getToken }));

  let route: Route = $state(parseRoute(window.location.hash));
  let needsToken = $state(false);
  let refused = $state(false);
  // Bumped after a token is entered, so the current view mounts again and retries its request.
  let attempt = $state(0);
  let main: HTMLElement | undefined = $state();

  // Pending approvals across all runs: the Approvals view lists them, the nav counts them. The same
  // socket keeps the count of active runs beside Fleet.
  const approvals = new ApprovalsState();
  const active = new ActiveRuns();
  let retryApprovals = $state(() => {});
  $effect(() => {
    // Read so that a new token starts again.
    void attempt;
    const follow = followApprovals({
      api,
      state: approvals,
      runs: active,
      onUnauthorized,
      base: socketBase(window.location),
      sockets,
    });
    retryApprovals = follow.retry;
    return () => follow.stop();
  });

  /** The run opened last in this tab: the Run item in the sidebar goes back to it. */
  const opened = parseRoute(window.location.hash);
  let lastRun: string | null = $state(opened.name === 'run' ? opened.id : null);

  $effect(() => {
    const onHash = () => {
      route = parseRoute(window.location.hash);
      if (route.name === 'run') lastRun = route.id;
    };
    window.addEventListener('hashchange', onHash);
    return () => window.removeEventListener('hashchange', onHash);
  });

  function onUnauthorized(sent: string | null) {
    // A 401 for a token that is no longer held (an answer that crossed a token change) is stale.
    if (!isCurrent(sent)) return;
    refused = sent !== null;
    clearToken();
    needsToken = true;
  }

  function submitToken(token: string) {
    setToken(token);
    refused = false;
    needsToken = false;
    attempt += 1;
  }

  // A plain `href="#main"` would change the hash and leave the current route.
  function skipToContent(event: MouseEvent) {
    event.preventDefault();
    main?.focus();
  }

  /**
   * Pages of later milestones: shown so the shape of the product is clear, not linked. The tag is
   * only seen; a screen reader says the full phrase.
   */
  const later = [
    { label: 'Agents', tag: 'M4', said: 'coming in M4' },
    { label: 'Workflows', tag: 'M4', said: 'coming in M4' },
    { label: 'Triggers', tag: 'M4', said: 'coming in M4' },
    { label: 'Meetings', tag: 'M5', said: 'coming in M5' },
    { label: 'Costs', tag: 'M3', said: 'coming in M3' },
    { label: 'Settings', tag: 'Soon', said: 'coming soon' },
  ];
</script>

<a class="skip" href="#main" onclick={skipToContent}>Skip to content</a>
<div class="shell">
  <aside class="sidebar">
    <div class="brand">
      <svg class="logo" viewBox="0 0 32 32" aria-hidden="true">
        <rect width="32" height="32" rx="8" fill="var(--color-accent)" />
        <path d="M8 23c4-1 7-4 9-9l1-4c2 1 4 3 5 6l-4-1c-1 4-5 7-11 8z" fill="#fff8f0" />
        <circle cx="19.5" cy="11.5" r="1.2" fill="var(--color-accent)" />
      </svg>
      <span>Kestrel</span>
    </div>
    <nav aria-label="Main">
      <ul>
        <li>
          <a class="item" href={href({ name: 'fleet' })} aria-current={route.name === 'fleet' ? 'page' : undefined}>
            <span>Fleet</span>
            {#if active.loaded}<span class="count">{active.count}<span class="visually-hidden">&nbsp;active</span></span>{/if}
          </a>
        </li>
        <li>
          {#if lastRun === null}
            <span class="item off">Run<span class="visually-hidden">, open one from Fleet</span></span>
          {:else}
            <a class="item" href={href({ name: 'run', id: lastRun })} aria-current={route.name === 'run' ? 'page' : undefined}>Run</a>
          {/if}
        </li>
        <li>
          <a
            class="item"
            href={href({ name: 'approvals' })}
            aria-current={route.name === 'approvals' ? 'page' : undefined}
          >
            <span>Approvals</span>
            {#if approvals.count > 0}<span class="count hot"
                >{approvals.count}<span class="visually-hidden">&nbsp;pending</span></span
              >{/if}
          </a>
        </li>
        {#each later as page (page.label)}
          <li>
            <span class="item off"
              >{page.label}<span class="visually-hidden">, {page.said}</span><span class="tag" aria-hidden="true">{page.tag}</span></span
            >
          </li>
        {/each}
      </ul>
    </nav>
  </aside>
  <main id="main" tabindex="-1" bind:this={main}>
  {#if needsToken}
    <TokenRequired {refused} onSubmit={submitToken} />
  {:else}
    {#key attempt}
      {#if route.name === 'fleet'}
        <Fleet {api} {onUnauthorized} {sockets} {approvals} liveRuns={active} />
      {:else if route.name === 'approvals'}
        <Approvals {approvals} {api} {onUnauthorized} onRetry={() => retryApprovals()} />
      {:else if route.name === 'run'}
        <!-- Not keyed by id: moving between runs of one tree keeps the tree as it is. -->
        <Run id={route.id} {api} {onUnauthorized} {sockets} />
      {:else}
        <NotFound />
      {/if}
    {/key}
  {/if}
  </main>
</div>

<style>
  .skip {
    position: absolute;
    left: var(--space-2);
    top: calc(-1 * var(--space-6));
    padding: var(--space-1) var(--space-2);
    background: var(--color-surface);
    color: var(--color-text);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
  }
  .skip:focus {
    top: var(--space-2);
  }
  .shell {
    display: grid;
    grid-template-columns: 15rem minmax(0, 1fr);
    min-height: 100vh;
  }
  .sidebar {
    position: sticky;
    top: 0;
    align-self: start;
    height: 100vh;
    box-sizing: border-box;
    padding: var(--space-5) var(--space-3);
    border-right: 1px solid var(--color-border);
    background: var(--color-sidebar);
    overflow-y: auto;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    margin: 0 var(--space-2) var(--space-5);
    color: var(--color-text);
    font-size: 1.25rem;
    font-weight: 700;
  }
  .logo {
    width: 2rem;
    height: 2rem;
    flex: none;
  }
  nav ul {
    display: grid;
    gap: var(--space-1);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid transparent;
    border-radius: var(--radius);
    color: var(--color-muted);
    text-decoration: none;
  }
  a.item:hover {
    background: var(--color-raised);
    color: var(--color-text);
  }
  a.item[aria-current='page'] {
    border-left-color: var(--color-accent);
    background: var(--color-accent-soft);
    color: var(--color-text);
  }
  .off {
    color: var(--color-faint);
  }
  .count,
  .tag {
    min-width: 1.25rem;
    padding: 0 var(--space-2);
    border-radius: 999px;
    background: var(--color-raised);
    color: var(--color-muted);
    font-size: 0.75rem;
    text-align: center;
  }
  .count.hot {
    background: var(--color-accent);
    color: var(--color-on-accent);
    font-weight: 700;
  }
  .tag {
    font-family: var(--font-mono);
  }
  main {
    box-sizing: border-box;
    width: 100%;
    max-width: 90rem;
    padding: var(--space-6) var(--space-6) var(--space-6);
  }
  main:focus {
    outline: none;
  }
  @media (max-width: 48rem) {
    .shell {
      grid-template-columns: 1fr;
    }
    .sidebar {
      position: static;
      height: auto;
      padding: var(--space-3) var(--space-4);
      border-right: 0;
      border-bottom: 1px solid var(--color-border);
    }
    .brand {
      margin-bottom: var(--space-3);
    }
    nav ul {
      display: flex;
      flex-wrap: wrap;
    }
    .off {
      display: none;
    }
    main {
      padding: var(--space-4);
    }
  }
</style>
