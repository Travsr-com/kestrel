<script lang="ts">
  import { createApi } from './lib/api';
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

  // Pending approvals across all runs: the Approvals view lists them, the nav counts them.
  const approvals = new ApprovalsState();
  let retryApprovals = $state(() => {});
  $effect(() => {
    // Read so that a new token starts again.
    void attempt;
    const follow = followApprovals({ api, state: approvals, onUnauthorized, base: socketBase(window.location), sockets });
    retryApprovals = follow.retry;
    return () => follow.stop();
  });

  $effect(() => {
    const onHash = () => (route = parseRoute(window.location.hash));
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

  const nav = [
    { label: 'Fleet', to: href({ name: 'fleet' }), active: (r: Route) => r.name === 'fleet', count: false },
    { label: 'Approvals', to: href({ name: 'approvals' }), active: (r: Route) => r.name === 'approvals', count: true },
  ];
</script>

<a class="skip" href="#main" onclick={skipToContent}>Skip to content</a>
<header class="bar">
  <span class="brand">Kestrel</span>
  <nav aria-label="Main">
    <ul>
      {#each nav as item (item.to)}
        <li>
          <a href={item.to} aria-current={item.active(route) ? 'page' : undefined}
            >{item.label}{#if item.count && approvals.count > 0}&nbsp;<span class="count"
                  >{approvals.count}<span class="hidden">&nbsp;pending</span></span
                >{/if}</a
          >
        </li>
      {/each}
    </ul>
  </nav>
</header>
<main id="main" tabindex="-1" bind:this={main}>
  {#if needsToken}
    <TokenRequired {refused} onSubmit={submitToken} />
  {:else}
    {#key attempt}
      {#if route.name === 'fleet'}
        <Fleet {api} {onUnauthorized} {sockets} />
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
  .bar {
    display: flex;
    align-items: center;
    gap: var(--space-4);
    padding: var(--space-2) var(--space-4);
    border-bottom: 1px solid var(--color-border);
    background: var(--color-surface);
  }
  .brand {
    font-weight: 700;
  }
  nav ul {
    display: flex;
    gap: var(--space-3);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  nav a {
    color: var(--color-muted);
    text-decoration: none;
  }
  nav a[aria-current='page'] {
    color: var(--color-text);
    font-weight: 600;
    text-decoration: underline;
    text-underline-offset: 0.3em;
  }
  main {
    max-width: 72rem;
    margin: 0 auto;
    padding: var(--space-4);
  }
  main:focus {
    outline: none;
  }
  .count {
    display: inline-block;
    min-width: 1.25rem;
    margin-left: var(--space-1);
    padding: 0 var(--space-1);
    border-radius: 999px;
    background: var(--color-accent);
    color: var(--color-on-accent);
    font-size: 0.75rem;
    font-weight: 600;
    text-align: center;
  }
  .hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
</style>
