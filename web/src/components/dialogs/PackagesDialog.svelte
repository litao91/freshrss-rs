<script lang="ts">
  /**
   * `#rd-packages-dialog`: subscription starter packs.
   *
   * Packs are multi-select — the reference reader's copy says "pick one or more" — and each
   * install is one `POST /packages/{slug}/install` returning the same report shape as an OPML
   * import. Installs run one after another rather than in parallel: each creates a folder and a
   * batch of feeds, and a serial run keeps the server's writer lock uncontended and the errors
   * attributable to a single pack.
   */
  import { api, ApiError, type Package } from '../../lib/api';
  import { t, tf } from '../../lib/i18n';
  import { notify, notifyError } from '../../lib/toast.svelte';

  interface Props {
    /** True while `#rd-packages-dialog` should be open. */
    open: boolean;
    onclose: () => void;
    /** Called after installing so the sidebar and the list can pick up the new feeds. */
    onChanged: () => Promise<void> | void;
  }

  let { open, onclose, onChanged }: Props = $props();

  let dialogEl: HTMLDialogElement | undefined = $state();
  let loading = $state(false);
  let loadError = $state<string | null>(null);
  let packs = $state<Package[]>([]);
  /** Slugs of the selected cards. */
  let selected = $state<Set<string>>(new Set());
  let installing = $state(false);

  $effect(() => {
    if (!dialogEl) return;
    if (!open) {
      if (dialogEl.open) dialogEl.close();
    } else {
      if (!dialogEl.open) dialogEl.showModal();
      void load();
    }
  });

  async function load(): Promise<void> {
    loading = true;
    loadError = null;
    selected = new Set();
    try {
      packs = await api.packages();
    } catch (err) {
      packs = [];
      loadError = err instanceof ApiError ? err.message : t('packs.loadError');
    } finally {
      loading = false;
    }
  }

  function toggle(slug: string): void {
    const next = new Set(selected);
    if (next.has(slug)) next.delete(slug);
    else next.add(slug);
    selected = next;
  }

  async function install(): Promise<void> {
    if (installing || selected.size === 0) return;
    installing = true;
    let created = 0;
    const failures: string[] = [];
    // Snapshot the selection: the set is replaced, not mutated, but the loop should not chase it.
    for (const slug of [...selected]) {
      try {
        const report = await api.installPackage(slug);
        created += report.created_feeds;
        failures.push(...report.errors);
      } catch (err) {
        const pack = packs.find((p) => p.slug === slug);
        const message = err instanceof ApiError ? err.message : t('packs.installError');
        failures.push(`${pack?.name ?? slug}: ${message}`);
      }
    }
    installing = false;

    if (created > 0) {
      notify(tf('packs.done', { n: created }));
      await Promise.resolve(onChanged());
    }
    if (failures.length > 0) {
      // Stay open so the reader can see which packs failed and retry the others.
      loadError = failures.join(' — ');
      notifyError(t('packs.installError'));
      return;
    }
    onclose();
  }

  function handleCancel(): void {
    onclose();
  }
</script>

<dialog
  id="rd-packages-dialog"
  class="rd-dialog rd-dialog--wide"
  bind:this={dialogEl}
  aria-labelledby="rd-packages-title"
  oncancel={(e) => {
    e.preventDefault();
    handleCancel();
  }}
>
  <div class="rd-dialog-body">
    <h2 id="rd-packages-title">{t('packs.title')}</h2>
    <p class="rd-dialog-lead">{t('packs.lead')}</p>

    {#if loadError}
      <p class="rd-field-error" role="alert">{loadError}</p>
    {/if}

    {#if loading && packs.length === 0}
      <p class="rd-hint">
        <i class="ri-loader-4-line rd-spin" aria-hidden="true"></i>
        {t('common.loading')}
      </p>
    {:else if packs.length === 0 && !loadError}
      <p class="rd-hint">{t('packs.empty')}</p>
    {:else}
      <ul class="rd-packs" role="group" aria-labelledby="rd-packages-title">
        {#each packs as pack (pack.slug)}
          <li>
            <button
              class="rd-pack"
              type="button"
              aria-pressed={selected.has(pack.slug)}
              disabled={installing}
              onclick={() => toggle(pack.slug)}
            >
              <span class="rd-pack-body">
                <span class="rd-pack-name">{pack.name}</span>
                <span class="rd-pack-desc">{pack.description}</span>
                <span class="rd-pack-desc">{tf('packs.sources', { n: pack.feeds.length })}</span>
              </span>
            </button>
          </li>
        {/each}
      </ul>
    {/if}

    <div class="rd-dialog-actions">
      <button class="rd-btn rd-btn--quiet" type="button" disabled={installing} onclick={handleCancel}>
        {t('common.cancel')}
      </button>
      <button
        class="rd-btn rd-btn--primary"
        type="button"
        disabled={installing || selected.size === 0}
        onclick={() => void install()}
      >
        {#if installing}
          <i class="ri-loader-4-line rd-spin" aria-hidden="true"></i>
          {t('packs.installing')}
        {:else}
          {tf('packs.subscribe', { n: selected.size })}
        {/if}
      </button>
    </div>
  </div>
</dialog>
