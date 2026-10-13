/**
 * The paged article list.
 *
 * Holds the rows, the cursor, the snapshot fence and the in-flight request, and is the only place that
 * talks to `GET /entries`. The components render it; the shortcuts drive it.
 *
 * Two things here are easy to get wrong and are handled deliberately:
 *
 * **Races.** Changing folder while a page is loading would otherwise let the slower response land last
 * and show articles from the folder the reader left. Every fetch aborts the previous one, and a
 * response is discarded if the listing it belongs to is no longer current.
 *
 * **The snapshot fence.** `id_max` is captured from the first page and sent back on every page after
 * it, so an article ingested while the reader scrolls cannot become the newest row and shift every
 * following page by one. It is cleared whenever the listing restarts, which is how "show me what is
 * new" happens.
 */

import {
  api,
  type EntryListParams,
  type EntrySummary,
  type EntryPage,
} from './api';
import { notifyError } from './toast.svelte';
import { syncCounts } from './session.svelte';
import { scopeId, scopeParam, view } from './view.svelte';

/** Rows per page. The server caps this at 500; 40 is about two screens at the default density. */
export const PAGE_SIZE = 40;

export const entries = $state({
  items: [] as EntrySummary[],
  cursor: null as string | null,
  hasMore: false,
  /** The snapshot fence for the current listing, or null before the first page arrives. */
  idMax: null as string | null,
  unreadInScope: 0,
  loading: false,
  loadingMore: false,
  error: null as string | null,
});

let controller: AbortController | null = null;

/**
 * A token identifying the listing currently on screen.
 *
 * Compared rather than the parameters themselves: a reader who switches away and back to the same
 * folder is starting a new listing, and the old response must not land in it.
 */
let generation = 0;

/** The request parameters for the current view, minus the cursor and the fence. */
export function listParams(): EntryListParams {
  const params: EntryListParams = {
    scope: scopeParam(view.scope),
    limit: PAGE_SIZE,
    sort: view.sort,
    order: view.order,
  };
  const id = scopeId(view.scope);
  if (id !== undefined) params.id = id;
  if (view.readFilter === 'unread') params.read = 'unread';
  if (view.scope.kind === 'starred') params.starred = 'yes';
  if (view.scope.kind === 'search') params.search = view.scope.query;
  return params;
}

/** Starts a new listing from the top, discarding whatever was loaded. */
export async function reload(): Promise<void> {
  controller?.abort();
  controller = new AbortController();
  const mine = ++generation;

  entries.loading = true;
  entries.error = null;
  entries.cursor = null;
  entries.idMax = null;

  try {
    const page = await api.entries(listParams(), controller.signal);
    if (mine !== generation) return;
    apply(page, false);
  } catch (err) {
    if (mine !== generation || isAbort(err)) return;
    entries.error = message(err);
    entries.items = [];
    entries.hasMore = false;
  } finally {
    if (mine === generation) entries.loading = false;
  }
}

/** Appends the next page, if there is one and none is already in flight. */
export async function loadMore(): Promise<void> {
  if (!entries.hasMore || entries.loadingMore || entries.loading || !entries.cursor) return;
  const mine = generation;
  entries.loadingMore = true;
  try {
    const page = await api.entries({
      ...listParams(),
      cursor: entries.cursor,
      // The fence comes from the first page, not from now.
      id_max: entries.idMax ?? undefined,
    });
    if (mine !== generation) return;
    apply(page, true);
  } catch (err) {
    if (mine !== generation || isAbort(err)) return;
    notifyError(message(err));
  } finally {
    if (mine === generation) entries.loadingMore = false;
  }
}

function apply(page: EntryPage, append: boolean): void {
  if (append) {
    // A row can appear twice if the fence is not honoured; dropping the repeat costs nothing and keeps
    // the Svelte `{#each}` keys unique, which a duplicate would break.
    const have = new Set(entries.items.map((e) => e.id));
    entries.items = [...entries.items, ...page.items.filter((e) => !have.has(e.id))];
  } else {
    entries.items = page.items;
    entries.idMax = page.id_max;
    entries.unreadInScope = page.unread_in_scope;
  }
  entries.cursor = page.cursor;
  entries.hasMore = page.has_more;
}

/** True when the list has nothing to show and nothing is coming. */
export function isEmpty(): boolean {
  return !entries.loading && entries.items.length === 0 && entries.error === null;
}

// --- mutations --------------------------------------------------------------

/**
 * Marks articles read or unread.
 *
 * Optimistic: the row changes immediately and the request confirms afterwards. Reverting on failure is
 * the point of keeping the previous values rather than re-fetching, which would also lose the reader's
 * scroll position.
 */
export async function setRead(ids: string[], read: boolean): Promise<void> {
  const changed = patch(ids, (e) => (e.is_read = read));
  try {
    await (read ? api.markRead(ids) : api.markUnread(ids));
    if (read) entries.unreadInScope = Math.max(0, entries.unreadInScope - changed);
    void syncCounts();
  } catch (err) {
    patch(ids, (e) => (e.is_read = !read));
    notifyError(message(err));
  }
}

export async function setStarred(ids: string[], starred: boolean): Promise<void> {
  patch(ids, (e) => (e.is_favorite = starred));
  try {
    await (starred ? api.star(ids) : api.unstar(ids));
    void syncCounts();
  } catch (err) {
    patch(ids, (e) => (e.is_favorite = !starred));
    notifyError(message(err));
  }
}

/**
 * Marks everything the current filters select read — not everything loaded.
 *
 * The button says "all", and a reader who has scrolled nine pages has only loaded nine.
 */
export async function markAllRead(): Promise<number> {
  const params = listParams();
  // `mark-all-read` takes the same filters but ignores paging and sort.
  delete params.limit;
  delete params.sort;
  delete params.order;
  try {
    const { marked } = await api.markAllRead({
      ...params,
      id_max: entries.idMax ?? undefined,
    });
    for (const entry of entries.items) entry.is_read = true;
    entries.unreadInScope = 0;
    void syncCounts();
    // An unread-filtered list must also drop the rows that just became read.
    if (view.readFilter === 'unread') await reload();
    return marked;
  } catch (err) {
    notifyError(message(err));
    return 0;
  }
}

/** Applies `change` to every loaded row with one of these ids, and reports how many it touched. */
function patch(ids: string[], change: (entry: EntrySummary) => void): number {
  const wanted = new Set(ids);
  let touched = 0;
  for (const entry of entries.items) {
    if (wanted.has(entry.id)) {
      change(entry);
      touched += 1;
    }
  }
  return touched;
}

/** Replaces one row's labels, after a successful add or remove. */
export function setLabels(id: string, labels: { id: string; name: string }[]): void {
  const entry = entries.items.find((e) => e.id === id);
  if (entry) entry.labels = labels;
}

// --- helpers ----------------------------------------------------------------

function isAbort(err: unknown): boolean {
  return err instanceof DOMException && err.name === 'AbortError';
}

function message(err: unknown): string {
  return err instanceof Error ? err.message : 'Something went wrong';
}
