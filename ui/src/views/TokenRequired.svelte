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
<section class="panel">
  <p class="lead">
    Enter the API token that <code>kestrel up</code> printed. It stays in this tab's memory and is
    forgotten when you close the tab.
  </p>
  {#if refused}
    <p role="alert" class="refused">That token was refused.</p>
  {/if}
  <form onsubmit={submit}>
    <label for="token">API token</label>
    <div class="row">
      <input id="token" type="password" autocomplete="off" spellcheck="false" bind:value bind:this={input} />
      <button type="submit" class="primary">Continue</button>
    </div>
  </form>
</section>

<style>
  h1 {
    margin-top: 0;
  }
  .panel {
    max-width: 36rem;
  }
  .lead {
    margin: 0 0 var(--space-4);
    color: var(--color-muted);
  }
  form {
    display: grid;
    gap: var(--space-2);
  }
  label {
    font-weight: 600;
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  input {
    flex: 1 1 16rem;
    min-width: 0;
  }
  .refused {
    margin: 0 0 var(--space-3);
    color: var(--color-danger);
  }
</style>
