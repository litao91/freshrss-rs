/**
 * The refresh-everything job (`r`, and the top bar's refresh button).
 *
 * `POST /refresh` only enqueues: the server answers 202 with a job id and the work is watched by
 * polling `GET /refresh/{id}` until the state stops being `running`. The poller lives here rather
 * than in a component for two reasons:
 *
 * **Resume.** A long job outlives a page reload, so startup probes `GET /refresh/status` once and
 * re-enters the polling loop when the server is still busy — the spinner comes back instead of the
 * reader reporting idle while feeds are being fetched.
 *
 * **One job.** The guard on `running` means the button, the shortcut and a stray double-press all
 * converge on the same job; a second `POST /refresh` is never sent while one is in flight.
 */

import { api, ApiError, type RefreshJobError } from './api';
import { reload as reloadEntries } from './entries.svelte';
import { refreshTree } from './session.svelte';
import { notify, notifyError } from './toast.svelte';

/** The poll interval. Feed fetches take longer than this anyway; no point hammering. */
const POLL_MS = 1500;

/** Consecutive failed polls tolerated before giving up: a blip must not kill a long job's UI. */
const MAX_POLL_FAILURES = 3;

export const refreshJob = $state({
  running: false,
  jobId: null as string | null,
  feedsDone: 0,
  newEntries: 0,
  updatedEntries: 0,
  errors: [] as RefreshJobError[],
});

let timer: ReturnType<typeof setTimeout> | null = null;
/** Bumped on every stop, so a late poll response cannot revive a finished or abandoned job. */
let generation = 0;
let pollFailures = 0;

function message(err: unknown): string {
  return err instanceof Error ? err.message : 'Something went wrong';
}

function clearTimer(): void {
  if (timer !== null) {
    clearTimeout(timer);
    timer = null;
  }
}

/** Stops polling and forgets the job. The job itself keeps running server-side; that is fine. */
export function stopPolling(): void {
  generation += 1;
  clearTimer();
}

function settle(): void {
  stopPolling();
  refreshJob.running = false;
  refreshJob.jobId = null;
  pollFailures = 0;
}

/** Starts a new job. Pressing again while one runs is a no-op, not a second job. */
export async function startRefresh(): Promise<void> {
  // `running` flips before the POST resolves, so a double-press inside the request window is
  // already covered by the guard — the button being disabled is belt and braces on top.
  if (refreshJob.running) return;
  refreshJob.running = true;
  refreshJob.feedsDone = 0;
  refreshJob.newEntries = 0;
  refreshJob.updatedEntries = 0;
  refreshJob.errors = [];
  pollFailures = 0;
  try {
    const { job_id } = await api.startRefresh();
    refreshJob.jobId = job_id;
    schedule();
  } catch (err) {
    refreshJob.running = false;
    notifyError(message(err));
  }
}

/**
 * Call once at startup, after the session is restored. A probe that fails stays silent: the server
 * being down is reported by the next action the reader takes, not by a toast at boot.
 */
export async function resumeRefresh(): Promise<void> {
  if (refreshJob.running) return;
  try {
    const status = await api.refreshStatus();
    if (!status.running || !status.job_id) return;
    refreshJob.running = true;
    refreshJob.jobId = status.job_id;
    refreshJob.feedsDone = status.feeds_done;
    refreshJob.newEntries = status.new_entries;
    pollFailures = 0;
    schedule();
  } catch {
    // Idle is the safe assumption; nothing worth interrupting the reader for.
  }
}

function schedule(): void {
  clearTimer();
  const mine = generation;
  timer = setTimeout(() => {
    timer = null;
    void pollOnce(mine);
  }, POLL_MS);
}

async function pollOnce(mine: number): Promise<void> {
  if (mine !== generation || !refreshJob.jobId) return;
  try {
    const job = await api.refreshJob(refreshJob.jobId);
    if (mine !== generation) return;
    pollFailures = 0;
    refreshJob.feedsDone = job.feeds_done;
    refreshJob.newEntries = job.new_entries;
    refreshJob.updatedEntries = job.updated_entries;
    refreshJob.errors = job.errors;
    if (job.state === 'running') {
      schedule();
      return;
    }
    await finish(job.state === 'done');
  } catch (err) {
    if (mine !== generation) return;
    if (err instanceof ApiError && err.isUnauthorized) {
      // The session died mid-job; polling on would only collect 401s.
      settle();
      return;
    }
    pollFailures += 1;
    if (pollFailures >= MAX_POLL_FAILURES) {
      settle();
      notifyError(`Lost touch with the refresh job: ${message(err)}`);
      return;
    }
    // One failed poll is a blip; the job is still running on the server.
    schedule();
  }
}

async function finish(succeeded: boolean): Promise<void> {
  const { feedsDone, newEntries, errors } = refreshJob;
  settle();

  // A failed job may still have ingested articles, so the tree and the list reload either way.
  try {
    await refreshTree();
  } catch (err) {
    notifyError(message(err));
  }
  await reloadEntries();

  const articles = `${newEntries} new ${newEntries === 1 ? 'article' : 'articles'}`;
  if (!succeeded) {
    notifyError(`Refresh failed after ${feedsDone} ${feedsDone === 1 ? 'feed' : 'feeds'} (${articles}).`);
  } else if (errors.length > 0) {
    notifyError(
      `Refreshed ${feedsDone} ${feedsDone === 1 ? 'feed' : 'feeds'}: ${articles}, ` +
        `${errors.length} ${errors.length === 1 ? 'failure' : 'failures'}.`,
    );
  } else {
    notify(`Refreshed ${feedsDone} ${feedsDone === 1 ? 'feed' : 'feeds'}: ${articles}.`);
  }
}
