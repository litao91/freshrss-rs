<script lang="ts">
  /**
   * `#rd-import-dialog`: OPML import with a dry-run preview first.
   *
   * The preview exists because an import is a bulk mutation: `dry_run=true` runs the same parse
   * and the same duplicate checks but changes nothing, so the reader sees "42 added, 7 skipped,
   * these 3 failed" *before* committing. The real import is a second POST with `dry_run=false`.
   *
   * The body is the file's raw bytes, not a multipart form — that is the endpoint's contract.
   */
  import { api, ApiError, type ImportReport } from '../../lib/api';
  import { t, tf } from '../../lib/i18n';
  import { tree } from '../../lib/session.svelte';
  import { notify } from '../../lib/toast.svelte';

  interface Props {
    /** True while `#rd-import-dialog` should be open. */
    open: boolean;
    onclose: () => void;
    /** Called after a real import so the sidebar and the list can pick up the new feeds. */
    onChanged: () => Promise<void> | void;
  }

  let { open, onclose, onChanged }: Props = $props();

  /** The reference reader's ceiling; the server enforces its own, this just fails faster. */
  const MAX_BYTES = 2 * 1024 * 1024;

  let dialogEl: HTMLDialogElement | undefined = $state();
  let fileInput: HTMLInputElement | undefined = $state();
  let file = $state<File | null>(null);
  /** `''` keeps the file's own folders; an id files everything under one category. */
  let forceCategory = $state('');
  let busy = $state<'idle' | 'preview' | 'import'>('idle');
  let formError = $state<string | null>(null);
  /** The dry-run answer; `null` until a preview succeeds. The real import requires one. */
  let preview = $state<ImportReport | null>(null);

  const categories = $derived(tree.data?.categories ?? []);

  $effect(() => {
    if (!dialogEl) return;
    if (!open) {
      if (dialogEl.open) dialogEl.close();
    } else {
      reset();
      if (!dialogEl.open) dialogEl.showModal();
    }
  });

  function reset(): void {
    file = null;
    forceCategory = '';
    busy = 'idle';
    formError = null;
    preview = null;
    if (fileInput) fileInput.value = '';
  }

  function onFilePicked(): void {
    const picked = fileInput?.files?.[0] ?? null;
    file = picked;
    preview = null;
    formError = null;
    if (picked && picked.size > MAX_BYTES) {
      formError = t('import.tooBig');
      file = null;
    }
  }

  async function send(dryRun: boolean): Promise<void> {
    if (!file) {
      formError = t('import.pickFile');
      return;
    }
    busy = dryRun ? 'preview' : 'import';
    formError = null;
    try {
      const bytes = await file.arrayBuffer();
      const report = await api.importOpml(bytes, dryRun, forceCategory === '' ? undefined : forceCategory);
      if (dryRun) {
        preview = report;
      } else {
        notify(tf('import.done', { n: report.created_feeds }));
        await onChanged();
        onclose();
      }
    } catch (err) {
      // Shown in the dialog rather than as a toast: the reader is mid-flow and the message
      // belongs next to the file that caused it.
      formError = err instanceof ApiError ? err.message : t('import.error');
      if (!dryRun) preview = null;
    } finally {
      busy = 'idle';
    }
  }

  /** One line per non-zero part of a report, so "0 feeds added" noise stays out. */
  function summary(report: ImportReport): string[] {
    const lines: string[] = [];
    if (report.created_feeds > 0) lines.push(tf('import.createdFeeds', { n: report.created_feeds }));
    if (report.skipped_feeds > 0) lines.push(tf('import.skippedFeeds', { n: report.skipped_feeds }));
    if (report.created_categories > 0) {
      lines.push(tf('import.createdCategories', { n: report.created_categories }));
    }
    return lines;
  }

  function handleCancel(): void {
    onclose();
  }
</script>

<dialog
  id="rd-import-dialog"
  class="rd-dialog"
  bind:this={dialogEl}
  aria-labelledby="rd-import-title"
  oncancel={(e) => {
    e.preventDefault();
    handleCancel();
  }}
>
  <div class="rd-dialog-body">
    <h2 id="rd-import-title">{t('import.title')}</h2>
    <p class="rd-dialog-lead">{t('import.lead')}</p>

    <label class="rd-label" for="rd-import-file">{t('import.file')}</label>
    <input
      id="rd-import-file"
      class="rd-input"
      type="file"
      accept=".opml,.xml,text/x-opml,text/xml,application/xml"
      bind:this={fileInput}
      onchange={onFilePicked}
    />

    <label class="rd-label" for="rd-import-folder">{t('import.folder')}</label>
    <select
      id="rd-import-folder"
      class="rd-input"
      bind:value={forceCategory}
      // A different destination invalidates the preview: it promised a result for the old one.
      onchange={() => (preview = null)}
    >
      <option value="">{t('import.keepFolders')}</option>
      {#each categories as category (category.id)}
        <option value={category.id}>{category.name}</option>
      {/each}
    </select>

    {#if preview}
      <div class="rd-report" role="status">
        {#each summary(preview) as line (line)}
          <p class="rd-hint">{line}</p>
        {/each}
        {#if preview.errors.length > 0}
          <p class="rd-field-error">{t('import.errorsTitle')}</p>
          <ul class="rd-report-errors">
            {#each preview.errors as error, i (i)}
              <li>{error}</li>
            {/each}
          </ul>
        {/if}
        <p class="rd-hint">{t('import.previewNote')}</p>
      </div>
    {/if}

    {#if formError}
      <p class="rd-field-error" role="alert">{formError}</p>
    {/if}

    <div class="rd-dialog-actions">
      <button class="rd-btn rd-btn--quiet" type="button" disabled={busy !== 'idle'} onclick={handleCancel}>
        {t('common.cancel')}
      </button>
      <button
        class="rd-btn"
        type="button"
        disabled={busy !== 'idle' || !file}
        onclick={() => void send(true)}
      >
        <i class="ri-eye-line" aria-hidden="true"></i>
        {busy === 'preview' ? t('import.previewing') : t('import.preview')}
      </button>
      <button
        class="rd-btn rd-btn--primary"
        type="button"
        disabled={busy !== 'idle' || !preview}
        onclick={() => void send(false)}
      >
        {busy === 'import' ? t('import.importing') : t('import.import')}
      </button>
    </div>
  </div>
</dialog>
