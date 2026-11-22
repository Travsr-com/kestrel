<script lang="ts">
  import { firstLine, formatElapsed, formatUsd, percentUsed, statusPill, statusTone } from '../lib/format';
  import type { ChildCount, FleetRun } from '../lib/fleet.svelte';
  import { href } from '../lib/router';

  let {
    run,
    now,
    children,
    parentAgent,
    ask,
  }: {
    run: FleetRun;
    now: number;
    children?: ChildCount;
    parentAgent?: string;
    /** What the approval this run waits on asks, when the pending list has it. */
    ask?: string;
  } = $props();

  const tone = $derived(statusTone(run.status));

  // A run whose end event was seen shows how long it ran; otherwise just that it ended.
  const elapsed = $derived(
    run.endedAt === null
      ? formatElapsed(now - Date.parse(run.created_at))
      : run.finishedAt !== null
        ? `ran ${formatElapsed(run.finishedAt - Date.parse(run.created_at))}`
        : 'ended',
  );

  /** What the run waits on or why it stopped; `null` when there is nothing to say. */
  const note = $derived.by(() => {
    const status = run.status;
    if (typeof status === 'string') return null;
    if ('Failed' in status) return status.Failed;
    const reason = status.Suspended;
    if (reason === 'Budget') return 'Budget used up. Raise the limit to continue.';
    if (reason === 'Manual') return 'Paused.';
    // The first line, capped: an action can be 50,000 characters.
    if ('Approval' in reason) return ask === undefined ? 'Waiting for an approval.' : firstLine(ask);
    return reason.Error;
  });

  // A limit known only from a spawn event is not the run's real limit yet.
  const percent = $derived(run.fetched ? percentUsed(run.spent, run.limit) : null);
  const limitText = $derived(
    !run.fetched ? ', limit not known yet' : run.limit === null ? ', no limit' : ` of ${formatUsd(run.limit)}`,
  );

  const subAgents = $derived.by(() => {
    const total = children?.total ?? 0;
    const running = children?.running ?? 0;
    const count = total === 0 ? 'no sub-agents' : total === 1 ? '1 sub-agent' : `${total} sub-agents`;
    return running > 0 ? `${count}, ${running} running` : count;
  });
</script>

<article class="panel card tone-{tone}">
  <div class="top">
    <!-- The agent link covers the whole card, so a click anywhere on it opens the run. -->
    <!-- Described by its status and id, so cards of the same agent are told apart in a list of links. -->
    <h2>
      <a class="open" href={href({ name: 'run', id: run.id })} aria-describedby="status-{run.id} id-{run.id}"
        >{run.agent}</a
      >
    </h2>
    <span class="pill tone-{tone}" id="status-{run.id}">{statusPill(run.status)}</span>
  </div>
  <!-- The end of the id: a time-ordered id starts with its timestamp, shared by runs started together. -->
  <p class="id mono" id="id-{run.id}">run {run.id.slice(-8)}</p>
  {#if note !== null}
    <p class="note">{note}</p>
  {/if}
  <div class="spend">
    <span class="cost">{formatUsd(run.spent)}{limitText}</span>
    {#if percent !== null}<span>{percent}%</span>{/if}
  </div>
  {#if percent !== null}
    <div
      class="track"
      role="meter"
      aria-label="Budget used by {run.agent}, run {run.id.slice(-8)}"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={percent}
      aria-valuetext="{percent}%"
    >
      <div class="fill" style:width="{percent}%"></div>
    </div>
  {/if}
  <p class="foot">
    {#if run.parent}
      <a class="parent" href={href({ name: 'run', id: run.parent })}>Parent: {parentAgent ?? 'run'}</a>
    {:else}
      <span>{subAgents}</span>
    {/if}
    <span><span class="visually-hidden">Elapsed </span>{elapsed}</span>
    <span><span class="visually-hidden">Sandbox </span>{run.sandbox_tier ?? 'tier not known yet'}</span>
  </p>
</article>

<style>
  .card {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    height: 100%;
    box-sizing: border-box;
  }
  .tone-approval.card {
    border-color: var(--color-approval-border);
  }
  .top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }
  h2 {
    margin: 0;
    min-width: 0;
    overflow-wrap: anywhere;
    font-size: 1.1rem;
  }
  .open {
    color: var(--color-text);
    text-decoration: none;
  }
  .open::after {
    content: '';
    position: absolute;
    inset: 0;
    border-radius: inherit;
  }
  .open:focus-visible {
    outline: none;
  }
  .open:focus-visible::after {
    outline: 2px solid var(--color-accent);
    outline-offset: 2px;
  }
  .parent {
    position: relative;
    z-index: 1;
  }
  p {
    margin: 0;
  }
  .id {
    margin-top: calc(-1 * var(--space-2));
    /* Muted, like the rest of the card's secondary text. */
    color: var(--color-muted);
    font-size: 0.85rem;
  }
  .note {
    overflow-wrap: anywhere;
  }
  .spend {
    display: flex;
    justify-content: space-between;
    gap: var(--space-3);
    margin-top: auto;
    padding-top: var(--space-2);
    color: var(--color-muted);
    font-size: 0.9rem;
    font-variant-numeric: tabular-nums;
  }
  .foot {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1) var(--space-4);
    color: var(--color-muted);
    font-size: 0.9rem;
    font-variant-numeric: tabular-nums;
  }
</style>
