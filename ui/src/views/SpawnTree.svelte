<script lang="ts">
  // The spawn tree as an indented list: each node shows agent, status and cost, links to its run,
  // and a node with children can be collapsed. A failed node carries its reason as a description
  // (read on focus) and a tooltip (shown on hover).
  import type { FleetRun } from '../lib/fleet.svelte';
  import { formatUsd, statusLabel, statusName } from '../lib/format';
  import { href } from '../lib/router';
  import type { RunId } from '../lib/types';

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

  const failure = (run: FleetRun) =>
    typeof run.status === 'object' && 'Failed' in run.status ? statusLabel(run.status) : undefined;
</script>

<nav aria-label="Spawn tree" class="tree">
  {#if runs[root]}
    <ul>{@render node(runs[root])}</ul>
  {/if}
</nav>

{#snippet node(run: FleetRun)}
  {@const children = kids[run.id] ?? []}
  {@const open = !collapsed[run.id]}
  <li>
    <div class="row" class:selected={run.id === selected}>
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
      <a
        href={href({ name: 'run', id: run.id })}
        aria-current={run.id === selected ? 'page' : undefined}
        aria-describedby="node-{run.id}"
        title={failure(run)}>{run.agent}</a
      >
      <span id="node-{run.id}" class="meta status-{statusName(run.status)}">
        {statusLabel(run.status)}, {formatUsd(run.spent)}
      </span>
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
    margin: 0;
    padding-left: var(--space-3);
    list-style: none;
  }
  .tree > ul {
    padding-left: 0;
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-1) var(--space-2);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius);
  }
  .selected {
    background: var(--color-surface);
    outline: 1px solid var(--color-border);
    font-weight: 600;
  }
  .toggle {
    width: 1.5rem;
    font: inherit;
  }
  .spacer {
    display: inline-block;
    width: 1.5rem;
  }
  .meta {
    color: var(--color-muted);
    font-size: 0.875rem;
    font-weight: 400;
  }
  .status-failed {
    color: var(--color-danger);
  }
</style>
