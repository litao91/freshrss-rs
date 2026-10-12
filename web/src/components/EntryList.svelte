<script lang="ts">
  /**
   * The list pane: `section#icerik.rd-list`.
   *
   * Markup follows the reference reader exactly — the class names, the `is-read`/`is-unread`/
   * `is-selected` states, the screen-reader `Unread:` prefix inside `.rd-item-meta` — because the CSS
   * is authored against those hooks and because a screen reader user of the original should find the
   * same announcements here.
   */
  import { onMount, tick } from 'svelte';
  import type { EntrySummary } from '../lib/api';
  import {
    entries,
    isEmpty,
    loadMore,
    markAllRead,
    reload,
    setRead,
    setStarred,
  } from '../lib/entries.svelte';
  import { formatCount, formatListTime } from '../lib/format';
  import { notifyError } from '../lib/toast.svelte';
  import { refreshTree } from '../lib/session.svelte';
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
    onOpen,
  }: {
    selectedId: string | null;
    onOpen: (id: string) => void;
  } = $props();

  let scrollEl: HTMLElement | undefined = $state();
  let sentinel: HTMLElement | undefined = $state();
  /**
   * Deliberately not `$state`.
   *
   * The effect below reads it (to disconnect the previous one) and writes it, and a reactive variable
   * that an effect both reads and writes re-runs itself until Svelte gives up with
   * `effect_update_depth_exceeded`. Nothing renders it, so it does not need to be tracked.
   */
  let observer: IntersectionObserver | undefined;

  const title = $derived(scopeTitle(view.scope));
  const unreadLabel = $derived(formatCount(entries.unreadInScope));

  /** Which sorts the segment control offers. The rest are reachable from a saved query. */
  const sorts: SortKey[] = ['id', 'date', 'title', 'feed'];

  /**
   * Loads the next page when the sentinel scrolls into view.
   *
   * An observer rather than a scroll handler: the handler would fire on every pixel of movement and
   * re-check a condition that only changes near the end, and it would need the container's scroll
   * geometry, which the observer already knows.
   */
  function observe(): void {
    observer?.disconnect();
    if (!sentinel || !scrollEl) return;
    observer = new IntersectionObserver(
      (hits) => {
        // `rootMargin` would start the fetch earlier, but then a fast scroll could queue several pages
        // before the first lands; `loadMore` guards against that, and one page of lookahead is enough.
        if (hits.some((h) => h.isIntersecting)) void loadMore();
      },
      { root: scrollEl, rootMargin: '400px 0px', threshold: 0 },
    );
    observer.observe(sentinel);
  }

  // Restart the listing whenever the slice of the corpus or the way it is ordered changes.
  $effect(() => {
    // Reading each field inside the effect is what subscribes to it.
    void view.scope;
    void view.readFilter;
    void view.sort;
    void view.order;
    void reload();
  });

  $effect(() => {
    void entries.items.length;
    void sentinel;
    void scrollEl;
    observe();
  });

  onMount(() => {
    observe();
    return () => observer?.disconnect();
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

  async function onMarkAll(): Promise<void> {
    const marked = await markAllRead();
    if (marked > 0) {
      // The sidebar's badges come from a separate aggregate; without this they keep the number the
      // reader just invalidated.
      try {
        await refreshTree();
      } catch (err) {
        notifyError(err instanceof Error ? err.message : 'Could not refresh the counts');
      }
    }
  }

  /** Scrolls the selected row into view after `j`/`k`, without yanking the list when it is already visible. */
  export async function revealSelected(): Promise<void> {
    await tick();
    if (!selectedId || !scrollEl) return;
    const row = scrollEl.querySelector<HTMLElement>(`#rd-item-${CSS.escape(selectedId)}`);
    row?.scrollIntoView({ block: 'nearest' });
  }
</script>

<section id="icerik" class="rd-list">
  <div class="rd-list-head">
    <div class="rd-list-title-row">
      <h1 id="rd-list-title" class="rd-list-title">{title}</h1>
    </div>

    <div class="rd-list-tools">
      <div class="rd-segment" aria-label="Show" role="group">
        <button
          type="button"
          class:is-active={view.readFilter === 'unread'}
          aria-pressed={view.readFilter === 'unread'}
          onclick={toggleReadFilter}
        >
          Unread
          <span class="rd-segment-count">{unreadLabel}</span>
        </button>
        <button
          type="button"
          class:is-active={view.readFilter === 'all'}
          aria-pressed={view.readFilter === 'all'}
          onclick={toggleReadFilter}
        >
          All
        </button>
      </div>

      <div class="rd-segment" aria-label="Sort" role="group">
        {#each sorts as sort (sort)}
          <button
            type="button"
            class:is-active={view.sort === sort}
            aria-pressed={view.sort === sort}
            title="{SORT_LABELS[sort]} {view.order === 'desc' ? 'first' : 'last'}"
            onclick={() => setSort(sort)}
          >
            {SORT_LABELS[sort]}
          </button>
        {/each}
      </div>

      <div class="rd-segment" aria-label="Sort order" role="group">
        <button
          type="button"
          class:is-active={view.order === 'desc'}
          title="Newest first"
          onclick={() => view.order !== 'desc' && toggleOrder()}
        >
          <i class="ri-sort-desc" aria-hidden="true"></i>
          <span class="rd-sr">Newest first</span>
        </button>
        <button
          type="button"
          class:is-active={view.order === 'asc'}
          title="Oldest first"
          onclick={() => view.order !== 'asc' && toggleOrder()}
        >
          <i class="ri-sort-asc" aria-hidden="true"></i>
          <span class="rd-sr">Oldest first</span>
        </button>
      </div>

      <button class="rd-btn rd-btn--quiet" type="button" onclick={onMarkAll}>
        <i class="ri-check-double-line" aria-hidden="true"></i>
        Mark all as read
      </button>
    </div>
  </div>

  <div id="rd-list-scroll" class="rd-list-body" bind:this={scrollEl}>
    {#if entries.error}
      <div class="rd-list-empty">
        <i class="ri-error-warning-line" aria-hidden="true"></i>
        <p>{entries.error}</p>
        <button class="rd-btn" type="button" onclick={() => void reload()}>Try again</button>
      </div>
    {:else if entries.loading && entries.items.length === 0}
      <div class="rd-list-empty">
        <i class="ri-loader-4-line rd-spin" aria-hidden="true"></i>
        <p>Loading…</p>
      </div>
    {:else if isEmpty()}
      <div class="rd-list-empty">
        <i class="ri-inbox-line" aria-hidden="true"></i>
        {#if view.readFilter === 'unread'}
          <p>Nothing unread here.</p>
          <p class="rd-hint">Switch to <button type="button" class="rd-linkish" onclick={toggleReadFilter}>All</button> to see everything.</p>
        {:else}
          <p>No articles match.</p>
        {/if}
      </div>
    {:else}
      <ul class="rd-items">
        {#each entries.items as entry (entry.id)}
          <li
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
                  <span class="rd-sr">Unread:</span>
                {/if}
                <span class="rd-item-feed">
                  {#if entry.thumbnail}
                    <img class="rd-item-media" src={entry.thumbnail} alt="" loading="lazy" />
                  {:else if entry.has_media}
                    <i class="ri-play-circle-line rd-item-media" aria-hidden="true"></i>
                  {/if}
                  {entry.feed_name}
                  {#if entry.is_updated}
                    <i class="ri-pencil-line" title="Updated upstream" aria-hidden="true"></i>
                  {/if}
                </span>
                <time datetime={new Date(entry.date * 1000).toISOString()} title={new Date(entry.date * 1000).toLocaleString()}>
                  {formatListTime(entry.date)}
                </time>
              </span>
              <span class="rd-item-title">{entry.title}</span>
              {#if entry.summary}
                <span class="rd-item-summary">{entry.summary}</span>
              {/if}
            </button>

            <button
              class="rd-star"
              class:is-starred={entry.is_favorite}
              type="button"
              title={entry.is_favorite ? 'Unstar (s)' : 'Star (s)'}
              aria-pressed={entry.is_favorite}
              onclick={(event) => void toggleStar(entry, event)}
            >
              <i class={entry.is_favorite ? 'ri-star-fill' : 'ri-star-line'} aria-hidden="true"></i>
              <span class="rd-sr">Star</span>
            </button>

            <button
              class="rd-star rd-read-toggle"
              type="button"
              title={entry.is_read ? 'Mark as unread (m)' : 'Mark as read (m)'}
              onclick={() => void toggleRead(entry)}
            >
              <i class={entry.is_read ? 'ri-mail-line' : 'ri-mail-unread-line'} aria-hidden="true"></i>
              <span class="rd-sr">{entry.is_read ? 'Mark as unread' : 'Mark as read'}</span>
            </button>
          </li>
        {/each}
      </ul>
    {/if}

    <div id="rd-sentinel" class="rd-list-end" bind:this={sentinel}>
      {#if entries.loadingMore}
        <i class="ri-loader-4-line rd-spin" aria-hidden="true"></i>
        Loading more…
      {:else if entries.hasMore}
        <button class="rd-btn rd-btn--quiet" type="button" onclick={() => void loadMore()}>
          Load more
        </button>
      {:else if entries.items.length > 0}
        <span class="rd-hint">End of list</span>
      {/if}
    </div>
  </div>
</section>
