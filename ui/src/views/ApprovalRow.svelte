<script lang="ts">
  // The detail panel of one pending approval: who asks and what, how long it has waited, the run's
  // budget, and the ways to decide it. Every pending approval keeps its panel (only the selected
  // one is shown), so a half-filled form or a decision in progress survives selecting another.
  // A decision stays "in progress" until the resolved event takes the approval away; a refusal
  // because someone else decided first (409) is handed to `onGone`; any other failure puts the
  // buttons back with the error and a Retry, which sends what the fields hold then. How a decision
  // is going is also handed to `onState`, as the panel is hidden while another one is selected, and
  // a decision the server accepted to `onAccepted`.
  import { ApiFailure, type Api } from '../lib/api';
  import type { ApprovalRow, DecisionState } from '../lib/approvals.svelte';
  import { formatElapsed, formatUsd } from '../lib/format';
  import { failureText, type OnUnauthorized } from '../lib/load';
  import { href } from '../lib/router';
  import type { RunSummary } from '../lib/types';

  let {
    row,
    run,
    now,
    selected,
    api,
    onUnauthorized,
    onGone,
    onDismiss,
    onState = () => {},
    onAccepted = () => {},
  }: {
    row: ApprovalRow;
    run: RunSummary | undefined;
    now: number;
    selected: boolean;
    api: Api;
    onUnauthorized: OnUnauthorized;
    onGone: (why: 'resolved' | 'not-waiting') => void;
    onDismiss: () => void;
    onState?: (state: DecisionState) => void;
    /** The server accepted a decision on it. */
    onAccepted?: () => void;
  } = $props();

  /** Characters of the payload shown before "Show all". */
  const PREVIEW = 500;

  const approval = $derived(row.approval);
  const id = $derived(approval.id);
  const short = (runId: string) => runId.slice(-8);
  const child = $derived(run !== undefined && run.root !== run.id);
  const budget = $derived(
    run === undefined ? 'not known yet' : run.limit === null ? 'no limit' : `${formatUsd(run.spent)} spent of ${formatUsd(run.limit)}`,
  );

  let mode: 'idle' | 'rejecting' | 'editing' = $state('idle');
  let sending: 'approve' | 'reject' | 'answer' | null = $state(null);
  let error: string | null = $state(null);
  let retry: (() => void) | null = null;
  let reason = $state('');
  let edited = $state('');
  let answer = $state('');
  let answerError: string | null = $state(null);
  let expanded = $state(false);

  const decision: DecisionState = $derived(sending ?? (error !== null ? 'failed' : null));
  $effect(() => onState(decision));

  const long = $derived(approval.action.length > PREVIEW);
  const shown = $derived(long && !expanded ? `${approval.action.slice(0, PREVIEW)}...` : approval.action);

  /**
   * Sends a decision. `again` is what Retry runs: by default the same call (which reads the fields
   * when it runs), or a submit that checks the fields first. On success the panel keeps its form
   * and label ("Rejecting", "Approving") until the resolved event takes it away.
   */
  async function decide(kind: 'approve' | 'reject' | 'answer', call: () => Promise<void>, again?: () => void) {
    sending = kind;
    error = null;
    retry = again ?? (() => void decide(kind, call));
    try {
      await call();
      onAccepted();
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

  // Focus follows the panel's own changes: an opened form's field, the button that opened it after
  // Cancel, and Dismiss (or the first button) when the buttons that had focus went away.
  let panel: HTMLElement | undefined = $state();
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
    hadFocus = panel?.contains(document.activeElement) ?? false;
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
    if (hadFocus && !panel?.contains(document.activeElement)) panel?.querySelector<HTMLElement>('.buttons button')?.focus();
    hadFocus = false;
  });
</script>

<!-- `hidden` while another approval is selected: out of sight, out of the tab order and the
     accessibility tree, with its state kept. -->
<section class="panel detail" data-id={id} aria-labelledby="title-{id}" hidden={!selected} bind:this={panel}>
  <p class="eyebrow kind">{approval.question ? 'Question' : 'Command approval'}</p>
  <!-- The buttons are described by this, so a screen reader says whose approval they decide. -->
  <h2 id="title-{id}">{run?.agent ?? 'An agent'} {approval.question ? 'asks a question' : 'asks for approval'}</h2>
  <p class="who">
    <a href={href({ name: 'run', id: approval.run_id })} title={approval.run_id}>Run {short(approval.run_id)}</a>
    {#if run && child}
      <a href={href({ name: 'run', id: run.root })} title={run.root}>Root {short(run.root)}</a>
    {/if}
    <span>waiting {formatElapsed(now - Date.parse(approval.requested_at))}</span>
  </p>

  <p class="label">Action</p>
  <pre class="action" id="action-{id}">{shown}</pre>
  {#if long}
    <button type="button" class="link" aria-expanded={expanded} aria-controls="action-{id}" onclick={() => (expanded = !expanded)}>
      {expanded ? 'Show less' : 'Show all'}
    </button>
  {/if}

  <dl>
    <dt>Agent</dt>
    <dd>{run?.agent ?? 'not known yet'}</dd>
    <dt>Run budget</dt>
    <dd>{budget}</dd>
    {#if run && child}
      <dt>Root</dt>
      <dd class="mono">run {short(run.root)}</dd>
    {/if}
  </dl>

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
        <button type="submit" class="danger" disabled={sending !== null}>{sending === 'reject' ? 'Rejecting' : 'Confirm reject'}</button>
        <button type="button" disabled={sending !== null} onclick={cancel}>Cancel</button>
      </div>
    </form>
  {:else if mode === 'editing'}
    <form class="stack" onsubmit={approveEdited}>
      <label for="edit-{id}">Edited action</label>
      <textarea id="edit-{id}" rows="4" class="mono" bind:value={edited} bind:this={editBox} disabled={sending !== null}></textarea>
      <div class="buttons">
        <button type="submit" class="primary" disabled={sending !== null}>{sending === 'approve' ? 'Approving' : 'Approve edited'}</button>
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
        <button type="submit" class="primary" disabled={sending !== null} aria-describedby="title-{id}">
          {sending === 'answer' ? 'Sending answer' : 'Send answer'}
        </button>
        <button
          type="button"
          class="danger"
          disabled={sending !== null}
          aria-describedby="title-{id}"
          bind:this={rejectButton}
          onclick={() => open('rejecting')}
        >
          Reject
        </button>
      </div>
    </form>
  {:else}
    <div class="buttons">
      <button type="button" class="primary" disabled={sending !== null} aria-describedby="title-{id}" onclick={approve}>
        {sending === 'approve' ? 'Approving' : 'Approve'}
      </button>
      <button
        type="button"
        disabled={sending !== null}
        aria-describedby="title-{id}"
        bind:this={editButton}
        onclick={() => open('editing')}
      >
        Edit and approve
      </button>
      <button
        type="button"
        class="danger"
        disabled={sending !== null}
        aria-describedby="title-{id}"
        bind:this={rejectButton}
        onclick={() => open('rejecting')}
      >
        Reject
      </button>
    </div>
  {/if}

  {#if error !== null}
    <div class="failed">
      <p class="error" role="alert">{error}</p>
      <button type="button" onclick={() => retry?.()}>Retry</button>
    </div>
  {/if}
</section>

<style>
  .detail {
    display: grid;
    gap: var(--space-3);
    border-color: var(--color-approval-border);
  }
  .detail[hidden] {
    display: none;
  }
  .kind {
    margin: 0;
    color: var(--color-approval);
  }
  h2 {
    margin: 0;
    font-size: 1.5rem;
    line-height: 1.25;
    overflow-wrap: anywhere;
  }
  .who {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-1) var(--space-3);
    margin: 0;
    color: var(--color-muted);
  }
  .label {
    margin: var(--space-2) 0 0;
    font-weight: 700;
  }
  .action {
    margin: 0;
    padding: var(--space-3) var(--space-4);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    background: var(--color-sunken);
    font-family: var(--font-mono);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  dl {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr);
    gap: var(--space-2) var(--space-5);
    margin: var(--space-2) 0;
  }
  dt {
    color: var(--color-muted);
  }
  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
  .stack {
    display: grid;
    gap: var(--space-2);
  }
  .buttons,
  .failed {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-3);
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
</style>
