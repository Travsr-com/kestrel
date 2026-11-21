<script lang="ts">
  // Pause, Resume, Cancel (after a confirmation that names the live child runs it also cancels),
  // Set budget and Send message. Each is enabled only when the run's status allows it. A refused
  // command shows the server's reason, and `onRefused` fetches the run again so the buttons match.
  import type { Api } from '../lib/api';
  import type { FleetRun } from '../lib/fleet.svelte';
  import { failureText, type OnUnauthorized } from '../lib/load';
  import { allowed, parseUsd } from '../lib/run';

  let {
    run,
    live,
    api,
    onUnauthorized,
    onRefused,
  }: { run: FleetRun; live: number; api: Api; onUnauthorized: OnUnauthorized; onRefused: () => void } = $props();

  const can = $derived(allowed(run.status));

  let refused: string | null = $state(null);
  let confirming = $state(false);
  let budget = $state('');
  let budgetError: string | null = $state(null);
  let message = $state('');

  let cancelButton: HTMLButtonElement | undefined = $state();
  let keepButton: HTMLButtonElement | undefined = $state();
  let confirmButton: HTMLButtonElement | undefined = $state();

  async function send(command: () => Promise<void>): Promise<boolean> {
    refused = null;
    try {
      await command();
      return true;
    } catch (e) {
      refused = failureText(e, onUnauthorized);
      onRefused();
      return false;
    }
  }

  $effect(() => {
    if (confirming) keepButton?.focus();
  });

  function close() {
    confirming = false;
    cancelButton?.focus();
  }

  function confirmCancel() {
    close();
    void send(() => api.cancel(run.id));
  }

  /** Esc closes the dialog; Tab and Shift+Tab stay on its two buttons. */
  function onDialogKey(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      close();
    } else if (e.key === 'Tab') {
      const leaving = e.shiftKey ? keepButton : confirmButton;
      if (document.activeElement === leaving) {
        e.preventDefault();
        (e.shiftKey ? confirmButton : keepButton)?.focus();
      }
    }
  }

  function setBudget(e: SubmitEvent) {
    e.preventDefault();
    if (parseUsd(budget) === null) {
      budgetError = 'Enter an amount in dollars, like 2.50, with at most 6 decimal places.';
      return;
    }
    budgetError = null;
    void send(() => api.setBudget(run.id, budget.trim()));
  }

  async function sendMessage(e: SubmitEvent) {
    e.preventDefault();
    const text = message.trim();
    if (text === '') return;
    if (await send(() => api.message(run.id, text))) message = '';
  }

  const children = (n: number) => (n === 1 ? '1 child run' : `${n} child runs`);
</script>

<div class="controls" role="group" aria-label="Controls">
  <div class="buttons">
    <button type="button" disabled={!can.pause} onclick={() => send(() => api.pause(run.id))}>Pause</button>
    <button type="button" disabled={!can.resume} onclick={() => send(() => api.resume(run.id))}>Resume</button>
    <button type="button" bind:this={cancelButton} disabled={!can.cancel} onclick={() => (confirming = true)}>
      Cancel
    </button>
  </div>

  <form class="inline" onsubmit={setBudget} novalidate>
    <label for="budget-{run.id}">New budget (USD)</label>
    <input
      id="budget-{run.id}"
      inputmode="decimal"
      autocomplete="off"
      bind:value={budget}
      disabled={!can.budget}
      aria-invalid={budgetError !== null}
      aria-describedby={budgetError === null ? undefined : `budget-error-${run.id}`}
    />
    <button type="submit" disabled={!can.budget}>Set budget</button>
  </form>
  {#if budgetError !== null}
    <p id="budget-error-{run.id}" class="error" role="alert">{budgetError}</p>
  {/if}

  <form class="inline" onsubmit={sendMessage}>
    <label for="message-{run.id}">Message</label>
    <input id="message-{run.id}" autocomplete="off" bind:value={message} disabled={!can.message} />
    <button type="submit" disabled={!can.message}>Send</button>
  </form>

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
      onkeydown={onDialogKey}
    >
      <h2 id="cancel-title-{run.id}">Cancel this run?</h2>
      <p id="cancel-text-{run.id}">
        {live === 0 ? `This cancels ${run.agent}.` : `This cancels ${run.agent} and ${children(live)}.`}
      </p>
      <div class="buttons">
        <button type="button" bind:this={keepButton} onclick={close}>Keep running</button>
        <button type="button" class="danger" bind:this={confirmButton} onclick={confirmCancel}>Cancel run</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .controls {
    display: grid;
    gap: var(--space-2);
  }
  .buttons,
  .inline {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }
  button,
  input {
    font: inherit;
  }
  .error {
    margin: 0;
    color: var(--color-danger);
  }
  .backdrop {
    position: fixed;
    inset: 0;
    display: grid;
    place-items: center;
    background: rgb(0 0 0 / 40%);
  }
  .dialog {
    max-width: 28rem;
    padding: var(--space-4);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    background: var(--color-bg);
    color: var(--color-text);
  }
  .dialog h2 {
    margin-top: 0;
    font-size: 1.125rem;
  }
  .danger {
    color: var(--color-danger);
  }
</style>
