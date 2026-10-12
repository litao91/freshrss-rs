<script lang="ts">
  import { toast } from '../lib/toast.svelte';
</script>

<div class="rd-toast" class:is-visible={toast.visible} class:is-error={toast.kind === 'error'} role="status">
  {#if toast.message}
    <!-- `key` forces the transition to replay when the same text is shown twice in a row. -->
    {#key toast.token}
      {toast.message}
    {/key}
  {/if}
</div>
