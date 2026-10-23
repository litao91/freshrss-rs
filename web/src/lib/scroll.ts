/**
 * Scrolling helpers.
 *
 * Two rules, both earned the hard way:
 *
 * - Smoothness is a preference, not a stylesheet default. `scroll-behavior: smooth` in CSS would
 *   apply to every scroll the page performs — including the raw `scrollTo` calls in
 *   `tools/browser-check.mjs`, whose waits were written for instant jumps — and the reduced-motion
 *   block in `app.css` cannot switch it back off, because it shortens transitions and animations,
 *   not scroll behaviour. So each call site passes `behavior` explicitly, through these helpers,
 *   which ask the OS first.
 * - Restarts are instant. Returning a replaced list (or a new article) to the top is a change of
 *   context; gliding through the old content on the way is motion with nothing to say.
 */

const REDUCED = '(prefers-reduced-motion: reduce)';

/** Queried per call rather than cached: cheap, and picks up a mid-session OS setting change. */
function behavior(): ScrollBehavior {
  return window.matchMedia(REDUCED).matches ? 'auto' : 'smooth';
}

/**
 * Scrolls a windowed list to bring one fixed-height row into view, moving as little as possible:
 * a row already on screen moves nothing at all, which is what keeps `j` and `k` from yanking the
 * list out from under the reader.
 *
 * The arithmetic is the same nearest-reveal `VirtualList` uses internally; what this adds is the
 * glide, which the library's instant `scroll_to_index` would drop.
 */
export function revealWindow(el: HTMLElement, top: number, size: number): void {
  const target = Math.min(top, Math.max(el.scrollTop, top + size - el.clientHeight));
  el.scrollTo({ top: target, behavior: behavior() });
}

/** Returns a container to the top instantly, for when its contents have been replaced. */
export function resetScroll(el: HTMLElement): void {
  el.scrollTo({ top: 0, behavior: 'auto' });
}
