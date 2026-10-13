/**
 * What the reader is currently looking at: which slice of the corpus, which filter, and which
 * article is open.
 *
 * This is the single source of truth that the sidebar, the list head and the reading pane all
 * read from. Keeping it in one place is what makes the keyboard shortcuts and the URL hash able to
 * drive the same state the mouse does.
 */

import { t, tf } from './i18n';

/** A slice of the corpus, mirroring the server's listing scopes. */
export type Scope =
  /** "All articles" — feeds at main-stream priority or above. */
  | { kind: 'main' }
  /** "Starred". */
  | { kind: 'starred' }
  | { kind: 'category'; id: string; name: string }
  | { kind: 'feed'; id: string; name: string }
  | { kind: 'label'; id: string; name: string }
  /** A BooleanSearch query from `#rd-search`. */
  | { kind: 'search'; query: string };

/** Unread-only versus everything, the `Unread | All` segment. */
export type ReadFilter = 'unread' | 'all';

/** Sort order for the list, matching the server's `order` parameter. */
export type Order = 'desc' | 'asc';

/**
 * What the list is ordered by.
 *
 * Only the sorts a reader would pick from a segment control. The server supports more (`length`,
 * `modified`, `rand`), and a saved query can name any of them, but the default listing is by ingestion
 * order and the alternative people actually want is publication date.
 */
export type SortKey = 'id' | 'date' | 'title' | 'feed';

/** The label each sort shows in the segment control. */
export const SORT_LABELS: Record<SortKey, string> = {
  id: 'Newest',
  date: 'Published',
  title: 'Title',
  feed: 'Feed',
};

export const view = $state({
  scope: { kind: 'main' } as Scope,
  readFilter: 'unread' as ReadFilter,
  sort: 'id' as SortKey,
  order: 'desc' as Order,
  /** The article open in the reading pane; `null` means the list is full width. */
  selectedEntryId: null as string | null,
});

/**
 * True while an article is open, which is what adds `.is-reading` to `.rd-app`.
 *
 * A function rather than an exported `$derived`: Svelte forbids exporting derived state from a
 * module (`derived_invalid_export`), and a plain function is still reactive because calling it
 * inside a template reads `view.selectedEntryId` within the rendering effect's tracking scope.
 */
export function isReading(): boolean {
  return view.selectedEntryId !== null;
}

/**
 * The heading for `#rd-list-title`.
 *
 * Named scopes (folder, feed, label) show the user's own name for them, which is data and never
 * translated; the fixed scopes go through `t()`, which also makes the heading re-render when the
 * language changes.
 */
export function scopeTitle(scope: Scope): string {
  switch (scope.kind) {
    case 'main':
      return t('scope.allArticles');
    case 'starred':
      return t('scope.starred');
    case 'category':
    case 'feed':
    case 'label':
      return scope.name;
    case 'search':
      return tf('scope.searchResults', { q: scope.query });
  }
}

/** The server's `scope` query parameter for the current slice. */
export function scopeParam(scope: Scope): string {
  switch (scope.kind) {
    case 'main':
      return 'main';
    case 'starred':
      return 'starred';
    case 'category':
      return 'category';
    case 'feed':
      return 'feed';
    case 'label':
      return 'tag';
    case 'search':
      // A bare search runs against the main stream, matching what the search box implies.
      return 'main';
  }
}

/** The `id` query parameter, when the scope needs one. */
export function scopeId(scope: Scope): string | undefined {
  return 'id' in scope ? scope.id : undefined;
}

export function selectScope(scope: Scope): void {
  view.scope = scope;
  // Opening a different slice invalidates the open article: leaving it selected would show an
  // article that is not in the list the user is looking at.
  view.selectedEntryId = null;
}

export function selectEntry(id: string | null): void {
  view.selectedEntryId = id;
}

export function toggleReadFilter(): void {
  view.readFilter = view.readFilter === 'unread' ? 'all' : 'unread';
  view.selectedEntryId = null;
}

export function toggleOrder(): void {
  view.order = view.order === 'desc' ? 'asc' : 'desc';
  view.selectedEntryId = null;
}

/**
 * Sets the sort, resetting the direction to the one that makes sense for it.
 *
 * Sorting by title ascending is what a person means by "by title"; the same is not true of the date
 * sorts, where newest first is the default and oldest first is the deliberate choice.
 */
export function setSort(sort: SortKey): void {
  if (view.sort === sort) {
    toggleOrder();
    return;
  }
  view.sort = sort;
  view.order = sort === 'title' || sort === 'feed' ? 'asc' : 'desc';
  view.selectedEntryId = null;
}

/**
 * Which sidebar row is active.
 *
 * Compared structurally rather than by object identity, because `selectScope` is called with a
 * freshly built object every time.
 */
export function isActive(current: Scope, candidate: Scope): boolean {
  if (current.kind !== candidate.kind) return false;
  if ('id' in current && 'id' in candidate) return current.id === candidate.id;
  if (current.kind === 'search' && candidate.kind === 'search') {
    return current.query === candidate.query;
  }
  return true;
}
