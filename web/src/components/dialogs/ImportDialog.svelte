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
  import { Dialog, FileInput } from 'svelte-widgets';
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

  /** Mirrors the parent's flag so `Dialog` can close itself while the parent owns whether it is up. */
  let shown = $state(false);
  /** What `FileInput` holds; the dialog only ever reads the first entry. */
  let picked = $state<File[]>([]);
  let file = $state<File | null>(null);
  /** `''` keeps the file's own folders; an id files everything under one category. */
  let forceCategory = $state('');
  let busy = $state<'idle' | 'preview' | 'import'>('idle');
  let formError = $state<string | null>(null);
  /** The dry-run answer; `null` until a preview succeeds. The real import requires one. */
  let preview = $state<ImportReport | null>(null);

  const categories = $derived(tree.data?.categories ?? []);

  $effect(() => {
    shown = open;
    if (open) reset();
  });

  function reset(): void {
    file = null;
    forceCategory = '';
    busy = 'idle';
    formError = null;
    preview = null;
    picked = [];
    file = null;
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

<Dialog
  id="rd-import-dialog"
  class="rd-dialog"
  bind:open={shown}
  closedby="closerequest"
  aria-labelledby="rd-import-title"
  on_close={handleCancel}
>
  <div class="rd-dialog-body">
    <h2 id="rd-import-title">{t('import.title')}</h2>
    <p class="rd-dialog-lead">{t('import.lead')}</p>

    <!-- Drag-and-drop, the picked-file list and the refusal of an over-large file come with the
         widget; `max_size` is the same ceiling the hand-rolled check enforced, now refused before
         the reader waits for a preview that cannot succeed. -->
    <FileInput
      class="rd-file-input"
      accept=".opml,.xml,text/x-opml,text/xml,application/xml"
      max_size={MAX_BYTES}
      label={t('import.file')}
      remove_label={t('common.cancel')}
      bind:files={picked}
      on_files={(files) => {
        file = files[0] ?? null;
        preview = null;
        formError = null;
      }}
      on_reject={(rejections) => {
        // The widget names the refused file and why in its own words; the dialog's error slot says
        // the same thing in the reader's language, which is what the rest of the flow speaks.
        file = null;
        preview = null;
        formError = rejections.some((r) => r.reason === 'size')
          ? t('import.tooBig')
          : t('import.error');
      }}
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
</Dialog>
