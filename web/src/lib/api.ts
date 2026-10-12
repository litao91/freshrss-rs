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
  /** Abort signal, for cancelling an in-flight list fetch when the user navigates. */
  signal?: AbortSignal;
  headers?: Record<string, string>;
}

async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const { method = 'GET', body, signal, headers } = options;

  let res: Response;
  try {
    res = await fetch(`/api/v1${path}`, {
      method,
      signal,
      // `same-origin` is the default and is what we want: the cookie is SameSite=Strict, so a
      // cross-origin request would not carry it anyway.
      credentials: 'same-origin',
      headers: {
        ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
        ...headers,
      },
      body: body === undefined ? undefined : JSON.stringify(body),
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

// --- endpoints -------------------------------------------------------------

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

  createFeed: (body: {
    url: string;
    name?: string;
    category_id?: string | null;
    priority?: number;
    ttl?: number;
    muted?: boolean;
  }) => request<FeedNode>('/feeds', { method: 'POST', body }),

  updateFeed: (
    id: string,
    body: {
      name?: string;
      url?: string;
      category_id?: string;
      clear_category?: boolean;
      priority?: number;
      ttl?: number;
      muted?: boolean;
      path_entries?: string;
      path_entries_filter?: string;
    },
  ) => request<FeedNode>(`/feeds/${id}`, { method: 'PUT', body }),

  deleteFeed: (id: string) => request<void>(`/feeds/${id}`, { method: 'DELETE' }),

  moveFeed: (id: string, category_id: string | null) =>
    request<void>(`/feeds/${id}/move`, { method: 'POST', body: { category_id } }),

  truncateFeed: (id: string) =>
    request<{ deleted: number }>(`/feeds/${id}/entries`, { method: 'DELETE' }),
};
