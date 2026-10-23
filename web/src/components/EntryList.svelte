<script lang="ts">
  /**
   * The list pane: `section#icerik.rd-list`.
   *
   * Markup follows the reference reader exactly — the class names, the `is-read`/`is-unread`/
   * `is-selected` states, the screen-reader `Unread:` prefix inside `.rd-item-meta` — because the CSS
   * is authored against those hooks and because a screen reader user of the original should find the
   * same announcements here.
   */
  import { tick } from 'svelte';
  import { ActionMenu, VirtualList, type CmdAction } from 'svelte-widgets';
  import { format_shortcut } from 'svelte-widgets/utils';
  import { mediaUrl, type EntrySummary } from '../lib/api';
  import {
    entries,
    isEmpty,
    loadMore,
    markReadFrom,
    reload,
    setRead,
    setStarred,
  } from '../lib/entries.svelte';
  import { formatCount, formatListTime } from '../lib/format';
  import { t } from '../lib/i18n';
  import { prefs } from '../lib/prefs.svelte';
  import { resetScroll, revealWindow } from '../lib/scroll';
  import {
    SORT_LABELS,
    scopeTitle,
    setSort,
    toggleOrder,
    toggleReadFilter,
    view,
    type SortKey,
  } from '../lib/view.svelte';

  let {
    selectedId,
    anchorId,
    onOpen,
    onMarkAll,
  }: {
    selectedId: string | null;
    /** The row the "mark above/below" actions work from; null while the keyboard is nowhere. */
    anchorId: string | null;
    onOpen: (id: string) => void;
    /** Asks App to open `#rd-markall-dialog`: marking everything read needs a confirmation. */
    onMarkAll: () => void;
  } = $props();

  let scrollEl: HTMLDivElement | undefined = $state();

  const title = $derived(scopeTitle(view.scope));
  const unreadLabel = $derived(formatCount(entries.unreadInScope));

  /** Which sorts the segment control offers. The rest are reachable from a saved query. */
  const sorts: SortKey[] = ['id', 'date', 'title', 'feed'];

  /** The sort button's label; `SORT_LABELS` is the English fallback for an untranslated key. */
  function sortLabel(sort: SortKey): string {
    return t(`sort.${sort}`, SORT_LABELS[sort]);
  }

  /**
   * One row's height in pixels, which `VirtualList` needs as a number.
   *
   * Read back off a probe rather than restated here: the CSS owns what a row is — the padding, the
   * three fixed line boxes and the hairline between rows, all of it in terms of the density tokens
   * — and a second copy in TypeScript would be a second thing to keep in step. The probe resolves
   * the `calc()` the way only the engine can, because custom properties substitute rather than
   * compute.
   */
  let itemSize = $state(72);

  $effect(() => {
    void prefs.density;
    const probe = document.createElement('div');
    probe.style.height = 'var(--rd-item-h)';
    probe.style.position = 'absolute';
    probe.style.visibility = 'hidden';
    document.body.append(probe);
    const measured = probe.getBoundingClientRect().height;
    probe.remove();
    if (measured > 0) itemSize = measured;
  });

  /**
   * Loads the next page as the windowed rows approach the end of what is loaded.
   *
   * The sentinel and its IntersectionObserver went with the `<ul>`: inside a virtual list there is
   * no element at the end of the data to observe, only a spacer as tall as everything. A scroll
   * handler on the list's own container is the same 400px of lookahead the observer had, and
   * `loadMore` still guards against queueing pages faster than they land.
   */
  function onListScroll(event: Event): void {
    const el = event.currentTarget as HTMLElement;
    if (el.scrollHeight - el.scrollTop - el.clientHeight < 400) void loadMore();
  }

  // Restart the listing whenever the slice of the corpus or the way it is ordered changes.
  $effect(() => {
    // Reading each field inside the effect is what subscribes to it.
    void view.scope;
    void view.readFilter;
    void view.sort;
    void view.order;
    // A restarted listing starts at the top: keeping the old offset drops the reader into the
    // middle of a feed they just chose, at a row nobody selected. Keyed to the same four fields as
    // the restart — and deliberately not to `entries.items`, or an appended page would yank them
    // back up mid-read.
    if (scrollEl) resetScroll(scrollEl);
    void reload();
  });

  function open(entry: EntrySummary): void {
    onOpen(entry.id);
    // Opening an article is the moment a reader stops caring about it being unread. Doing it here
    // rather than in the reading pane means the list updates even if the body fetch fails.
    if (!entry.is_read) void setRead([entry.id], true);
  }

  async function toggleStar(entry: EntrySummary, event: MouseEvent): Promise<void> {
    // The star button sits inside the row's own button, so the click would otherwise open the article.
    event.stopPropagation();
    await setStarred([entry.id], !entry.is_favorite);
  }

  async function toggleRead(entry: EntrySummary): Promise<void> {
    await setRead([entry.id], !entry.is_read);
  }

  /** One entry in a mark-above/below menu. */
  interface MarkAction extends CmdAction {
    /** A Remixicon glyph. svelte-widgets ships its own icon set; this reader keeps Feeddi's. */
    icon: string;
  }

  /**
   * The two anchored actions, for one known row.
   *
   * Both menus are built from this, so the toolbar and a right-click cannot offer different things.
   * `shortcut` is rendered as `<kbd>` hints by `ActionMenu` and names the binding `keys.ts` holds —
   * a third copy of the pairing would be a third thing to keep in step.
   */
  function markActions(anchor: string): MarkAction[] {
    return [
      {
        id: 'above',
        label: t('list.markAbove'),
        icon: 'ri-arrow-up-double-line',
        shortcut: 'Shift+K',
        action: () => void markReadFrom(anchor, 'above'),
      },
      {
        id: 'below',
        label: t('list.markBelow'),
        icon: 'ri-arrow-down-double-line',
        shortcut: 'Shift+J',
        action: () => void markReadFrom(anchor, 'below'),
      },
    ];
  }

  /**
   * An empty list is how the toolbar menu says "there is nothing to anchor on": `ActionMenu` refuses
   * to open when it has no actions and sets `aria-disabled` on the trigger, which `.rd-btn` already
   * renders as dimmed and unclickable.
   */
  const toolbarActions = $derived(anchorId === null ? [] : markActions(anchorId));

  /** Where the last right-click landed. Setting it back to null is what closes the row menu. */
  let rowMenu = $state<{ x: number; y: number } | null>(null);
  /** The row it landed on, which is what the menu anchors to rather than the keyboard cursor. */
  let rowMenuAnchor = $state<string | null>(null);
  const rowActions = $derived(rowMenuAnchor === null ? [] : markActions(rowMenuAnchor));

  /**
   * Works out which row a right-click belongs to.
   *
   * Found by walking up from the target rather than by a handler on each row, because a right-click
   * on the list's own padding belongs to no article and must not quietly re-anchor the menu on
   * whatever row was clicked last. Clearing the anchor is what makes `ActionMenu` fall through to
   * the browser's own menu there.
   */
  function onListContextMenu(event: MouseEvent): void {
    const row = (event.target as Element | null)?.closest<HTMLElement>('.rd-item');
    // A row's own id is `rd-item-<entry id>`, which `revealSelected` already addresses it by.
    rowMenuAnchor = row ? row.id.slice('rd-item-'.length) : null;
  }

  /** Scrolls the selected row into view after `j`/`k`, without yanking the list when it is already visible. */
  export async function revealSelected(): Promise<void> {
    await tick();
    if (!selectedId || !scrollEl) return;
    const index = entries.items.findIndex((e) => e.id === selectedId);
    if (index < 0) return;
    // By index rather than by element: under windowing the row may not be rendered at all, and its
    // position is arithmetic either way.
    revealWindow(scrollEl, index * itemSize, itemSize);
  }
</script>

<section id="icerik" class="rd-list">
  {#snippet markItem({ action }: { action: MarkAction })}
    <span class="rd-mark-item">
      <i class={action.icon} aria-hidden="true"></i>
      <span class="rd-mark-item-label">{action.label}</span>
      {#if action.shortcut}
        <span class="rd-kbd" aria-hidden="true">
          {#each format_shortcut(action.shortcut) as part, index (index)}
            <kbd>{part}</kbd>
          {/each}
        </span>
      {/if}
    </span>
  {/snippet}

  <div class="rd-list-head">
    <div class="rd-list-title-row">
      <h1 id="rd-list-title" class="rd-list-title">{title}</h1>
    </div>

    <div class="rd-list-tools">
      <div class="rd-segment" aria-label={t('list.showAria')} role="group">
        <button
          type="button"
          class:is-active={view.readFilter === 'unread'}
          aria-pressed={view.readFilter === 'unread'}
          onclick={toggleReadFilter}
        >
          {t('list.unread')}
          <span class="rd-segment-count">{unreadLabel}</span>
        </button>
        <button
          type="button"
          class:is-active={view.readFilter === 'all'}
          aria-pressed={view.readFilter === 'all'}
          onclick={toggleReadFilter}
        >
          {t('list.all')}
        </button>
      </div>

      <div class="rd-segment" aria-label={t('list.sortAria')} role="group">
        {#each sorts as sort (sort)}
          <button
            type="button"
            class:is-active={view.sort === sort}
            aria-pressed={view.sort === sort}
            title="{sortLabel(sort)} {view.order === 'desc' ? t('list.first') : t('list.last')}"
            onclick={() => setSort(sort)}
          >
            {sortLabel(sort)}
          </button>
        {/each}
      </div>

      <div class="rd-segment" aria-label={t('list.orderAria')} role="group">
        <button
          type="button"
          class:is-active={view.order === 'desc'}
          title={t('list.newestFirst')}
          onclick={() => view.order !== 'desc' && toggleOrder()}
        >
          <i class="ri-sort-desc" aria-hidden="true"></i>
          <span class="rd-sr">{t('list.newestFirst')}</span>
        </button>
        <button
          type="button"
          class:is-active={view.order === 'asc'}
          title={t('list.oldestFirst')}
          onclick={() => view.order !== 'asc' && toggleOrder()}
        >
          <i class="ri-sort-asc" aria-hidden="true"></i>
          <span class="rd-sr">{t('list.oldestFirst')}</span>
        </button>
      </div>

      <button class="rd-btn rd-btn--quiet" type="button" onclick={onMarkAll}>
        <i class="ri-check-double-line" aria-hidden="true"></i>
        {t('list.markAll')}
      </button>

      <!-- Sits beside "mark all" rather than replacing it: that one is the whole scope, these two
           are one side of the row the reader is on. `ActionMenu` renders its trigger inside a
           `display: contents` wrapper, so this button joins the flex row without the wrapper
           breaking `.rd-list-tools > .rd-btn`, which is what pushes the pair to the right.

           `dismiss_on: 'press'` is load-bearing, not a preference. It moves the menu off the
           browser's own light-dismiss and onto the library's, which claims Escape and stops it
           propagating — without that, App's global Escape handler both cancels the popover's
           default dismissal and closes the article underneath the menu. It also dismisses on
           `pointerdown`, so a right-click away closes it the way an OS menu would. -->
      <ActionMenu
        actions={toolbarActions}
        placement="bottom"
        align="end"
        dismiss={{ dismiss_on: 'press' }}
        item={markItem}
      >
        {#snippet trigger(props)}
          <button
            class="rd-btn rd-btn--quiet rd-mark-menu"
            type="button"
            title={anchorId === null ? t('list.needAnchor') : t('list.markMenu')}
            {...props}
          >
            <i class="ri-more-2-line" aria-hidden="true"></i>
            <span class="rd-sr">{t('list.markMenu')}</span>
          </button>
        {/snippet}
      </ActionMenu>
    </div>
  </div>

  <!-- The region a right-click applies to. `ActionMenu` wraps this in a `display: contents` div
       of its own, so the list's layout is untouched, and it owns the popover, the deferral that
       stops Chromium light-dismissing it within the same gesture, and the arrow-key and focus
       handling. Empty `rowActions` — a right-click that landed on no row — makes it fall
       through to the browser's own menu. -->
  <ActionMenu
    trigger="region"
    bind:at={rowMenu}
    actions={rowActions}
    dismiss={{ dismiss_on: 'press' }}
    item={markItem}
  >
    {#if entries.error}
      <div class="rd-list-empty">
        <i class="ri-error-warning-line" aria-hidden="true"></i>
        <p>{entries.error}</p>
        <button class="rd-btn" type="button" onclick={() => void reload()}>{t('common.tryAgain')}</button>
      </div>
    {:else if entries.loading && entries.items.length === 0}
      <div class="rd-list-empty">
        <i class="ri-loader-4-line rd-spin" aria-hidden="true"></i>
        <p>{t('common.loading')}</p>
      </div>
    {:else if isEmpty()}
      <div class="rd-list-empty">
        <i class="ri-inbox-line" aria-hidden="true"></i>
        {#if view.readFilter === 'unread'}
          <p>{t('list.nothingUnread')}</p>
          <p class="rd-hint">{t('list.nothingUnreadHint')}</p>
        {:else}
          <p>{t('list.noMatches')}</p>
        {/if}
      </div>
    {:else}
      <!-- `VirtualList` renders only the rows beside the scroll position, inside a spacer as tall
           as the whole listing, so a few thousand articles cost a few dozen elements. Every row is
           exactly `itemSize` tall — see the probe above and `--rd-item-h` in app.css — because
           that is the one thing windowing has to know to place its window. -->
      <VirtualList
        id="rd-list-scroll"
        class="rd-list-body rd-items"
        bind:element={scrollEl}
        items={entries.items}
        item_size={itemSize}
        overscan={6}
        key={(entry) => entry.id}
        onscroll={onListScroll}
        oncontextmenu={onListContextMenu}
      >
        {#snippet children(entry)}
          <div
            id="rd-item-{entry.id}"
            class="rd-item"
            class:is-read={entry.is_read}
            class:is-unread={!entry.is_read}
            class:is-selected={entry.id === selectedId}
          >
            <button class="rd-item-btn" type="button" onclick={() => open(entry)}>
              <span class="rd-item-meta">
                {#if !entry.is_read}
                  <span class="rd-dot" aria-hidden="true"></span>
                  <span class="rd-sr">{t('list.unreadSr')}</span>
                {/if}
                <span class="rd-item-feed">
                  {#if entry.thumbnail}
                    <img
                      class="rd-item-media"
                      src={mediaUrl(entry.thumbnail, entry.link)}
                      alt=""
                      loading="lazy"
                    />
                  {:else if entry.has_media}
                    <i class="ri-play-circle-line rd-item-media" aria-hidden="true"></i>
                  {/if}
                  {entry.feed_name}
                  {#if entry.is_updated}
                    <i class="ri-pencil-line" title={t('list.updatedUpstream')} aria-hidden="true"></i>
                  {/if}
                </span>
                <time datetime={new Date(entry.date * 1000).toISOString()} title={new Date(entry.date * 1000).toLocaleString()}>
                  {formatListTime(entry.date)}
                </time>
              </span>
              <span class="rd-item-title">{entry.title}</span>
              <!-- Always present, empty or not: a row whose summary line collapsed away would be
                   a different height from its neighbours, and a windowed list cannot place a
                   window of rows it cannot measure. -->
              <span class="rd-item-summary">{entry.summary ?? ''}</span>
            </button>

            <button
              class="rd-star"
              class:is-starred={entry.is_favorite}
              type="button"
              title={entry.is_favorite ? t('list.unstarTitle') : t('list.starTitle')}
              aria-pressed={entry.is_favorite}
              onclick={(event) => void toggleStar(entry, event)}
            >
              <i class={entry.is_favorite ? 'ri-star-fill' : 'ri-star-line'} aria-hidden="true"></i>
              <span class="rd-sr">{t('list.starSr')}</span>
            </button>

            <button
              class="rd-star rd-read-toggle"
              type="button"
              title={entry.is_read ? t('list.markUnreadTitle') : t('list.markReadTitle')}
              onclick={() => void toggleRead(entry)}
            >
              <i class={entry.is_read ? 'ri-mail-line' : 'ri-mail-unread-line'} aria-hidden="true"></i>
              <span class="rd-sr">{entry.is_read ? t('list.markUnreadSr') : t('list.markReadSr')}</span>
            </button>
          </div>
        {/snippet}
      </VirtualList>
    {/if}
  </ActionMenu>

  {#if entries.items.length > 0 || entries.loadingMore}
    <div class="rd-list-end">
      {#if entries.loadingMore}
        <i class="ri-loader-4-line rd-spin" aria-hidden="true"></i>
        {t('list.loadingMore')}
      {:else if entries.hasMore}
        <button class="rd-btn rd-btn--quiet" type="button" onclick={() => void loadMore()}>
          {t('list.loadMore')}
        </button>
      {:else}
        <span class="rd-hint">{t('list.endOfList')}</span>
      {/if}
    </div>
  {/if}
</section>
