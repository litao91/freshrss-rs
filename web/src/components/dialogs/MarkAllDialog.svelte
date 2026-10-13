<script lang="ts">
  /**
   * `#rd-markall-dialog`: confirms "mark all as read" for the scope on screen.
   *
   * The confirmation exists because the action is irreversible at scale — it marks every article
   * the current filters select on the *server*, not just the pages the reader has scrolled
   * through. The lead names the scope so it is visible what "all" means right now.
   */
  import { entries, markAllRead } from '../../lib/entries.svelte';
  import { refreshTree } from '../../lib/session.svelte';
  import { t, tf } from '../../lib/i18n';
  import { notify, notifyError } from '../../lib/toast.svelte';
  import { scopeTitle, view } from '../../lib/view.svelte';

  interface Props {
    /** True while `#rd-markall-dialog` should be open. */
    open: boolean;
    onclose: () => void;
  }

  let { open, onclose }: Props = $props();

  let dialogEl: HTMLDialogElement | undefined = $state();
  let busy = $state(false);

  /** Unread in the whole scope, not just the loaded pages — the number the action will touch. */
  const count = $derived(entries.unreadInScope);
  const scope = $derived(scopeTitle(view.scope));

  $effect(() => {
    if (!dialogEl) return;
    if (!open) {
      if (dialogEl.open) dialogEl.close();
    } else if (!dialogEl.open) {
      busy = false;
      dialogEl.showModal();
    }
  });

  function handleCancel(): void {
    onclose();
  }

  async function confirm(event: Event): Promise<void> {
    event.preventDefault();
    if (busy) return;
    busy = true;
    try {
      // `markAllRead` honours the same filters the list uses and reports its own errors.
      const marked = await markAllRead();
      if (marked > 0) {
        notify(t('markall.done'));
        try {
          // The sidebar badges are a separate aggregate; without this they keep the old numbers.
          await refreshTree();
        } catch (err) {
          notifyError(err instanceof Error ? err.message : t('list.countsError'));
        }
      }
      onclose();
    } finally {
      busy = false;
    }
  }
</script>

<dialog
  id="rd-markall-dialog"
  class="rd-dialog"
  bind:this={dialogEl}
  aria-labelledby="rd-markall-title"
  aria-describedby="rd-markall-scope"
  oncancel={(e) => {
    e.preventDefault();
    handleCancel();
  }}
>
  <form class="rd-dialog-body" method="dialog" onsubmit={(e) => void confirm(e)}>
    <h2 id="rd-markall-title">{tf('markall.title', { n: count })}</h2>
    <p id="rd-markall-scope" class="rd-dialog-lead">{scope}</p>

    <div class="rd-dialog-actions">
      <!-- No `autofocus` attribute (the a11y lint rejects it); `showModal()` lands focus on the
           first focusable element, which is this Cancel button, and that is the safer default
           for a destructive bulk action anyway. -->
      <button class="rd-btn rd-btn--quiet" type="button" disabled={busy} onclick={handleCancel}>
        {t('common.cancel')}
      </button>
      <button class="rd-btn rd-btn--primary" type="submit" disabled={busy}>
        <i class="ri-check-double-line" aria-hidden="true"></i>
        {busy ? t('common.saving') : t('markall.confirm')}
      </button>
    </div>
  </form>
</dialog>
