<script lang="ts">
  // The spawn tree as an indented list: each node shows a status icon, the agent (a link to its
  // run), its status pill text and its cost, and a node with children can be collapsed. A failed
  // node carries its reason in its description (read on focus) and a tooltip (shown on hover).
  import type { FleetRun } from '../lib/fleet.svelte';
  import { formatUsd, statusLabel, statusPill, statusTone } from '../lib/format';
  import { href } from '../lib/router';
  import type { RunId, RunStatus } from '../lib/types';

  let { runs, root, selected }: { runs: Record<RunId, FleetRun>; root: RunId; selected: RunId } = $props();

  let collapsed: Record<RunId, boolean> = $state({});

  /** Each run's children, oldest first. */
  const kids = $derived.by(() => {
    const out: Record<RunId, FleetRun[]> = {};
    for (const run of Object.values(runs)) {
      if (run.parent !== null) (out[run.parent] ??= []).push(run);
    }
    for (const list of Object.values(out)) list.sort((a, b) => (a.created_at < b.created_at ? -1 : 1));
    return out;
  });

  const reason = (status: RunStatus) => (typeof status === 'object' && 'Failed' in status ? status.Failed : null);

  type Icon = 'play' | 'check' | 'pause' | 'clock' | 'cross';

  function icon(status: RunStatus): Icon {
    const tone = statusTone(status);
    if (tone === 'running') return 'play';
    if (tone === 'done') return 'check';
    if (tone === 'failed' || status === 'Cancelled') return 'cross';
    if (tone === 'approval' || statusPill(status) === 'Paused') return 'pause';
    return 'clock';
  }
</script>

<nav aria-label="Spawn tree" class="tree">
  {#if runs[root]}
    <ul>{@render node(runs[root])}</ul>
  {/if}
</nav>

{#snippet glyph(kind: Icon)}
  <svg viewBox="0 0 16 16" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.8">
    {#if kind === 'play'}
      <path d="M4.5 3v10l8-5z" stroke-linejoin="round" />
    {:else if kind === 'check'}
      <path d="M3 8.5l3.2 3L13 4.5" stroke-linecap="round" stroke-linejoin="round" />
    {:else if kind === 'pause'}
      <path d="M5.5 3.5v9M10.5 3.5v9" stroke-linecap="round" />
    {:else if kind === 'cross'}
      <path d="M4 4l8 8M12 4l-8 8" stroke-linecap="round" />
    {:else}
      <circle cx="8" cy="8" r="5.5" />
      <path d="M8 5v3.2l2 1.3" stroke-linecap="round" />
    {/if}
  </svg>
{/snippet}

{#snippet node(run: FleetRun)}
  {@const children = kids[run.id] ?? []}
  {@const open = !collapsed[run.id]}
  {@const tone = statusTone(run.status)}
  {@const failed = reason(run.status)}
  {@const shape = icon(run.status)}
  <li>
    <div class="row tone-{tone}" class:selected={run.id === selected}>
      {#if children.length > 0}
        <button
          type="button"
          class="toggle"
          aria-expanded={open}
          aria-label="Children of {run.agent}"
          onclick={() => (collapsed[run.id] = open)}
        >
          <span aria-hidden="true">{open ? '-' : '+'}</span>
        </button>
      {:else}
        <span class="spacer"></span>
      {/if}
      <span class="icon" aria-hidden="true" data-icon={shape}>{@render glyph(shape)}</span>
      <span class="name">
        <a
          href={href({ name: 'run', id: run.id })}
          aria-current={run.id === selected ? 'page' : undefined}
          aria-describedby="node-status-{run.id} node-cost-{run.id}"
          title={failed === null ? undefined : statusLabel(run.status)}>{run.agent}</a
        >
        <span id="node-status-{run.id}" class="status tone-{tone}"
          >{statusPill(run.status)}{#if failed !== null}<span class="visually-hidden">: {failed}</span>{/if}</span
        >
      </span>
      <span id="node-cost-{run.id}" class="cost mono">{formatUsd(run.spent)}</span>
    </div>
    {#if children.length > 0 && open}
      <ul>
        {#each children as child (child.id)}
          {@render node(child)}
        {/each}
      </ul>
    {/if}
  </li>
{/snippet}

<style>
  ul {
    display: grid;
    gap: var(--space-1);
    margin: 0;
    padding-left: var(--space-5);
    list-style: none;
  }
  ul ul {
    margin-top: var(--space-1);
  }
  .tree > ul {
    padding-left: 0;
  }
  .row {
    display: grid;
    grid-template-columns: 1.5rem 1rem minmax(0, 1fr) auto;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3) var(--space-2) var(--space-1);
    border: 1px solid transparent;
    border-radius: var(--radius);
  }
  .selected {
    border-color: var(--color-accent);
    background: var(--color-accent-soft);
  }
  .toggle {
    width: 1.5rem;
    height: 1.5rem;
    padding: 0;
    border-color: transparent;
    background: none;
    color: var(--color-muted);
    line-height: 1;
  }
  .spacer {
    width: 1.5rem;
  }
  .icon {
    display: flex;
    color: var(--tone);
  }
  .name {
    display: grid;
    min-width: 0;
  }
  .name a {
    color: var(--color-text);
    font-weight: 600;
    text-decoration: none;
    overflow-wrap: anywhere;
  }
  .name a:hover {
    text-decoration: underline;
  }
  .status {
    color: var(--tone);
    font-size: 0.85rem;
  }
  .cost {
    color: var(--color-muted);
    font-size: 0.875rem;
  }
</style>
