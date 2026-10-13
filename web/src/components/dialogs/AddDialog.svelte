<script lang="ts">
  import { api, ApiError, type CandidateSource, type FeedCandidate } from '../../lib/api';
  import { tree } from '../../lib/session.svelte';
  import { notify } from '../../lib/toast.svelte';

  interface Props {
    /** True while `#rd-add-dialog` should be open. */
    open: boolean;
    onclose: () => void;
    /** Called after subscribing so the sidebar and the list can pick up the new feed. */
    onChanged: () => Promise<void> | void;
  }

  let { open, onclose, onChanged }: Props = $props();

  let dialogEl: HTMLDialogElement | undefined = $state();
  let url = $state('');
  let name = $state('');
  /** The `#rd-add-folder` selection; `''` means "no folder" and becomes `category_id: null`. */
  let folder = $state('');
  let busy = $state<'idle' | 'discovering' | 'subscribing'>('idle');
  let formError = $state<string | null>(null);
  /** Discovery results; `null` until the first successful `POST /feeds/discover`. */
  let candidates = $state<FeedCandidate[] | null>(null);
  /** True when the address given was itself a feed, which skips the choosing step. */
  let isFeed = $state(false);
  let selected = $state<FeedCandidate | null>(null);

  const categories = $derived(tree.data?.categories ?? []);
  const verified = $derived(candidates?.filter((c) => c.verified) ?? []);

  /**
   * How much trust a source earns, in one line.
   *
   * A `link`/`anchor`/`area` hit is the publisher declaring its own feed; the `*_body` hits are
   * guesses off URLs that merely contain a word like "rss". The reader deserves to see that
   * difference rather than a flat list that pretends every row is equally good.
   */
  const SOURCE_LABELS: Record<CandidateSource, string> = {
    given: 'The address you entered',
    link: 'Declared by the page',
    anchor: 'Declared by the page',
    area: 'Declared by the page',
    local_extension: 'Feed-like address on this site',
    remote_extension: 'Feed-like address on another site',
    local_body: 'Guessed from the page text',
    remote_body: 'Guessed from the page text',
  };

  $effect(() => {
    if (!dialogEl) return;
    if (!open) {
      if (dialogEl.open) dialogEl.close();
    } else {
      reset();
      if (!dialogEl.open) dialogEl.showModal();
    }
  });

  function reset(): void {
    url = '';
    name = '';
    folder = '';
    busy = 'idle';
    formError = null;
    candidates = null;
    isFeed = false;
    selected = null;
  }

  function hostname(rawUrl: string): string {
    try {
      return new URL(rawUrl).hostname;
    } catch {
      return rawUrl;
    }
  }

  /** The provenance line under a verified candidate: how much the source is worth, plus the facts. */
  function candidateMeta(candidate: FeedCandidate): string {
    const parts = [SOURCE_LABELS[candidate.source]];
    if (candidate.item_count !== null) parts.push(`${candidate.item_count} items`);
    if (candidate.format !== null) parts.push(candidate.format);
    return parts.join(' · ');
  }

  /** Editing the address invalidates whatever the last discovery found. */
  function invalidateResults(): void {
    candidates = null;
    isFeed = false;
    selected = null;
    formError = null;
  }

  function choose(candidate: FeedCandidate | null): void {
    selected = candidate;
    // The name is prefilled from the candidate's own title; empty means "let the server decide".
    name = candidate?.title ?? '';
    formError = null;
  }

  async function discover(): Promise<void> {
    const trimmed = url.trim();
    if (!trimmed) {
      formError = 'Enter a site or feed address first.';
      return;
    }
    busy = 'discovering';
    formError = null;
    candidates = null;
    isFeed = false;
    selected = null;
    name = '';
    try {
      const result = await api.discoverFeeds(trimmed);
      candidates = result.candidates;
      isFeed = result.is_feed;
      if (result.is_feed) {
        // The address given is itself the feed; there is nothing to choose.
        choose(result.candidates.find((c) => c.source === 'given') ?? null);
        if (!selected && result.url) {
          // Defensive: `is_feed` without a `given` candidate should not happen, but a missing row
          // must not strand the reader — fall back to the normalised address.
          choose({ url: result.url, source: 'given', verified: true, title: null, website: null, description: null, icon: null, item_count: null, format: null, rejected: null });
        }
      }
    } catch (err) {
      // The envelope's message ("could not reach …", "no content …") is the answer; a generic
      // "discovery failed" would hide exactly the part the reader can act on.
      formError = err instanceof ApiError ? err.message : 'Could not search for feeds.';
    } finally {
      busy = 'idle';
    }
  }

  async function subscribe(): Promise<void> {
    if (!selected) return;
    busy = 'subscribing';
    formError = null;
    try {
      const trimmedName = name.trim();
      const result = await api.createFeed({
        url: selected.url,
        name: trimmedName === '' ? undefined : trimmedName,
        category_id: folder === '' ? null : folder,
      });
      notify(
        result.new_entries > 0
          ? `Added “${result.feed.name}” — ${result.new_entries} new ${
              result.new_entries === 1 ? 'article' : 'articles'
            }.`
          : `Added “${result.feed.name}”.`,
      );
      await onChanged();
      onclose();
    } catch (err) {
      formError = err instanceof ApiError ? err.message : 'Could not add the subscription.';
    } finally {
      busy = 'idle';
    }
  }

  /** One form, two steps: Enter means "find" before a candidate is chosen, "subscribe" after. */
  function submit(event: Event): void {
    event.preventDefault();
    if (busy !== 'idle') return;
    void (selected ? subscribe() : discover());
  }

  function handleCancel(): void {
    onclose();
  }
</script>

<dialog
  id="rd-add-dialog"
  class="rd-dialog"
  bind:this={dialogEl}
  aria-labelledby="rd-add-title"
  oncancel={(e) => {
    e.preventDefault();
    handleCancel();
  }}
>
  <form class="rd-dialog-body" method="dialog" onsubmit={submit}>
    <h2 id="rd-add-title">Add subscription</h2>
    <p class="rd-dialog-lead">Paste a site or feed address. We'll find the feed.</p>

    <label class="rd-label" for="rd-add-url">Address</label>
    <input
      id="rd-add-url"
      class="rd-input"
      type="text"
      placeholder="example.com or https://example.com/feed"
      autocomplete="off"
      spellcheck="false"
      bind:value={url}
      oninput={invalidateResults}
    />

    {#if candidates !== null && !isFeed}
      {#if candidates.length === 0}
        <p class="rd-hint">No feed found at that address.</p>
      {:else}
        <p class="rd-label" id="rd-add-found">Feeds we found</p>
        <ul class="rd-packs" role="group" aria-labelledby="rd-add-found">
          {#each candidates as candidate (candidate.url)}
            <li>
              <button
                class="rd-pack"
                type="button"
                aria-pressed={candidate.verified ? selected?.url === candidate.url : undefined}
                disabled={!candidate.verified}
                onclick={() => choose(candidate)}
              >
                <span class="rd-pack-body">
                  <span class="rd-pack-name">{candidate.title ?? hostname(candidate.url)}</span>
                  <span class="rd-pack-desc">{candidate.url}</span>
                  {#if candidate.verified}
                    <span class="rd-pack-desc">{candidateMeta(candidate)}</span>
                  {:else}
                    <span class="rd-pack-desc">
                      Not a feed{candidate.rejected ? `: ${candidate.rejected}` : ''}
                    </span>
                  {/if}
                </span>
              </button>
            </li>
          {/each}
        </ul>
        {#if verified.length === 0}
          <p class="rd-hint">
            We tried {candidates.length} {candidates.length === 1 ? 'address' : 'addresses'} on that
            page and none was a feed.
          </p>
        {/if}
      {/if}
    {/if}

    {#if selected}
      <label class="rd-label" for="rd-add-name">Name</label>
      <input
        id="rd-add-name"
        class="rd-input"
        type="text"
        autocomplete="off"
        maxlength="191"
        placeholder={selected.title ?? hostname(selected.url)}
        bind:value={name}
      />
    {/if}

    <label class="rd-label" for="rd-add-folder">Folder</label>
    <select id="rd-add-folder" class="rd-input" bind:value={folder}>
      <option value="">No folder</option>
      {#each categories as category (category.id)}
        <option value={category.id}>{category.name}</option>
      {/each}
    </select>

    {#if formError}
      <p class="rd-field-error" role="alert">{formError}</p>
    {/if}

    <div class="rd-dialog-actions">
      <button class="rd-btn rd-btn--quiet" type="button" disabled={busy !== 'idle'} onclick={handleCancel}>
        Cancel
      </button>
      <button class="rd-btn rd-btn--primary" type="submit" disabled={busy !== 'idle'}>
        {#if busy === 'discovering'}Searching…{:else if busy === 'subscribing'}Adding…{:else if selected}Subscribe{:else}Find feeds{/if}
      </button>
    </div>
  </form>
</dialog>
