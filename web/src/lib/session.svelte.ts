/**
 * Authentication state.
 *
 * `bootstrap()` is called once at startup: it either restores a session from the cookie or leaves
 * the app signed out. A 401 anywhere in the app routes through `handleUnauthorized` so a session
 * that expired in another tab does not leave the reader showing stale data.
 */

import {
  api,
  ApiError,
  type AuthPolicy,
  type Bootstrap,
  type Counts,
  type Tree,
  type User,
  type UserConfig,
} from './api';

interface SessionState {
  user: User | null;
  config: UserConfig | null;
  version: { freshrss_rs: string; schema: number } | null;
  /**
   * The server's registration policy, or null while it is unknown. The login page offers a sign-up
   * form only when `can_register` is true; a failed probe leaves it null and the form stays, since
   * the endpoint remains the authority and refuses what it must.
   */
  policy: AuthPolicy | null;
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
  policy: null,
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
  // The policy is fetched alongside, not after: the login page renders as soon as `/me` settles and
  // must not flash a sign-up form a closed server will refuse.
  const policy = api.authPolicy().catch(() => null);
  try {
    const [boot, settled] = await Promise.all([api.bootstrap(), policy]);
    session.policy = settled;
    apply(boot);
  } catch (err) {
    session.policy = await policy;
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

/** Replaces the counts in place, used after a mutation changed unread numbers. */
export function applyCounts(counts: Counts): void {
  if (tree.data) tree.data = { ...tree.data, counts };
}

/**
 * Re-reads just the counts, for the sidebar badges after a read or star mutation.
 *
 * Cheaper than `refreshTree` and enough: the mutation already reloaded what it touched, and a badge
 * that keeps claiming three unread after the last one was marked read is the kind of lie a reader
 * notices. A failed sync is cosmetic, so it is swallowed rather than toasted.
 */
export async function syncCounts(): Promise<void> {
  try {
    applyCounts(await api.counts());
  } catch {
    // The next mutation or refresh will pick the counts up.
  }
}

/** Call from any handler that sees a 401. */
export function handleUnauthorized(): void {
  clear();
  tree.data = null;
}
