<script lang="ts">
  import { onMount } from 'svelte';
  import type { CategoryNode, FeedNode } from './lib/api';
  import { t } from './lib/i18n';
  import { initPrefs, setNavOpen, toggleFocus, toggleNav } from './lib/prefs.svelte';
  import { bootstrap, isSignedIn, refreshTree, session, tree } from './lib/session.svelte';
  import { notifyError } from './lib/toast.svelte';
  import { scopeTitle, selectEntry, selectScope, toggleReadFilter, view, isReading, type Scope } from './lib/view.svelte';

  import Auth from './components/Auth.svelte';
  import EntryList from './components/EntryList.svelte';
  import NavTree from './components/NavTree.svelte';
  import ReadingPane from './components/ReadingPane.svelte';
  import Toast from './components/Toast.svelte';
  import TopBar from './components/TopBar.svelte';
  import AddDialog from './components/dialogs/AddDialog.svelte';
  import FeedDialog from './components/dialogs/FeedDialog.svelte';
  import FolderDialog from './components/dialogs/FolderDialog.svelte';
  import HelpDialog from './components/dialogs/HelpDialog.svelte';
  import ImportDialog from './components/dialogs/ImportDialog.svelte';
  import MarkAllDialog from './components/dialogs/MarkAllDialog.svelte';
  import PackagesDialog from './components/dialogs/PackagesDialog.svelte';
  import ProblemsDialog from './components/dialogs/ProblemsDialog.svelte';

  import { entries, loadMore, reload as reloadEntries, setRead, setStarred } from './lib/entries.svelte';
  import { find as findShortcut } from './lib/keys';
  import { nudgeSize } from './lib/prefs.svelte';
  import { refreshJob, resumeRefresh, startRefresh, stopPolling } from './lib/refresh.svelte';

  let search = $state('');

  // `undefined` means closed; `null` means "create a new folder".
  let folderDialog = $state<CategoryNode | null | undefined>(undefined);
  /** `undefined` means closed; a feed node opens `#rd-feed-dialog` for it. */
  let feedDialog = $state<FeedNode | undefined>(undefined);
  let addDialogOpen = $state(false);
  let problemsDialogOpen = $state(false);
  let helpDialogOpen = $state(false);
  let markAllDialogOpen = $state(false);
  let importDialogOpen = $state(false);
  let packagesDialogOpen = $state(false);

  let topBar: TopBar | undefined = $state();
  let entryList: EntryList | undefined = $state();
  let readingPane: ReadingPane | undefined = $state();

  /** The row the keyboard is on, which is not necessarily the one that is open. */
  let highlighted = $state<string | null>(null);

  /**
   * Reads the query string into a scope.
   *
   * The reference reader's sidebar links are `?feed=`, `?folder=`, `?view=starred` and `?label=`,
   * so mirroring them means a pasted link and the browser's back button both work.
   */
  function scopeFromLocation(): Scope {
    const params = new URLSearchParams(window.location.search);
    const search = params.get('search');
    if (search) return { kind: 'search', query: search };

    const feed = params.get('feed');
    if (feed) {
      const found = findFeed(feed);
      return { kind: 'feed', id: feed, name: found?.name ?? 'Feed' };
    }

    const folder = params.get('folder');
    if (folder) {
      const found = tree.data?.categories.find((c) => c.id === folder);
      return { kind: 'category', id: folder, name: found?.name ?? 'Folder' };
    }

    const label = params.get('label');
    if (label) {
      const found = tree.data?.counts.labels.find((l) => l.id === label);
      return { kind: 'label', id: label, name: found?.name ?? 'Label' };
    }

    if (params.get('view') === 'starred') return { kind: 'starred' };
    return { kind: 'main' };
  }

  function findFeed(id: string): FeedNode | undefined {
    if (!tree.data) return undefined;
    for (const category of tree.data.categories) {
      const hit = category.feeds.find((f) => f.id === id);
      if (hit) return hit;
    }
    return tree.data.orphans.find((f) => f.id === id);
  }

  function syncScopeFromUrl(): void {
    selectScope(scopeFromLocation());
  }

  /**
   * Moves the keyboard cursor by `delta` rows, opening the article it lands on.
   *
   * The list is the source of truth for order, so `j` and `k` follow whatever sort is on screen rather
   * than a separate notion of "next". At the end of the loaded pages the next one is fetched, which is
   * what makes reading by keyboard work past the first screenful.
   */
  function move(delta: number): void {
    const items = entries.items;
    if (items.length === 0) return;
    const current = items.findIndex((e) => e.id === (highlighted ?? view.selectedEntryId));
    let next = current < 0 ? (delta > 0 ? 0 : items.length - 1) : current + delta;

    if (next >= items.length) {
      if (!entries.hasMore) return;
      void loadMore().then(() => {
        if (next < entries.items.length) select(entries.items[next].id);
      });
      return;
    }
    if (next < 0) next = 0;
    select(items[next].id);
  }

  function select(id: string): void {
    highlighted = id;
    selectEntry(id);
    void entryList?.revealSelected();
  }

  function onKeydown(event: KeyboardEvent): void {
    const target = event.target as HTMLElement | null;
    const typing =
      target instanceof HTMLInputElement ||
      target instanceof HTMLTextAreaElement ||
      target instanceof HTMLSelectElement ||
      target?.isContentEditable === true;

    // Escape closes whatever is on top. An open <dialog> handles its own, so this covers the rest.
    if (event.key === 'Escape' && !typing) {
      if (isReading()) {
        event.preventDefault();
        selectEntry(null);
        highlighted = null;
      }
      return;
    }
    if (typing) return;

    const shortcut = findShortcut(event);
    if (!shortcut) return;

    const reading = isReading();
    if (reading && !shortcut.reading) return;
    if (!reading && !shortcut.list) return;
    event.preventDefault();

    const ids = (): string[] => {
      const id = view.selectedEntryId ?? highlighted;
      return id ? [id] : [];
    };

    switch (shortcut.key) {
      case 'j':
        if (reading) readingPane?.goNext();
        else move(1);
        break;
      case 'k':
        if (reading) readingPane?.goPrev();
        else move(-1);
        break;
      case 'Enter':
        if (highlighted) selectEntry(highlighted);
        break;
      case 's': {
        const list = ids();
        if (list.length === 0) break;
        const entry = entries.items.find((e) => e.id === list[0]);
        void setStarred(list, !(entry?.is_favorite ?? false));
        break;
      }
      case 'm': {
        const list = ids();
        if (list.length === 0) break;
        const entry = entries.items.find((e) => e.id === list[0]);
        void setRead(list, !(entry?.is_read ?? false));
        break;
      }
      case 'v':
        readingPane?.openOriginal();
        break;
      case 'u':
        toggleReadFilter();
        break;
      case 'a':
        // Confirmation first: the action marks every article the filters select read server-side,
        // not just the loaded pages, and the dialog is what shows the scope and the count.
        markAllDialogOpen = true;
        break;
      case 'r':
        onRefresh();
        break;
      case 'f':
        toggleFocus();
        break;
      case 'b':
        toggleNav();
        break;
      case '+':
        nudgeSize(1);
        break;
      case '-':
        nudgeSize(-1);
        break;
      case '/':
        topBar?.focusSearch();
        break;
      case '?':
        helpDialogOpen = true;
        break;
      default:
        break;
    }
  }

  onMount(() => {
    initPrefs();
    window.addEventListener('keydown', onKeydown);
    window.addEventListener('popstate', syncScopeFromUrl);

    void bootstrap().then(() => {
      // The tree has to exist before the URL can resolve feed and folder ids to names.
      syncScopeFromUrl();
      // A refresh job started before a page reload is still running server-side; picking it back
      // up here is what makes the spinner survive the reload instead of reporting idle.
      if (isSignedIn()) void resumeRefresh();
    });

    return () => {
      window.removeEventListener('keydown', onKeydown);
      window.removeEventListener('popstate', syncScopeFromUrl);
    };
  });

  // The refresh poller is a module-level timer; teardown must not leave it running.
  $effect(() => {
    return () => stopPolling();
  });

  async function reload(): Promise<void> {
    try {
      await refreshTree();
      syncScopeFromUrl();
    } catch (err) {
      notifyError(err instanceof Error ? err.message : t('app.couldNotReload'));
    }
  }

  /**
   * Picks up new feeds and articles after a dialog changed something, without touching the scope:
   * unlike `reload`, a reader with an article open keeps reading it.
   */
  async function refreshData(): Promise<void> {
    try {
      await refreshTree();
    } catch (err) {
      notifyError(err instanceof Error ? err.message : t('app.couldNotReload'));
    }
    await reloadEntries();
  }

  function onSearchSubmit(query: string): void {
    const scope: Scope = query ? { kind: 'search', query } : { kind: 'main' };
    selectScope(scope);
    const url = query ? `/?search=${encodeURIComponent(query)}` : '/';
    window.history.pushState({}, '', url);
  }

  function onRefresh(): void {
    // Enqueues the server-side job; the store polls it and guards against a second start.
    void startRefresh();
  }

  const title = $derived(scopeTitle(view.scope));

</script>

<svelte:head>
  <title>{isSignedIn() ? `${title} · freshrss-rs` : 'Sign in · freshrss-rs'}</title>
</svelte:head>

{#if session.loading}
  <div class="rd-auth">
    <div class="rd-auth-card">
      <div class="rd-auth-brand"><span>{t('common.loading')}</span></div>
    </div>
  </div>
{:else if !isSignedIn()}
  <Auth />
{:else}
  <div class="rd-app" class:is-reading={isReading()}>
    <TopBar
      bind:this={topBar}
      bind:search
      onSearch={onSearchSubmit}
      onRefresh={onRefresh}
      onShowShortcuts={() => (helpDialogOpen = true)}
      refreshing={refreshJob.running}
    />

    <NavTree
      onAddSubscription={() => (addDialogOpen = true)}
      onShowProblems={() => (problemsDialogOpen = true)}
      onManageFolder={(folder) => (folderDialog = folder)}
      onManageFeed={(feed) => (feedDialog = feed)}
      onShowPackages={() => (packagesDialogOpen = true)}
      onImportOpml={() => (importDialogOpen = true)}
    />

    <div class="rd-scrim" role="presentation" onclick={() => setNavOpen(false)}></div>

    <EntryList
      bind:this={entryList}
      selectedId={view.selectedEntryId}
      onOpen={(id) => {
        highlighted = id;
        selectEntry(id);
      }}
      onMarkAll={() => (markAllDialogOpen = true)}
    />

    {#if view.selectedEntryId}
      <ReadingPane
        bind:this={readingPane}
        entryId={view.selectedEntryId}
        onBack={() => {
          selectEntry(null);
          highlighted = null;
        }}
        onNavigate={select}
      />
    {/if}

    <FolderDialog folder={folderDialog} onclose={() => (folderDialog = undefined)} onChanged={reload} />
    <FeedDialog feed={feedDialog} onclose={() => (feedDialog = undefined)} onChanged={refreshData} />
    <AddDialog open={addDialogOpen} onclose={() => (addDialogOpen = false)} onChanged={refreshData} />
    <ProblemsDialog
      open={problemsDialogOpen}
      onclose={() => (problemsDialogOpen = false)}
      onChanged={refreshData}
    />
    <HelpDialog open={helpDialogOpen} onclose={() => (helpDialogOpen = false)} />
    <MarkAllDialog open={markAllDialogOpen} onclose={() => (markAllDialogOpen = false)} />
    <ImportDialog
      open={importDialogOpen}
      onclose={() => (importDialogOpen = false)}
      onChanged={refreshData}
    />
    <PackagesDialog
      open={packagesDialogOpen}
      onclose={() => (packagesDialogOpen = false)}
      onChanged={refreshData}
    />
  </div>
{/if}

<Toast />
