<script lang="ts">
  // The run's commands, in three parts the Run view places apart: `buttons` (Pause or Resume, and
  // Cancel run after a confirmation that names the live child runs it also cancels), `limit` (the
  // Change limit panel) and `message` (the Message panel, whose box shows only while the run is running
  // and its agent takes messages, and which otherwise says why). Each is enabled only when the run's
  // status allows it; Cancel run also stays on for an ended run while `live` runs below it go on. A refused command shows the server's reason in its own part, and
  // `onRefused` fetches the run again so the controls match.
  import type { Api } from '../lib/api';
  import type { FleetRun } from '../lib/fleet.svelte';
  import { isTerminal } from '../lib/format';
  import { failureText, type OnUnauthorized } from '../lib/load';
  import { allowed, MAX_MESSAGE_BYTES, messageRefusal, parseUsd } from '../lib/run';

  let {
    part,
    run,
    live = 0,
    api,
    onUnauthorized,
    onRefused,
    onBudget = () => {},
  }: {
    part: 'buttons' | 'limit' | 'message';
    run: FleetRun;
    live?: number;
    api: Api;
    onUnauthorized: OnUnauthorized;
    onRefused: () => void;
    onBudget?: () => void;
  } = $props();

  const can = $derived(allowed(run.status, live));
  const ended = $derived(isTerminal(run.status));
  // The Message box shows only while the run can take a message; otherwise this says why.
  const noMessage = $derived(messageRefusal(run.status, run.takes_messages));

  let refused: string | null = $state(null);
  let confirming = $state(false);
  let budget = $state('');
  let budgetError: string | null = $state(null);
  let message = $state('');
  /** Said once a message is stored: the agent reads it when it next checks. */
  let sent = $state('');
  /** A command is on its way: another click sends nothing. */
  let busy = false;

  let group: HTMLElement | undefined = $state();
  let dialog: HTMLElement | undefined = $state();

  let cancelButton: HTMLButtonElement | undefined = $state();
  let keepButton: HTMLButtonElement | undefined = $state();
  let confirmButton: HTMLButtonElement | undefined = $state();

  async function send(command: () => Promise<void>): Promise<boolean> {
    if (busy) return false;
    busy = true;
    refused = null;
    try {
      await command();
      return true;
    } catch (e) {
      refused = failureText(e, onUnauthorized);
      onRefused();
      return false;
    } finally {
      busy = false;
    }
  }

  // One button that turns from Pause into Resume and back, so focus stays on it when the status
  // changes under it.
  function pauseOrResume() {
    void send(() => (can.resume ? api.resume(run.id) : api.pause(run.id)));
  }

  $effect(() => {
    if (confirming) keepButton?.focus();
  });

  // The run ended while the dialog was open: there is nothing left to cancel.
  $effect(() => {
    if (confirming && !can.cancel) close();
  });

  /** Closes the dialog; focus goes back to Cancel run, or to the controls if it is now off. */
  function close() {
    confirming = false;
    if (cancelButton && !cancelButton.disabled) cancelButton.focus();
    else group?.focus();
  }

  function confirmCancel() {
    close();
    void send(() => api.cancel(run.id));
  }

  /**
   * While the dialog is open, from anywhere on the page: Esc closes it, and Tab and Shift+Tab move
   * between its two buttons only.
   */
  function onKey(e: KeyboardEvent) {
    if (!confirming) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      close();
    } else if (e.key === 'Tab') {
      e.preventDefault();
      const order = [keepButton, confirmButton];
      const at = order.indexOf(document.activeElement as HTMLButtonElement);
      // Two buttons: the next one either way is the other one; from elsewhere, Tab enters at the
      // first and Shift+Tab at the last.
      const next = at === -1 ? (e.shiftKey ? 1 : 0) : 1 - at;
      order[next]?.focus();
    }
  }

  /** Focus that lands outside the open dialog is brought back into it. */
  function onFocusIn(e: FocusEvent) {
    if (confirming && dialog && !dialog.contains(e.target as Node)) keepButton?.focus();
  }

  // Only the part that owns the cancel dialog listens on the window, not all three.
  $effect(() => {
    if (part !== 'buttons') return;
    window.addEventListener('keydown', onKey);
    window.addEventListener('focusin', onFocusIn);
    return () => {
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('focusin', onFocusIn);
    };
  });

  function setBudget(e: SubmitEvent) {
    e.preventDefault();
    if (parseUsd(budget) === null) {
      budgetError = 'Enter an amount in dollars, like 2.50, with at most 6 decimal places.';
      return;
    }
    budgetError = null;
    void send(() => api.setBudget(run.id, budget.trim())).then((ok) => {
      if (!ok) return;
      budget = '';
      onBudget();
    });
  }

  async function sendMessage(e: SubmitEvent) {
    e.preventDefault();
    const text = message.trim();
    if (text === '') return;
    sent = '';
    if (await send(() => api.message(run.id, text))) {
      message = '';
      sent = 'Sent. The agent reads it the next time it checks for messages.';
    }
  }

  const children = (n: number) => (n === 1 ? '1 child run' : `${n} child runs`);
</script>

{#if part === 'buttons'}
  <div class="controls" role="group" aria-label="Controls" tabindex="-1" bind:this={group}>
    <div class="buttons">
      <button type="button" disabled={!can.pause && !can.resume} onclick={pauseOrResume}>
        {can.resume ? 'Resume' : 'Pause'}
      </button>
      <button
        type="button"
        class="danger"
        bind:this={cancelButton}
        disabled={!can.cancel}
        onclick={() => (confirming = true)}>Cancel run</button
      >
    </div>
    {#if refused !== null}
      <p class="error" role="alert">{refused}</p>
    {/if}
  </div>

  {#if confirming}
    <div class="backdrop">
      <div
        class="dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="cancel-title-{run.id}"
        aria-describedby="cancel-text-{run.id}"
        tabindex="-1"
        bind:this={dialog}
      >
        <h2 id="cancel-title-{run.id}">Cancel this run?</h2>
        <p id="cancel-text-{run.id}">
          {#if ended}
            {`${run.agent} has ended. This cancels its ${children(live)}.`}
          {:else}
            {live === 0 ? `This cancels ${run.agent}.` : `This cancels ${run.agent} and ${children(live)}.`}
          {/if}
        </p>
        <div class="buttons">
          <button type="button" bind:this={keepButton} onclick={close}>Keep running</button>
          <button type="button" class="danger" bind:this={confirmButton} onclick={confirmCancel}>Yes, cancel it</button>
        </div>
      </div>
    </div>
  {/if}
{:else if part === 'limit'}
  <form class="panel" onsubmit={setBudget} novalidate>
    <label for="budget-{run.id}">Change limit (USD)</label>
    <div class="inline">
      <input
        id="budget-{run.id}"
        class="mono"
        inputmode="decimal"
        autocomplete="off"
        bind:value={budget}
        disabled={!can.budget}
        aria-invalid={budgetError !== null}
        aria-describedby={budgetError === null ? `budget-help-${run.id}` : `budget-error-${run.id} budget-help-${run.id}`}
      />
      <button type="submit" class="primary" disabled={!can.budget}>Set</button>
    </div>
    {#if budgetError !== null}
      <p id="budget-error-{run.id}" class="error" role="alert">{budgetError}</p>
    {/if}
    <p id="budget-help-{run.id}" class="help">Sub-agents draw from this limit. Their total can never go above it.</p>
    {#if refused !== null}
      <p class="error" role="alert">{refused}</p>
    {/if}
  </form>
{:else}
  <section class="panel message" aria-label="Message to the agent">
    {#if noMessage === null}
      <form onsubmit={sendMessage}>
        <label for="message-{run.id}">Message</label>
        <div class="inline">
          <input id="message-{run.id}" autocomplete="off" maxlength={MAX_MESSAGE_BYTES} bind:value={message} />
          <button type="submit">Send</button>
        </div>
      </form>
    {:else}
      <p class="title">Message</p>
      <p class="help">{noMessage}</p>
    {/if}
    <p class="help" role="status">{sent}</p>
    {#if refused !== null}
      <p class="error" role="alert">{refused}</p>
    {/if}
  </section>
{/if}

<style>
  .controls {
    display: grid;
    justify-items: end;
    gap: var(--space-2);
  }
  .buttons {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  form,
  .message {
    display: grid;
    gap: var(--space-3);
  }
  label,
  .title {
    font-weight: 600;
  }
  .title {
    margin: 0;
  }
  .help:empty {
    display: none;
  }
  .inline {
    display: flex;
    gap: var(--space-2);
  }
  .inline input {
    flex: 1;
    min-width: 0;
  }
  .help {
    margin: 0;
    color: var(--color-muted);
    font-size: 0.9rem;
  }
  .error {
    margin: 0;
    color: var(--color-danger);
  }
  .backdrop {
    position: fixed;
    inset: 0;
    /* Above the positioned stream and its "Jump to latest" button, which come later in the page. */
    z-index: 10;
    display: grid;
    place-items: center;
    background: rgb(0 0 0 / 55%);
  }
  .dialog {
    max-width: 28rem;
    margin: var(--space-4);
    padding: var(--space-5);
    border: 1px solid var(--color-border-strong);
    border-radius: var(--radius-lg);
    background: var(--color-surface);
    color: var(--color-text);
  }
  .dialog h2 {
    margin-top: 0;
    font-size: 1.125rem;
  }
  .dialog .buttons {
    justify-content: flex-end;
  }
</style>
