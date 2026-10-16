/**
 * The API client.
 *
 * Every call goes through `request`, which is the one place that knows the envelope shape, turns a
 * non-2xx into a thrown `ApiError`, and — importantly — does *not* retry or swallow failures. Callers
 * decide what a failure means to the user.
 */

/** Mirrors the server's `{"error":{"code","message"}}` envelope. */
export class ApiError extends Error {
  readonly code: string;
  readonly status: number;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = 'ApiError';
    this.code = code;
    this.status = status;
  }

  /** True when re-authenticating is the only sensible response. */
  get isUnauthorized(): boolean {
    return this.status === 401;
  }
}

export interface RequestOptions {
  method?: 'GET' | 'POST' | 'PUT' | 'DELETE' | 'PATCH';
  body?: unknown;
  /** A pre-encoded body (the OPML bytes); takes precedence over `body` and is sent as-is. */
  raw?: BodyInit;
  /** The `Content-Type` for a `raw` body. */
  contentType?: string;
  /** Abort signal, for cancelling an in-flight list fetch when the user navigates. */
  signal?: AbortSignal;
  headers?: Record<string, string>;
}

async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const { method = 'GET', body, raw, contentType, signal, headers } = options;

  let res: Response;
  try {
    res = await fetch(`/api/v1${path}`, {
      method,
      signal,
      // `same-origin` is the default and is what we want: the cookie is SameSite=Strict, so a
      // cross-origin request would not carry it anyway.
      credentials: 'same-origin',
      headers: {
        // A raw body carries its own type (or none); a JSON body is always `application/json`.
        ...(raw !== undefined
          ? contentType
            ? { 'Content-Type': contentType }
            : {}
          : body !== undefined
            ? { 'Content-Type': 'application/json' }
            : {}),
        ...headers,
      },
      body: raw ?? (body === undefined ? undefined : JSON.stringify(body)),
    });
  } catch (err) {
    // A network failure is not an ApiError; it has no status and no code.
    throw new ApiError(0, 'network', err instanceof Error ? err.message : 'network request failed');
  }

  if (res.status === 204) {
    return undefined as T;
  }

  const text = await res.text();
  const parsed: unknown = text ? safeJson(text) : null;

  if (!res.ok) {
    const envelope = parsed as { error?: { code?: string; message?: string } } | null;
    throw new ApiError(
      res.status,
      envelope?.error?.code ?? 'unknown',
      envelope?.error?.message ?? `request failed with HTTP ${res.status}`,
    );
  }

  return parsed as T;
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    // A proxy or captive portal returning HTML is a real failure mode; report it as such rather
    // than throwing a confusing SyntaxError from deep inside the call stack.
    throw new ApiError(0, 'bad_response', 'the server returned a response that was not JSON');
  }
}

// --- types shared with the server's DTOs ---------------------------------

export interface User {
  id: string;
  username: string;
  email: string | null;
  is_admin: boolean;
  created_at: number;
  initial: string;
}

export interface UserConfig {
  ttl_default: number;
  sort: string;
  sort_order: 'asc' | 'desc';
  secondary_sort: string;
  secondary_sort_order: 'asc' | 'desc';
  posts_per_page: number;
  language: string;
}

export interface Favicon {
  url?: string;
  letter: string;
  color: string;
}

export interface FeedNode {
  id: string;
  name: string;
  url: string;
  website: string;
  description: string;
  kind: number;
  priority: number;
  ttl: number;
  muted: boolean;
  category: number | null;
  error: number;
  last_update: number;
  unread: number;
  total: number;
  favicon: Favicon;
}

export interface CategoryNode {
  id: string;
  name: string;
  kind: number;
  position: number;
  error: number;
  last_update: number;
  unread: number;
  total: number;
  is_dynamic_opml: boolean;
  feeds: FeedNode[];
}

export interface ScopeCounts {
  unread: number;
  total: number;
}

export interface LabelCount {
  id: string;
  name: string;
  unread: number;
  total: number;
}

export interface Counts {
  all: ScopeCounts;
  main_stream: ScopeCounts;
  important: ScopeCounts;
  starred: ScopeCounts;
  labels: LabelCount[];
}

export interface Tree {
  categories: CategoryNode[];
  orphans: FeedNode[];
  counts: Counts;
  problems: { count: number };
}

export interface Bootstrap {
  user: User;
  config: UserConfig;
  tree: Tree;
  version: { freshrss_rs: string; schema: number };
}

export interface SessionInfo {
  id: string;
  created_at: number;
  expires_at: number;
  ip: string | null;
  user_agent: string | null;
  current: boolean;
}

/** What the server's registration policy allows, fetched before anyone has a session. */
export interface AuthPolicy {
  mode: 'open' | 'first-user-only' | 'closed';
  can_register: boolean;
}

// --- feed discovery ----------------------------------------------------------

/**
 * Where a discovered candidate came from.
 *
 * `given` is the address the user typed; `link`/`anchor`/`area` are feeds the page *declared*
 * (`<link rel="alternate">` and friends); the `*_extension` and `*_body` sources are guesses —
 * URLs that merely look like feeds, found next to the page or in its text — and the UI says so.
 */
export type CandidateSource =
  | 'given'
  | 'link'
  | 'anchor'
  | 'area'
  | 'local_extension'
  | 'local_body'
  | 'remote_extension'
  | 'remote_body';

export interface FeedCandidate {
  url: string;
  source: CandidateSource;
  /** False when the address was tried and turned out not to be a feed; `rejected` says why. */
  verified: boolean;
  title: string | null;
  website: string | null;
  description: string | null;
  icon: string | null;
  item_count: number | null;
  format: string | null;
  /** The rejection reason, or null for a candidate that is a feed. */
  rejected: string | null;
}

export interface DiscoverResult {
  /** True when the given address is itself a feed; the lone `given` candidate is the answer. */
  is_feed: boolean;
  url: string;
  candidates: FeedCandidate[];
}

// --- feed filter rules -----------------------------------------------------------

/** One of the three actions a filter rule can perform, as the server serializes them. */
export type FilterActionName = 'read' | 'star' | 'label';

/** One `{search, actions}` rule; both halves are required or the server refuses the rule. */
export interface FilterRule {
  search: string;
  actions: FilterActionName[];
}

// --- OPML import/export and starter packs ----------------------------------------

/** What one OPML import — or one starter-pack install — did. */
export interface ImportReport {
  created_feeds: number;
  skipped_feeds: number;
  created_categories: number;
  /** One line per feed that could not be added. */
  errors: string[];
}

/** A subscription starter pack: a themed bundle of feeds that installs into its own folder. */
export interface Package {
  slug: string;
  name: string;
  description: string;
  feeds: { url: string; title: string }[];
}

/** Plain `<a href>` target: the session cookie rides along, so a link is the whole download UI. */
export const OPML_EXPORT_URL = '/api/v1/export/opml';

// --- problems ------------------------------------------------------------------

/** A feed that needs attention: its last check failed, or the reader deliberately muted it. */
export interface ProblemFeed {
  id: string;
  name: string;
  url: string;
  /** Unix seconds of the last failed check; 0 means no error. */
  error: number;
  muted: boolean;
  /** Unix seconds of the last successful refresh; 0 means never. */
  last_update: number;
  unread: number;
  total: number;
}

// --- refresh -------------------------------------------------------------------

export type FeedRefreshStatus = 'fetched' | 'unchanged' | 'not_modified' | 'muted' | 'failed';

/** The outcome of refreshing one feed. `message` is present when `status` is `failed`. */
export interface FeedRefreshResult {
  new_entries: number;
  updated_entries: number;
  unchanged: number;
  status: FeedRefreshStatus;
  message?: string;
}

export interface RefreshJobError {
  feed_id: string;
  name: string;
  message: string;
  http_status: number | null;
}

export interface RefreshJob {
  state: 'running' | 'done' | 'failed';
  feeds_done: number;
  new_entries: number;
  updated_entries: number;
  errors: RefreshJobError[];
  /** Unix seconds, or null while the job is still running. */
  finished_at: number | null;
}

/** The startup probe: is a job already running server-side (e.g. across a page reload)? */
export interface RefreshStatus {
  running: boolean;
  job_id: string | null;
  feeds_done: number;
  new_entries: number;
}

// --- entries ----------------------------------------------------------------

export interface LabelRef {
  id: string;
  name: string;
}

export interface Enclosure {
  url: string;
  type?: string;
  medium?: string;
  length?: number;
  title?: string;
  description?: string;
  height?: number;
  width?: number;
}

/** One row of the list pane. Deliberately carries no body: a page of fifty full articles would be
 * megabytes, and the reading pane fetches the one it needs. */
export interface EntrySummary {
  id: string;
  feed_id: string;
  feed_name: string;
  category_id: string | null;
  category_name: string | null;
  title: string;
  link: string;
  author: string;
  summary: string;
  date: number;
  last_modified: number | null;
  last_seen: number;
  is_read: boolean;
  is_favorite: boolean;
  tags: string[];
  labels: LabelRef[];
  thumbnail: string | null;
  has_enclosures: boolean;
  has_media: boolean;
  is_updated: boolean;
}

export interface EntryPage {
  items: EntrySummary[];
  /** Opaque; send back unchanged for the next page. `null` when there is none. */
  cursor: string | null;
  has_more: boolean;
  /** The snapshot fence. Send it back on later pages so articles arriving mid-scroll do not shift them. */
  id_max: string;
  unread_in_scope: number;
}

export interface EntryFull extends EntrySummary {
  content_html: string;
  content_text: string;
  /** The body scraped from the article's own page, once the reader has asked for it. */
  full_content?: string | null;
  guid: string;
  enclosures: Enclosure[];
}

/** What an on-demand full-text extraction produced. */
export interface FulltextResult {
  content_html: string;
  matched: number;
  url: string;
  selector: string;
  stored: boolean;
}

export interface FeedRef {
  id: string;
  name: string;
  website: string;
}

export interface EntryDetail {
  entry: EntryFull;
  prev_id: string | null;
  next_id: string | null;
  feed: FeedRef;
}

export interface SearchParse {
  canonical: string;
  ast: unknown;
  warnings: string[];
  min_priority: number | null;
}

export interface SearchPreview {
  matches: boolean;
  explain: { operator: string; matched: boolean; negated: boolean }[];
}

/** The listing parameters. Every one maps straight onto a `GET /entries` query parameter. */
export interface EntryListParams {
  scope?: string;
  id?: string;
  read?: 'unread' | 'read' | 'any';
  starred?: 'yes' | 'no' | 'any';
  or_unread?: boolean;
  or_starred?: boolean;
  search?: string;
  sort?: string;
  order?: 'asc' | 'desc';
  secondary_sort?: string;
  secondary_order?: 'asc' | 'desc';
  id_max?: string;
  id_min?: string;
  cursor?: string;
  limit?: number;
}

/** Builds a query string, dropping anything undefined so absent parameters stay absent. */
function query(params: EntryListParams): string {
  const out = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null || value === '') continue;
    out.set(key, String(value));
  }
  const text = out.toString();
  return text ? `?${text}` : '';
}

// --- endpoints -------------------------------------------------------------

/**
 * Routes a remote image through `GET /api/v1/media`, naming the article it belongs to.
 *
 * Image hosts police `Referer` in both directions and a browser cannot win either way: some accept
 * only their own site as referer, and the only referer a page served from this reader can give is
 * the reader's own origin. The proxy replays the article's address instead. Local and data: URLs
 * pass through untouched.
 */
export function mediaUrl(url: string, via: string): string {
  if (!/^https?:/i.test(url)) return url;
  const params = new URLSearchParams({ url, via });
  return `/api/v1/media?${params.toString()}`;
}

export const api = {
  bootstrap: () => request<Bootstrap>('/me'),

  register: (username: string, password: string, email?: string) =>
    request<{ user: User }>('/auth/register', {
      method: 'POST',
      body: { username, password, email: email ?? null },
    }),

  login: (username: string, password: string) =>
    request<{ user: User }>('/auth/login', { method: 'POST', body: { username, password } }),

  logout: () => request<void>('/auth/logout', { method: 'POST' }),

  authPolicy: () => request<AuthPolicy>('/auth/policy'),

  changePassword: (current: string, next: string) =>
    request<{ revoked_sessions: number }>('/auth/password', {
      method: 'PUT',
      body: { current, next },
    }),

  sessions: () => request<SessionInfo[]>('/me/sessions'),

  revokeSession: (id: string) => request<void>(`/me/sessions/${encodeURIComponent(id)}`, { method: 'DELETE' }),

  tree: () => request<Tree>('/tree'),

  categories: () => request<CategoryNode[]>('/categories'),

  createCategory: (name: string) =>
    request<CategoryNode>('/categories', { method: 'POST', body: { name } }),

  renameCategory: (id: string, name: string) =>
    request<CategoryNode>(`/categories/${id}`, { method: 'PUT', body: { name } }),

  deleteCategory: (id: string) =>
    request<{ deleted: string; orphaned_feeds: number }>(`/categories/${id}`, { method: 'DELETE' }),

  reorderCategories: (ids: string[]) =>
    request<{ repositioned: number }>('/categories/order', { method: 'PUT', body: { ids } }),

  feeds: () => request<FeedNode[]>('/feeds'),

  /** Probes an address and reports what feeds it yields, without subscribing to anything. */
  discoverFeeds: (url: string) =>
    request<DiscoverResult>('/feeds/discover', { method: 'POST', body: { url } }),

  /** Subscribes, and eagerly fetches the feed: `new_entries` is what arrived right now. */
  createFeed: (body: {
    url: string;
    name?: string;
    category_id?: string | null;
    priority?: number;
    ttl?: number;
    muted?: boolean;
  }) => request<{ feed: FeedNode; new_entries: number }>('/feeds', { method: 'POST', body }),

  updateFeed: (
    id: string,
    body: {
      name?: string;
      url?: string;
      /** Folder ids travel as JSON strings; the server's `deserialize_id` accepts both forms. */
      category_id?: string;
      clear_category?: boolean;
      priority?: number;
      ttl?: number;
      kind?: number;
      muted?: boolean;
      path_entries?: string;
      path_entries_filter?: string;
      /** Both halves together; an empty username clears the stored credentials. */
      http_auth?: { username: string; password: string };
    },
  ) => request<FeedNode>(`/feeds/${id}`, { method: 'PUT', body }),

  /** One feed, fresh from the server — what `#rd-feed-dialog` seeds its form from. */
  feed: (id: string) => request<FeedNode>(`/feeds/${encodeURIComponent(id)}`),

  feedFilters: (id: string) =>
    request<{ filters: FilterRule[] }>(`/feeds/${encodeURIComponent(id)}/filters`),

  /** Replaces the feed's rules wholesale; each search is parsed server-side and refused if bad. */
  setFeedFilters: (id: string, filters: FilterRule[]) =>
    request<{ filters: FilterRule[] }>(`/feeds/${encodeURIComponent(id)}/filters`, {
      method: 'PUT',
      body: { filters },
    }),

  deleteFeed: (id: string) => request<void>(`/feeds/${id}`, { method: 'DELETE' }),

  moveFeed: (id: string, category_id: string | null) =>
    request<void>(`/feeds/${id}/move`, { method: 'POST', body: { category_id } }),

  truncateFeed: (id: string) =>
    request<{ deleted: number }>(`/feeds/${id}/entries`, { method: 'DELETE' }),

  /** Fetches one feed right now, out of band from the full refresh job. */
  refreshFeed: (id: string) =>
    request<FeedRefreshResult>(`/feeds/${encodeURIComponent(id)}/refresh`, { method: 'POST' }),

  // --- problems and refresh jobs ---

  problems: () => request<{ count: number; feeds: ProblemFeed[] }>('/problems'),

  /** Enqueues a refresh of everything and returns immediately; poll `refreshJob` for progress. */
  startRefresh: () => request<{ job_id: string }>('/refresh', { method: 'POST', body: {} }),

  refreshJob: (jobId: string) => request<RefreshJob>(`/refresh/${encodeURIComponent(jobId)}`),

  /** The once-at-startup probe used to resume the spinner after a page reload mid-job. */
  refreshStatus: () => request<RefreshStatus>('/refresh/status'),

  // --- entries ---

  entries: (params: EntryListParams, signal?: AbortSignal) =>
    request<EntryPage>(`/entries${query(params)}`, { signal }),

  entry: (id: string, params: EntryListParams = {}, signal?: AbortSignal) =>
    request<EntryDetail>(`/entries/${encodeURIComponent(id)}${query(params)}`, { signal }),

  markRead: (ids: string[]) =>
    request<{ marked: number }>('/entries/read', { method: 'POST', body: { ids } }),

  markUnread: (ids: string[]) =>
    request<{ marked: number }>('/entries/unread', { method: 'POST', body: { ids } }),

  star: (ids: string[]) =>
    request<{ marked: number }>('/entries/star', { method: 'POST', body: { ids } }),

  unstar: (ids: string[]) =>
    request<{ marked: number }>('/entries/unstar', { method: 'POST', body: { ids } }),

  markAllRead: (body: EntryListParams & { star?: boolean }) =>
    request<{ marked: number }>('/entries/mark-all-read', { method: 'POST', body }),

  addLabels: (id: string, names: string[]) =>
    request<LabelRef[]>(`/entries/${encodeURIComponent(id)}/labels`, {
      method: 'POST',
      body: { names },
    }),

  removeLabel: (id: string, name: string) =>
    request<LabelRef[]>(
      `/entries/${encodeURIComponent(id)}/labels/${encodeURIComponent(name)}`,
      { method: 'DELETE' },
    ),

  /** Fetches the article body from the article's own page, and stores it on the entry. */
  fulltext: (id: string) =>
    request<FulltextResult>(`/entries/${encodeURIComponent(id)}/fulltext`, {
      method: 'POST',
      body: {},
    }),

  parseSearch: (q: string) => request<SearchParse>('/search/parse', { method: 'POST', body: { q } }),

  previewSearch: (q: string, entry_id: string) =>
    request<SearchPreview>('/search/preview', { method: 'POST', body: { q, entry_id } }),

  // --- OPML import and starter packs ---

  /**
   * Posts the raw OPML bytes; `dry_run=true` previews without touching the subscriptions.
   * `forceCategory` files everything under one folder instead of keeping the file's own.
   */
  importOpml: (data: BodyInit, dryRun: boolean, forceCategory?: string) => {
    const q = new URLSearchParams({ dry_run: String(dryRun) });
    if (forceCategory) q.set('force_category', forceCategory);
    return request<ImportReport>(`/import/opml?${q.toString()}`, {
      method: 'POST',
      raw: data,
      contentType: 'application/xml',
    });
  },

  packages: () => request<Package[]>('/packages'),

  installPackage: (slug: string) =>
    request<ImportReport>(`/packages/${encodeURIComponent(slug)}/install`, {
      method: 'POST',
      body: {},
    }),
};
