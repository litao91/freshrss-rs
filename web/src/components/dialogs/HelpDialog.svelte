<script lang="ts">
  /**
   * `#rd-help-dialog`: the keyboard-shortcut table.
   *
   * The rows come from `SHORTCUTS` in `lib/keys.ts` rather than a copy here, so a binding added
   * there shows up in the dialog automatically — that file is the single source of truth for both
   * the handler and this table. English descriptions are those from `keys.ts` (passed as the `t()`
   * fallback); the dictionary only carries translations.
   */
  import { SHORTCUTS, type Shortcut } from '../../lib/keys';
  import { t } from '../../lib/i18n';

  interface Props {
    /** True while `#rd-help-dialog` should be open. */
    open: boolean;
    onclose: () => void;
  }

  let { open, onclose }: Props = $props();

  let dialogEl: HTMLDialogElement | undefined = $state();

  $effect(() => {
    if (!dialogEl) return;
    if (!open) {
      if (dialogEl.open) dialogEl.close();
    } else if (!dialogEl.open) {
      dialogEl.showModal();
    }
  });

  /**
   * Three groups rather than the two `keys.ts` flags: a binding that applies in both contexts is
   * listed once under "Anywhere" instead of twice, which is how the reference table reads.
   */
  const groups = $derived.by(() => [
    { label: t('help.groupAnywhere'), shortcuts: SHORTCUTS.filter((s) => s.list && s.reading) },
    { label: t('help.groupList'), shortcuts: SHORTCUTS.filter((s) => s.list && !s.reading) },
    { label: t('help.groupReading'), shortcuts: SHORTCUTS.filter((s) => s.reading && !s.list) },
  ]);

  /** `Escape` shows as `Esc`, as the reference table does; everything else is the literal key. */
  function keyLabel(key: string): string {
    return key === 'Escape' ? 'Esc' : key;
  }

  function description(shortcut: Shortcut): string {
    return t(`shortcut.${shortcut.key}`, shortcut.description);
  }

  function handleCancel(): void {
    onclose();
  }
</script>

<dialog
  id="rd-help-dialog"
  class="rd-dialog"
  bind:this={dialogEl}
  aria-labelledby="rd-help-title"
  oncancel={(e) => {
    e.preventDefault();
    handleCancel();
  }}
  // The Close button is a `method="dialog"` submit, which closes the element natively; this is
  // what tells the parent, so its `open` state cannot drift from the element.
  onclose={handleCancel}
>
  <div class="rd-dialog-body">
    <h2 id="rd-help-title">{t('help.title')}</h2>

    {#each groups as group (group.label)}
      {#if group.shortcuts.length > 0}
        <p class="rd-menu-label">{group.label}</p>
        <dl class="rd-keys">
          {#each group.shortcuts as shortcut (shortcut.key)}
            <dt>
              {#if shortcut.shift}
                <kbd>⇧</kbd>
              {/if}
              <kbd>{keyLabel(shortcut.key)}</kbd>
            </dt>
            <dd>{description(shortcut)}</dd>
          {/each}
        </dl>
      {/if}
    {/each}

    <form method="dialog" class="rd-dialog-actions">
      <button type="submit" class="rd-btn rd-btn--primary" onclick={handleCancel}>
        {t('common.close')}
      </button>
    </form>
  </div>
</dialog>
