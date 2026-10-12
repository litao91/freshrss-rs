/**
 * Authentication state.
 *
 * `bootstrap()` is called once at startup: it either restores a session from the cookie or leaves
 * the app signed out. A 401 anywhere in the app routes through `handleUnauthorized` so a session
 * that expired in another tab does not leave the reader showing stale data.
 */

import { api, ApiError, type Bootstrap, type Counts, type Tree, type User, type UserConfig } from './api';

interface SessionState {
  user: User | null;
  config: UserConfig | null;
  version: { freshrss_rs: string; schema: number } | null;
  /** True until the first `/me` resolves, so the UI can show a splash instead of a login form. */
  loading: boolean;
  /**
   * Set when startup could not reach the server at all. Distinct from "signed out": showing a login
   * form when the API is down just produces a second, more confusing failure.
   */
  error: string | null;
}

export const session = $state<SessionState>({
  user: null,
  config: null,
  version: null,
  loading: true,
  error: null,
});

export const tree = $state<{ data: Tree | null }>({ data: null });

export function isSignedIn(): boolean {
  return session.user !== null;
}

function apply(bootstrap: Bootstrap): void {
  session.user = bootstrap.user;
  session.config = bootstrap.config;
  session.version = bootstrap.version;
  tree.data = bootstrap.tree;
  session.loading = false;
}

function clear(): void {
  session.user = null;
  session.config = null;
  session.loading = false;
}

/** Restores a session, or leaves us signed out. Never throws: startup must not depend on it. */
export async function bootstrap(): Promise<void> {
  session.loading = true;
  try {
    apply(await api.bootstrap());
  } catch (err) {
    if (err instanceof ApiError && err.isUnauthorized) {
      clear();
      return;
    }
    // Anything else (network down, 500) is not "signed out" — say so rather than showing a login
    // form that will also fail.
    clear();
    session.error = err instanceof Error ? err.message : 'could not reach the server';
  }
}

export async function signIn(username: string, password: string): Promise<void> {
  session.error = null;
  await api.login(username, password);
  // Re-fetch the bootstrap payload rather than assembling it from the login response: the tree and
  // config have to come from the same place on both the login and the reload path.
  apply(await api.bootstrap());
}

export async function signUp(username: string, password: string, email?: string): Promise<void> {
  await api.register(username, password, email);
  apply(await api.bootstrap());
}

export async function signOut(): Promise<void> {
  try {
    await api.logout();
  } finally {
    // Clear local state even if the request failed; the server-side session is best-effort here
    // and a stuck UI is worse than an orphaned row that the sweep will collect.
    clear();
    tree.data = null;
  }
}

/** Refreshes just the counts and tree, after a mutation changed unread numbers. */
export async function refreshTree(): Promise<void> {
  if (!session.user) return;
  tree.data = await api.tree();
}

/** Replaces the counts in place, used by the cheap `GET /counts` poll. */
export function applyCounts(counts: Counts): void {
  if (tree.data) tree.data = { ...tree.data, counts };
}

/** Call from any handler that sees a 401. */
export function handleUnauthorized(): void {
  clear();
  tree.data = null;
}
