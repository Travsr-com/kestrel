<script lang="ts">
  let { refused, onSubmit }: { refused: boolean; onSubmit: (token: string) => void } = $props();

  let value = $state('');
  let input: HTMLInputElement | undefined = $state();

  // Focus the field when the screen appears, without the `autofocus` attribute.
  $effect(() => {
    input?.focus();
  });

  function submit(event: SubmitEvent) {
    event.preventDefault();
    const token = value.trim();
    if (token !== '') onSubmit(token);
  }
</script>

<h1>Token required</h1>
<p>
  Enter the API token that <code>kestrel up</code> printed. It stays in this tab's memory and is
  forgotten when you close the tab.
</p>
{#if refused}
  <p role="alert" class="refused">That token was refused.</p>
{/if}
<form onsubmit={submit}>
  <label for="token">API token</label>
  <input id="token" type="password" autocomplete="off" spellcheck="false" bind:value bind:this={input} />
  <button type="submit">Continue</button>
</form>

<style>
  form {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }
  input {
    min-width: 20rem;
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    background: var(--color-bg);
    color: var(--color-text);
    font: inherit;
  }
  button {
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--color-accent);
    border-radius: var(--radius);
    background: var(--color-accent);
    color: var(--color-on-accent);
    font: inherit;
    cursor: pointer;
  }
  .refused {
    color: var(--color-danger);
  }
</style>
