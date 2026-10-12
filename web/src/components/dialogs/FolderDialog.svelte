<script lang="ts">
  import { api, ApiError } from '../../lib/api';
  import { notify } from '../../lib/toast.svelte';

  interface Props {
    /**
     * `undefined` closes the dialog, `null` opens it in "create" mode, and a folder object opens it
     * to edit that folder. Three states rather than two because "closed" and "create" must be
     * distinguishable.
     */
    folder: import('../../lib/api').CategoryNode | null | undefined;
    onclose: () => void;
    /** Called after a create/rename/delete so the sidebar can refresh. */
    onChanged: () => Promise<void> | void;
  }

  let { folder, onclose, onChanged }: Props = $props();

  let dialogEl: HTMLDialogElement | undefined = $state();
  let name = $state('');
  let busy = $state(false);
  let formError = $state<string | null>(null);

  const editing = $derived(folder !== undefined && folder !== null);
  const title = $derived(editing ? 'Folder settings' : 'New folder');

  $effect(() => {
    if (!dialogEl) return;
    if (folder === undefined) {
      if (dialogEl.open) dialogEl.close();
    } else {
      name = folder?.name ?? '';
      formError = null;
      if (!dialogEl.open) dialogEl.showModal();
    }
  });

  function handleCancel(): void {
    onclose();
  }

  async function submit(event: Event): Promise<void> {
    event.preventDefault();
    if (busy) return;

    const trimmed = name.trim();
    if (!trimmed) {
      formError = 'Give the folder a name.';
      return;
    }

    busy = true;
    formError = null;
    try {
      if (editing && folder) {
        await api.renameCategory(folder.id, trimmed);
        notify(`Renamed to “${trimmed}”`);
      } else {
        await api.createCategory(trimmed);
        notify(`Created “${trimmed}”`);
      }
      await onChanged();
      onclose();
    } catch (err) {
      formError = err instanceof ApiError ? err.message : 'Could not save the folder.';
    } finally {
      busy = false;
    }
  }

  async function remove(): Promise<void> {
    if (!folder || busy) return;
    busy = true;
    formError = null;
    try {
      const result = await api.deleteCategory(folder.id);
      // Deleting a folder does not delete its feeds — they become orphans. Saying so matters,
      // because the user's instinct is that the subscriptions went with the folder.
      notify(
        result.orphaned_feeds > 0
          ? `Deleted “${folder.name}”. Its ${result.orphaned_feeds} subscription${
              result.orphaned_feeds === 1 ? '' : 's'
            } moved out of the folder.`
          : `Deleted “${folder.name}”`,
      );
      await onChanged();
      onclose();
    } catch (err) {
      formError = err instanceof ApiError ? err.message : 'Could not delete the folder.';
    } finally {
      busy = false;
    }
  }
</script>

<dialog
  id="rd-folder-dialog"
  class="rd-dialog"
  bind:this={dialogEl}
  aria-labelledby="rd-folder-title"
  oncancel={(e) => {
    e.preventDefault();
    handleCancel();
  }}
>
  <form class="rd-dialog-body" method="dialog" onsubmit={submit}>
    <h2 id="rd-folder-title">{title}</h2>

    {#if editing}
      <p class="rd-dialog-lead">
        {folder?.feeds.length ?? 0} subscription{(folder?.feeds.length ?? 0) === 1 ? '' : 's'} in this folder.
      </p>
    {:else}
      <p class="rd-dialog-lead">Folders group subscriptions in the sidebar.</p>
    {/if}

    <label class="rd-label" for="rd-folder-name">Name</label>
    <input
      id="rd-folder-name"
      class="rd-input"
      type="text"
      autocomplete="off"
      maxlength="191"
      required
      bind:value={name}
    />

    {#if formError}
      <p class="rd-field-error" role="alert">{formError}</p>
    {/if}

    <div class="rd-dialog-actions">
      {#if editing}
        <button class="rd-btn rd-btn--quiet rd-btn--danger" type="button" disabled={busy} onclick={remove}>
          <i class="ri-delete-bin-line" aria-hidden="true"></i>
          Delete folder
        </button>
      {/if}
      <button class="rd-btn rd-btn--quiet" type="button" disabled={busy} onclick={handleCancel}>Cancel</button>
      <button class="rd-btn rd-btn--primary" type="submit" disabled={busy}>
        {#if busy}Working…{:else}{editing ? 'Save' : 'Create'}{/if}
      </button>
    </div>
  </form>
</dialog>
