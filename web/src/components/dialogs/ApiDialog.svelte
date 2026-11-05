<script lang="ts">
  /**
   * `#rd-api-dialog`: sets the password that Google Reader and Fever compatible clients use.
   *
   * Deliberately a separate credential from the web login: it travels to third-party apps and
   * derives two non-expiring keys, so rotating it should be a small, explicit act — and the dialog
   * says which addresses and username those apps want, because that is the half people get wrong
   * when configuring a client.
   */
  import { api, ApiError } from '../../lib/api';
  import { t } from '../../lib/i18n';
  import { session } from '../../lib/session.svelte';
  import { notify } from '../../lib/toast.svelte';

  interface Props {
    open: boolean;
    onclose: () => void;
  }

  let { open, onclose }: Props = $props();

  let dialogEl: HTMLDialogElement | undefined = $state();
  let password = $state('');
  let busy = $state(false);
  let formError = $state<string | null>(null);

  const greaderUrl = $derived(`${location.origin}/api/greader.php`);
  const feverUrl = $derived(`${location.origin}/api/fever.php`);

  $effect(() => {
    if (open && !dialogEl?.open) {
      dialogEl?.showModal();
      password = '';
      formError = null;
    } else if (!open && dialogEl?.open) {
      dialogEl?.close();
    }
  });

  async function submit(event: Event): Promise<void> {
    event.preventDefault();
    if (busy) return;
    busy = true;
    formError = null;
    try {
      await api.setApiPassword(password);
      notify(t('api.set'));
      password = '';
      onclose();
    } catch (err) {
      formError = err instanceof ApiError ? err.message : t('api.setError');
    } finally {
      busy = false;
    }
  }
</script>

<dialog
  id="rd-api-dialog"
  class="rd-dialog"
  bind:this={dialogEl}
  aria-labelledby="rd-api-title"
  oncancel={(e) => {
    e.preventDefault();
    onclose();
  }}
>
  <form class="rd-dialog-body" method="dialog" onsubmit={(e) => void submit(e)}>
    <h2 id="rd-api-title">{t('api.title')}</h2>
    <p class="rd-dialog-lead">{t('api.lead')}</p>

    <div class="rd-field-row">
      <label class="rd-label" for="rd-api-password">{t('api.password')}</label>
      <input
        id="rd-api-password"
        class="rd-input"
        type="password"
        autocomplete="new-password"
        minlength={8}
        required
        bind:value={password}
      />
    </div>

    {#if formError}
      <p class="rd-field-error" role="alert">{formError}</p>
    {/if}

    <dl class="rd-api-endpoints">
      <dt>Google Reader</dt>
      <dd><code>{greaderUrl}</code></dd>
      <dt>Fever</dt>
      <dd><code>{feverUrl}</code></dd>
      <dt>{t('api.username')}</dt>
      <dd><code>{session.user?.username ?? ''}</code></dd>
    </dl>

    <div class="rd-dialog-actions">
      <button class="rd-btn rd-btn--quiet" type="button" disabled={busy} onclick={onclose}>
        {t('common.cancel')}
      </button>
      <button class="rd-btn rd-btn--primary" type="submit" disabled={busy}>
        {busy ? t('common.saving') : t('api.save')}
      </button>
    </div>
  </form>
</dialog>

<style>
  .rd-api-endpoints {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.3rem 0.9rem;
    margin: 1rem 0 0;
    font-size: 0.85rem;
  }

  .rd-api-endpoints dt {
    color: var(--fd-muted);
  }

  .rd-api-endpoints dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
</style>
