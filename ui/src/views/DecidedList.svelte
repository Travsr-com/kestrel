<script lang="ts">
  // The approvals decided most recently: who asked, what (its first line, capped: an action can be
  // 50,000 characters), and the outcome. Read only.
  import { firstLine } from '../lib/format';
  import type { Approval, RunId, RunSummary } from '../lib/types';

  let {
    decided,
    failed,
    runs,
  }: { decided: Approval[] | null; failed: boolean; runs: Record<RunId, RunSummary> } = $props();

  const outcome = (status: Approval['status']) =>
    status === 'approved' ? 'Approved' : status === 'rejected' ? 'Rejected' : 'Expired';
</script>

<h2 class="eyebrow">Recently decided</h2>
{#if failed}
  <p class="muted">Could not load decided approvals.</p>
{:else if decided === null}
  <p class="muted">Loading decided approvals.</p>
{:else if decided.length === 0}
  <p class="muted">Nothing decided yet.</p>
{:else}
  <ul class="decided" aria-label="Recently decided">
    {#each decided as approval (approval.id)}
      <li>
        <span class="text">
          <span class="agent">{runs[approval.run_id]?.agent ?? 'agent not known yet'}</span>
          <span class="action">{firstLine(approval.action)}</span>
        </span>
        <span class="outcome {approval.status}">
          <svg viewBox="0 0 12 12" width="12" height="12" aria-hidden="true">
            {#if approval.status === 'approved'}
              <path d="M2 6.5l2.5 2.5L10 3.5" />
            {:else if approval.status === 'rejected'}
              <path d="M3 3l6 6M9 3l-6 6" />
            {:else}
              <circle cx="6" cy="6" r="4.5" /><path d="M6 3.5V6l1.5 1.5" />
            {/if}
          </svg>
          {outcome(approval.status)}
        </span>
      </li>
    {/each}
  </ul>
{/if}

<style>
  .decided {
    display: grid;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  li {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-lg);
    background: var(--color-surface);
  }
  .text {
    display: grid;
    min-width: 0;
  }
  .action {
    overflow: hidden;
    color: var(--color-muted);
    font-size: 0.9rem;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .outcome {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    flex: none;
    font-weight: 600;
  }
  .outcome svg {
    fill: none;
    stroke: currentColor;
    stroke-width: 1.6;
    stroke-linecap: round;
  }
  .approved {
    color: var(--color-done);
  }
  .rejected {
    color: var(--color-danger);
  }
  .expired {
    color: var(--color-muted);
  }
  .muted {
    margin: 0;
    color: var(--color-muted);
  }
</style>
