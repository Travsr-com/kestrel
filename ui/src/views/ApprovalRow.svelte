<script lang="ts">
  // One pending approval: who asks and what, how long it has waited, and the ways to decide it.
  // A decision stays "in progress" until the resolved event takes the row away; a refusal because
  // someone else decided first (409) is handed to `onGone`; any other failure puts the row back
  // with the error and a Retry, which sends what the fields hold then.
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
    onGone: (why: 'resolved' | 'not-waiting') => void;
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

  /**
   * Sends a decision. `again` is what Retry runs: by default the same call (which reads the fields
   * when it runs), or a submit that checks the fields first. On success the row keeps its form and
   * label ("Rejecting", "Approving") until the resolved event takes it away.
   */
  async function decide(kind: 'approve' | 'reject' | 'answer', call: () => Promise<void>, again?: () => void) {
    sending = kind;
    error = null;
    retry = again ?? (() => void decide(kind, call));
    try {
      await call();
    } catch (e) {
      sending = null;
      if (e instanceof ApiFailure && e.status === 409) {
        // The server says which: resolved already, or its run is no longer waiting on it.
        onGone(e.message.includes('not waiting') ? 'not-waiting' : 'resolved');
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
    // Read when sent, so a Retry sends the reason as it is then.
    void decide('reject', () => api.reject(id, reason.trim() === '' ? undefined : reason.trim()));
  }

  function approveEdited(e: SubmitEvent) {
    e.preventDefault();
    void decide('approve', () => api.approve(id, edited));
  }

  function sendAnswer(e: SubmitEvent) {
    e.preventDefault();
    submitAnswer();
  }

  /** Also what Retry runs, so a cleared answer is not sent either. */
  function submitAnswer() {
    const text = answer.trim();
    if (text === '') {
      error = null;
      answerError = 'Write an answer first.';
      return;
    }
    answerError = null;
    void decide('answer', () => api.approve(id, text), submitAnswer);
  }

  // Focus follows the row's own changes: an opened form's field, the button that opened it after
  // Cancel, and Dismiss (or the first button) when the buttons that had focus went away.
  let item: HTMLLIElement | undefined = $state();
  let reasonInput: HTMLInputElement | undefined = $state();
  let editBox: HTMLTextAreaElement | undefined = $state();
  let rejectButton: HTMLButtonElement | undefined = $state();
  let editButton: HTMLButtonElement | undefined = $state();
  let focusNext: 'reason' | 'edit' | 'reject' | 'editButton' | null = null;
  let hadFocus = false;

  function open(next: 'rejecting' | 'editing') {
    if (next === 'editing') edited = approval.action;
    focusNext = next === 'rejecting' ? 'reason' : 'edit';
    mode = next;
  }

  function cancel() {
    focusNext = mode === 'rejecting' ? 'reject' : 'editButton';
    mode = 'idle';
  }

  $effect.pre(() => {
    void mode;
    void row.ended;
    hadFocus = item?.contains(document.activeElement) ?? false;
  });
  $effect(() => {
    void mode;
    void row.ended;
    const target = { reason: reasonInput, edit: editBox, reject: rejectButton, editButton }[focusNext ?? 'reject'];
    if (focusNext !== null) {
      focusNext = null;
      target?.focus();
      return;
    }
    if (hadFocus && !item?.contains(document.activeElement)) item?.querySelector<HTMLElement>('.buttons button')?.focus();
    hadFocus = false;
  });
</script>

<li class="row" data-id={id} bind:this={item}>
  <p class="who" id="who-{id}">
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
  {:else if mode === 'rejecting'}
    <form class="stack" onsubmit={confirmReject}>
      <label for="reason-{id}">Reason (optional)</label>
      <input id="reason-{id}" autocomplete="off" bind:value={reason} bind:this={reasonInput} disabled={sending !== null} />
      <div class="buttons">
        <button type="submit" disabled={sending !== null}>{sending === 'reject' ? 'Rejecting' : 'Confirm reject'}</button>
        <button type="button" disabled={sending !== null} onclick={cancel}>Cancel</button>
      </div>
    </form>
  {:else if mode === 'editing'}
    <form class="stack" onsubmit={approveEdited}>
      <label for="edit-{id}">Edited action</label>
      <textarea id="edit-{id}" rows="4" bind:value={edited} bind:this={editBox} disabled={sending !== null}></textarea>
      <div class="buttons">
        <button type="submit" disabled={sending !== null}>{sending === 'approve' ? 'Approving' : 'Approve edited'}</button>
        <button type="button" disabled={sending !== null} onclick={cancel}>Cancel</button>
      </div>
    </form>
  {:else if approval.question}
    <form class="stack" onsubmit={sendAnswer} novalidate>
      <label for="answer-{id}">Answer</label>
      <textarea id="answer-{id}" rows="3" bind:value={answer} disabled={sending !== null}></textarea>
      {#if answerError !== null}
        <p class="error" role="alert">{answerError}</p>
      {/if}
      <div class="buttons">
        <button type="submit" disabled={sending !== null} aria-describedby="who-{id}">
          {sending === 'answer' ? 'Sending answer' : 'Send answer'}
        </button>
        <button
          type="button"
          disabled={sending !== null}
          aria-describedby="who-{id}"
          bind:this={rejectButton}
          onclick={() => open('rejecting')}
        >
          Reject
        </button>
      </div>
    </form>
  {:else}
    <!-- Described by who asks, so the buttons of different rows are told apart. -->
    <div class="buttons">
      <button type="button" disabled={sending !== null} aria-describedby="who-{id}" onclick={approve}>
        {sending === 'approve' ? 'Approving' : 'Approve'}
      </button>
      <button
        type="button"
        disabled={sending !== null}
        aria-describedby="who-{id}"
        bind:this={rejectButton}
        onclick={() => open('rejecting')}
      >
        Reject
      </button>
      <button
        type="button"
        disabled={sending !== null}
        aria-describedby="who-{id}"
        bind:this={editButton}
        onclick={() => open('editing')}
      >
        Edit and approve
      </button>
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
