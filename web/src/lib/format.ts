/**
 * Display formatting that matches the reference reader.
 *
 * The list pane shows a compact timestamp ("8:18 AM" for today, "Oct 3" otherwise) and the reading
 * pane shows the full form ("Monday, October 12, 2026 8:18 AM"). Counts over 999 render as "999+",
 * which is what the sidebar badge and the Unread segment show.
 */

/** `999+` above 999, otherwise the plain number. Zero renders as nothing in the sidebar. */
export function formatCount(n: number): string {
  if (n > 999) return '999+';
  return String(n);
}

/** The sidebar hides a zero badge entirely rather than showing "0" on every quiet feed. */
export function formatCountOrEmpty(n: number): string {
  return n > 0 ? formatCount(n) : '';
}

/** Seconds-since-epoch → `Date`. The API sends unix seconds, not milliseconds. */
export function toDate(seconds: number): Date {
  return new Date(seconds * 1000);
}

const TIME_FMT = new Intl.DateTimeFormat(undefined, { hour: 'numeric', minute: '2-digit' });
const SHORT_DATE_FMT = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' });
const FULL_FMT = new Intl.DateTimeFormat(undefined, {
  weekday: 'long',
  year: 'numeric',
  month: 'long',
  day: 'numeric',
  hour: 'numeric',
  minute: '2-digit',
});

function startOfToday(): Date {
  const now = new Date();
  return new Date(now.getFullYear(), now.getMonth(), now.getDate());
}

function isSameDay(a: Date, b: Date): boolean {
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

/**
 * The compact timestamp for a list row.
 *
 * Today's articles show a clock time, because "when this morning" is what matters while scanning;
 * older ones show a date, because a bare "3:14 PM" on an article from last week is meaningless.
 */
export function formatListTime(seconds: number): string {
  const date = toDate(seconds);
  const now = new Date();
  if (isSameDay(date, now)) return TIME_FMT.format(date);

  const yesterday = new Date(startOfToday());
  yesterday.setDate(yesterday.getDate() - 1);
  if (isSameDay(date, yesterday)) return 'Yesterday';

  // Within the current year the month and day are enough; across years, add the year.
  if (date.getFullYear() === now.getFullYear()) return SHORT_DATE_FMT.format(date);
  return new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', year: 'numeric' }).format(
    date,
  );
}

/** The full timestamp for `.rd-article-byline`. */
export function formatFullTime(seconds: number): string {
  return FULL_FMT.format(toDate(seconds));
}

/** An ISO-8601 string for the `<time datetime>` attribute. */
export function isoTime(seconds: number): string {
  return toDate(seconds).toISOString();
}
