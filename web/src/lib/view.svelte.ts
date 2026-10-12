/**
 * What the reader is currently looking at: which slice of the corpus, which filter, and which
 * article is open.
 *
 * This is the single source of truth that the sidebar, the list head and the reading pane all
 * read from. Keeping it in one place is what makes the keyboard shortcuts and the URL hash able to
 * drive the same state the mouse does.
 */

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

export const view = $state({
  scope: { kind: 'main' } as Scope,
  readFilter: 'unread' as ReadFilter,
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

/** The heading for `#rd-list-title`. */
export function scopeTitle(scope: Scope): string {
  switch (scope.kind) {
    case 'main':
      return 'All articles';
    case 'starred':
      return 'Starred';
    case 'category':
    case 'feed':
    case 'label':
      return scope.name;
    case 'search':
      return `Search: ${scope.query}`;
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
