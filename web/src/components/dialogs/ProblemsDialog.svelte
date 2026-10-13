<script lang="ts">
  import { api, ApiError, type FeedRefreshResult, type ProblemFeed } from '../../lib/api';
  import { formatRelativeTime } from '../../lib/format';
  import { selectScope } from '../../lib/view.svelte';

  interface Props {
    /** True while `#rd-problems-dialog` should be open. */
    open: boolean;
    onclose: () => void;
    /** Called after a retry fetched articles, so the sidebar and the list can catch up. */
    onChanged: () => Promise<void> | void;
  }

  let { open, onclose, onChanged }: Props = $props();

  let dialogEl: HTMLDialogElement | undefined = $state();
  let loading = $state(false);
  let loadError = $state<string | null>(null);
  let feeds = $state<ProblemFeed[]>([]);

  /** Per-row retry state, by feed id, so one slow feed does not block the others. */
  let retries = $state<Record<string, { pending: boolean; result: FeedRefreshResult | null; error: string | null }>>({});

  $effect(() => {
    if (!dialogEl) return;
    if (!open) {
      if (dialogEl.open) dialogEl.close();
    } else {
      if (!dialogEl.open) dialogEl.showModal();
      void load();
    }
  });

  async function load(): Promise<void> {
    loading = true;
    loadError = null;
    try {
      const data = await api.problems();
      feeds = data.feeds;
    } catch (err) {
      loadError = err instanceof ApiError ? err.message : 'Could not load the problem feeds.';
    } finally {
      loading = false;
    }
  }

  async function retry(feed: ProblemFeed): Promise<void> {
    if (retries[feed.id]?.pending) return;
    retries[feed.id] = { pending: true, result: null, error: null };
    try {
      const result = await api.refreshFeed(feed.id);
      retries[feed.id] = { pending: false, result, error: null };
      // Update the row in place rather than re-fetching the list: re-fetching would drop a feed
      // that just recovered, and with it the outcome the reader is waiting to read.
      if (result.status !== 'failed' && result.status !== 'muted') {
        feed.error = 0;
        feed.last_update = Math.floor(Date.now() / 1000);
      }
      // Counts and the sidebar's "feeds have problems" badge changed.
      void Promise.resolve(onChanged());
    } catch (err) {
      retries[feed.id] = {
        pending: false,
        result: null,
        error: err instanceof ApiError ? err.message : 'Could not retry the feed.',
      };
    }
  }

  /** The outcome line under a retried row. */
  function resultText(result: FeedRefreshResult): string {
    switch (result.status) {
      case 'fetched':
        return result.new_entries > 0
          ? `${result.new_entries} new ${result.new_entries === 1 ? 'article' : 'articles'}`
          : 'Fetched; nothing new';
      case 'unchanged':
        return 'Fetched; nothing changed';
      case 'not_modified':
        return 'Unchanged since the last check';
      case 'muted':
        return 'Muted, so the refresh was skipped';
      case 'failed':
        return result.message ?? 'Still failing';
    }
  }

  /**
   * The reason line, kept apart for muted feeds on purpose: a muted feed is the reader's own choice
   * and reads as a plain fact, while a failing feed is something that needs attention.
   */
  function reasonText(feed: ProblemFeed): string {
    if (feed.muted) return 'Muted by you — refreshes are paused while it is muted.';
    if (feed.error > 0) return `The last check failed ${formatRelativeTime(feed.error)}.`;
    return 'The last check succeeded.';
  }

  /** Clicking a feed name reads it, like the sidebar would, instead of a full page load. */
  function openFeed(event: MouseEvent, feed: ProblemFeed): void {
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || event.button !== 0) return;
    event.preventDefault();
    onclose();
    selectScope({ kind: 'feed', id: feed.id, name: feed.name });
    window.history.pushState({}, '', `/?feed=${feed.id}`);
  }

  function handleClose(): void {
    onclose();
  }
</script>

<dialog
  id="rd-problems-dialog"
  class="rd-dialog rd-dialog--wide"
  bind:this={dialogEl}
  aria-labelledby="rd-problems-title"
  oncancel={(e) => {
    e.preventDefault();
    handleClose();
  }}
>
  <div class="rd-dialog-body">
    <h2 id="rd-problems-title">Feeds with problems</h2>
    <p class="rd-hint">These feeds failed their last check, or you have muted them. Retry one to fetch it now.</p>

    {#if loadError}
      <p class="rd-field-error" role="alert">{loadError}</p>
    {:else if loading && feeds.length === 0}
      <p class="rd-hint">Loading…</p>
    {:else if feeds.length === 0}
      <p class="rd-hint">Nothing needs attention.</p>
    {:else}
      <ul class="rd-problems">
        {#each feeds as feed (feed.id)}
          {@const attempt = retries[feed.id]}
          <li>
            <a
              class="rd-problems-name"
              href={`/?feed=${feed.id}`}
              title={feed.name}
              onclick={(e) => openFeed(e, feed)}
            >
              {feed.name}
            </a>
            <p class="rd-problems-reason">{reasonText(feed)}</p>
            <dl>
              <dt>Feed address</dt>
              <dd>{feed.url}</dd>
              <dt>Last update</dt>
              <dd>{formatRelativeTime(feed.last_update)}</dd>
            </dl>
            {#if attempt?.result}
              <p
                class={attempt.result.status === 'failed' ? 'rd-field-error' : 'rd-hint'}
                role="status"
              >
                {resultText(attempt.result)}
              </p>
            {:else if attempt?.error}
              <p class="rd-field-error" role="alert">{attempt.error}</p>
            {/if}
            <button
              class="rd-btn rd-btn--quiet"
              type="button"
              disabled={attempt?.pending === true}
              onclick={() => void retry(feed)}
            >
              <i class="ri-refresh-line" class:is-spinning={attempt?.pending === true} aria-hidden="true"></i>
              {attempt?.pending ? 'Retrying…' : 'Retry'}
            </button>
          </li>
        {/each}
      </ul>
    {/if}

    <div class="rd-dialog-actions">
      <button class="rd-btn rd-btn--primary" type="button" onclick={handleClose}>Close</button>
    </div>
  </div>
</dialog>
