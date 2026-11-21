<script lang="ts">
  import { href } from '../lib/router';
</script>

<div data-testid="ready">
  <h1>Not found</h1>
  <p>There is no page at this address.</p>
  <p><a href={href({ name: 'fleet' })}>Go to Fleet</a></p>
</div>
