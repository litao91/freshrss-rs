<script lang="ts">
  /**
   * The notification stack, mounted once from `App.svelte`.
   *
   * `bottom-center` is where this reader has always put its confirmations, and the `--toast-*`
   * tokens in `app.css` give the card the same surface, border and shadow as the menu panels
   * beside it. The priority accent bar on the card's left edge replaces the old all-over danger
   * recolour for errors: one stripe says which kind of message this is without the whole card
   * shouting.
   */
  import { Toast } from 'svelte-widgets';
</script>

<Toast position="bottom-center" />
