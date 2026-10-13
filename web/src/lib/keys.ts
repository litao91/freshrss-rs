/**
 * The keyboard map.
 *
 * Declared as data rather than as a switch inside a handler, for two reasons: the shortcuts dialog has
 * to list them and would otherwise be a second copy that drifts, and a table makes the *absence* of a
 * binding visible in review.
 *
 * The set is the reference reader's. Adding a binding means adding a row here; the dialog picks it up.
 */

/** One binding, as the dialog shows it and as the handler matches it. */
export interface Shortcut {
  /** The `KeyboardEvent.key`, matched exactly. `?` is `Shift+/` and the browser reports it as `?`. */
  key: string;
  /** What it does, in the imperative, for the dialog. */
  description: string;
  /** True when it only applies with an article open. */
  reading?: boolean;
  /** True when it applies to the list. */
  list?: boolean;
  /** Set when the binding needs a modifier, so the handler can require exactly that one. */
  shift?: boolean;
}

export const SHORTCUTS: Shortcut[] = [
  { key: 'j', description: 'Next article', list: true, reading: true },
  { key: 'k', description: 'Previous article', list: true, reading: true },
  { key: 'Enter', description: 'Open the highlighted article', list: true },
  { key: 's', description: 'Star or unstar', list: true, reading: true },
  { key: 'm', description: 'Read or unread', list: true, reading: true },
  { key: 'v', description: 'Open the original in a new tab', reading: true },
  { key: 'u', description: 'Unread or all', list: true },
  { key: 'a', description: 'Mark everything in this view read', shift: true, list: true },
  { key: 'r', description: 'Refresh the feeds', list: true, reading: true },
  { key: 'f', description: 'Focus mode', list: true, reading: true },
  { key: 'b', description: 'Show or hide the sidebar', list: true, reading: true },
  { key: '+', description: 'Larger text', list: true, reading: true },
  { key: '-', description: 'Smaller text', list: true, reading: true },
  { key: '/', description: 'Search', list: true },
  { key: 'Escape', description: 'Back to the list', reading: true },
  { key: '?', description: 'This list', shift: true, list: true, reading: true },
];

/** Looks up a binding, or `undefined` when the key is not ours. */
export function find(event: KeyboardEvent): Shortcut | undefined {
  // A modifier other than the one the binding asks for means the browser or the OS owns the key:
  // Cmd+R is reload, Ctrl+F is find, and swallowing either would be hostile.
  const wantsShift = event.key === '?';
  if (event.metaKey || event.ctrlKey || event.altKey) return undefined;
  if (event.shiftKey !== wantsShift && event.key !== 'A') return undefined;
  return SHORTCUTS.find((s) =>
    s.shift ? (event.shiftKey && (event.key === s.key || event.key.toUpperCase() === s.key)) : event.key === s.key,
  );
}

/** Groups the bindings for the dialog, which shows list and reading shortcuts in two columns. */
export function grouped(): { list: Shortcut[]; reading: Shortcut[] } {
  return {
    list: SHORTCUTS.filter((s) => s.list),
    reading: SHORTCUTS.filter((s) => s.reading),
  };
}
