<script lang="ts">
  // One pending approval: who asks and what, how long it has waited, and the ways to decide it.
  // A decision stays "in progress" until the resolved event takes the row away; a refusal because
  // someone else decided first (409) is handed to `onGone`; any other failure puts the row back
  // with the error and a Retry.
  import { ApiFailure, type Api } from '../lib/api';
  import type { ApprovalRow } from '../lib/approvals.svelte';
  import { formatElapsed } from '../lib/format';
  import { failureText, type OnUnauthorized } from '../lib/load';
  import { href } from '../lib/router';
  import type { RunSummary } from '../lib/types';

  let {
    row,
    run,
    now,
    api,
    onUnauthorized,
    onGone,
    onDismiss,
  }: {
    row: ApprovalRow;
    run: RunSummary | undefined;
    now: number;
    api: Api;
    onUnauthorized: OnUnauthorized;
    onGone: () => void;
    onDismiss: () => void;
  } = $props();

  /** Characters of the payload shown before "Show all". */
  const PREVIEW = 500;

  const approval = $derived(row.approval);
  const id = $derived(approval.id);
  const short = (runId: string) => runId.slice(-8);

  let mode: 'idle' | 'rejecting' | 'editing' = $state('idle');
  let sending: 'approve' | 'reject' | 'answer' | null = $state(null);
  let error: string | null = $state(null);
  let retry: (() => void) | null = null;
  let reason = $state('');
  let edited = $state('');
  let answer = $state('');
  let answerError: string | null = $state(null);
  let expanded = $state(false);

  const long = $derived(approval.action.length > PREVIEW);
  const shown = $derived(long && !expanded ? `${approval.action.slice(0, PREVIEW)}...` : approval.action);

  async function decide(kind: 'approve' | 'reject' | 'answer', call: () => Promise<void>) {
    sending = kind;
    error = null;
    retry = () => void decide(kind, call);
    try {
      await call();
      mode = 'idle';
    } catch (e) {
      sending = null;
      if (e instanceof ApiFailure && e.status === 409) {
        onGone();
        return;
      }
      error = failureText(e, onUnauthorized);
    }
  }

  function approve() {
    void decide('approve', () => api.approve(id));
  }

  function confirmReject(e: SubmitEvent) {
    e.preventDefault();
    const text = reason.trim();
    void decide('reject', () => api.reject(id, text === '' ? undefined : text));
  }

  function startEdit() {
    edited = approval.action;
    mode = 'editing';
  }

  function approveEdited(e: SubmitEvent) {
    e.preventDefault();
    void decide('approve', () => api.approve(id, edited));
  }

  function sendAnswer(e: SubmitEvent) {
    e.preventDefault();
    const text = answer.trim();
    if (text === '') {
      answerError = 'Write an answer first.';
      return;
    }
    answerError = null;
    void decide('answer', () => api.approve(id, text));
  }
</script>

<li class="row" data-id={id}>
  <p class="who">
    <strong>{run?.agent ?? 'agent not known yet'}</strong>
    <a href={href({ name: 'run', id: approval.run_id })} title={approval.run_id}>Run {short(approval.run_id)}</a>
    {#if run && run.root !== run.id}
      <a href={href({ name: 'run', id: run.root })} title={run.root}>Root {short(run.root)}</a>
    {/if}
    <span class="wait">waiting {formatElapsed(now - Date.parse(approval.requested_at))}</span>
  </p>
  {#if approval.question}
    <p class="kind">Question</p>
  {/if}
  <p class="action" id="action-{id}">{shown}</p>
  {#if long}
    <button type="button" class="link" aria-expanded={expanded} aria-controls="action-{id}" onclick={() => (expanded = !expanded)}>
      {expanded ? 'Show less' : 'Show all'}
    </button>
  {/if}

  {#if row.ended !== null}
    <p class="ended">{row.ended === 'cancelled' ? 'Run cancelled' : 'Run ended'}</p>
    <div class="buttons">
      <button type="button" onclick={onDismiss}>Dismiss</button>
    </div>
  {:else if approval.question}
    <form class="stack" onsubmit={sendAnswer} novalidate>
      <label for="answer-{id}">Answer</label>
      <textarea id="answer-{id}" rows="3" bind:value={answer} disabled={sending !== null}></textarea>
      {#if answerError !== null}
        <p class="error" role="alert">{answerError}</p>
      {/if}
      <div class="buttons">
        <button type="submit" disabled={sending !== null}>{sending === 'answer' ? 'Sending answer' : 'Send answer'}</button>
        <button type="button" disabled={sending !== null} onclick={() => void decide('reject', () => api.reject(id))}>
          {sending === 'reject' ? 'Rejecting' : 'Reject'}
        </button>
      </div>
    </form>
  {:else if mode === 'rejecting'}
    <form class="stack" onsubmit={confirmReject}>
      <label for="reason-{id}">Reason (optional)</label>
      <input id="reason-{id}" autocomplete="off" bind:value={reason} disabled={sending !== null} />
      <div class="buttons">
        <button type="submit" disabled={sending !== null}>{sending === 'reject' ? 'Rejecting' : 'Confirm reject'}</button>
        <button type="button" disabled={sending !== null} onclick={() => (mode = 'idle')}>Cancel</button>
      </div>
    </form>
  {:else if mode === 'editing'}
    <form class="stack" onsubmit={approveEdited}>
      <label for="edit-{id}">Edited action</label>
      <textarea id="edit-{id}" rows="4" bind:value={edited} disabled={sending !== null}></textarea>
      <div class="buttons">
        <button type="submit" disabled={sending !== null}>{sending === 'approve' ? 'Approving' : 'Approve edited'}</button>
        <button type="button" disabled={sending !== null} onclick={() => (mode = 'idle')}>Cancel</button>
      </div>
    </form>
  {:else}
    <div class="buttons">
      <button type="button" disabled={sending !== null} onclick={approve}>{sending === 'approve' ? 'Approving' : 'Approve'}</button>
      <button type="button" disabled={sending !== null} onclick={() => (mode = 'rejecting')}>Reject</button>
      <button type="button" disabled={sending !== null} onclick={startEdit}>Edit and approve</button>
    </div>
  {/if}

  {#if error !== null}
    <div class="failed">
      <p class="error" role="alert">{error}</p>
      <button type="button" onclick={() => retry?.()}>Retry</button>
    </div>
  {/if}
</li>

<style>
  .row {
    display: grid;
    gap: var(--space-2);
    padding: var(--space-3);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    background: var(--color-surface);
  }
  .who {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-1) var(--space-3);
    margin: 0;
  }
  .wait,
  .kind {
    color: var(--color-muted);
    margin: 0;
  }
  .action {
    margin: 0;
    padding: var(--space-2);
    border-radius: var(--radius);
    background: var(--color-bg);
    font-family: ui-monospace, monospace;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .stack {
    display: grid;
    gap: var(--space-1);
  }
  .buttons,
  .failed {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }
  .error {
    margin: 0;
    color: var(--color-danger);
  }
  .ended {
    margin: 0;
    font-weight: 600;
  }
  .link {
    justify-self: start;
  }
  button,
  input,
  textarea {
    font: inherit;
  }
</style>
