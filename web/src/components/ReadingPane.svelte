<script lang="ts">
  /**
   * The reading pane: `section.rd-read`.
   *
   * This is the one component that renders HTML from a feed, and it is the whole XSS surface of the
   * application. The body has already been sanitized on ingest by `ammonia`, and Svelte escapes
   * everything it interpolates — but `{@html}` bypasses that, so the body is passed through DOMPurify
   * here as well. Two independent filters is deliberate: a mistake in either one is survivable, and a
   * feed parser is exactly the kind of code where a mistake is eventually found.
   */
  import DOMPurify from 'dompurify';
  import { api, mediaUrl, type EntryDetail } from '../lib/api';
  import { entries, setLabels, setRead, setStarred } from '../lib/entries.svelte';
  import { formatFullTime } from '../lib/format';
  import { notifyError } from '../lib/toast.svelte';
  import { prefs, toggleFocus } from '../lib/prefs.svelte';
  import { view } from '../lib/view.svelte';

  let {
    entryId,
    onBack,
    onNavigate,
  }: {
    entryId: string;
    onBack: () => void;
    /** Asks the list to move the selection to another article. */
    onNavigate: (id: string) => void;
  } = $props();

  let detail = $state<EntryDetail | null>(null);
  let loading = $state(false);
  let error = $state<string | null>(null);
  let copied = $state(false);
  /** Whether the pane shows the scraped full article rather than the feed's own body. */
  let showFull = $state(false);
  let fetchingFull = $state(false);
  let scrollEl: HTMLElement | undefined = $state();
  let controller: AbortController | null = null;

  /**
   * Configured once rather than per render.
   *
   * `USE_PROFILES.html` keeps the SVG and MathML allowances out: an article body has no business
   * containing either, and both have been a source of mXSS. Links get `rel` and `target` added below
   * rather than being allowed to carry their own, so a feed cannot open a tab or drop the referrer
   * policy the reader expects.
   */
  DOMPurify.addHook('afterSanitizeAttributes', (node) => {
    if (node instanceof HTMLAnchorElement && node.hasAttribute('href')) {
      node.target = '_blank';
      node.rel = 'noopener noreferrer';
    }
    // Media starts paused and without a poster download, which is what the sanitizer on the server
    // already asked for; repeating it here means a body that reached the DOM by another route is safe.
    if (node instanceof HTMLMediaElement) {
      node.preload = 'none';
      node.removeAttribute('autoplay');
    }
    if (node instanceof HTMLImageElement) {
      // Lazy-loading placeholders: a scraped page's `<img src="1x1.gif" data-src="real.jpg">` never
      // runs the script that swaps them, so without this the article is a column of blank boxes.
      // The sanitizer keeps `data-*`, which is what makes the real address recoverable here.
      const lazy =
        node.dataset.src ?? node.dataset.original ?? node.dataset.lazySrc ?? node.dataset.actualsrc;
      if (lazy) node.setAttribute('src', lazy);
      if (node.dataset.srcset) node.setAttribute('srcset', node.dataset.srcset);
      // Image hosts police Referer in both directions and a browser cannot satisfy either policy
      // from inside the reader, so remote images are fetched by the server, which replays the
      // article's own address. See `mediaUrl`.
      const src = node.getAttribute('src');
      if (src) node.setAttribute('src', mediaUrl(src, mediaVia));
      const srcset = node.getAttribute('srcset');
      if (srcset) {
        node.setAttribute(
          'srcset',
          srcset
            .split(',')
            .map((part) => {
              const [url, ...rest] = part.trim().split(/\s+/);
              return [mediaUrl(url, mediaVia), ...rest].join(' ');
            })
            .join(', '),
        );
      }
    }
  });

  /** The article the body being sanitized belongs to; the hook above routes its images through it. */
  let mediaVia = '';

  const clean = (html: string, via: string): string => {
    mediaVia = via;
    return DOMPurify.sanitize(html, {
      USE_PROFILES: { html: true },
      ADD_ATTR: ['target', 'rel', 'controls', 'preload', 'sandbox', 'allowfullscreen'],
    });
  };

  /**
   * The prose, sanitized. Derived so it recomputes only when the article or the mode changes.
   *
   * The full article is preferred once asked for: it is what the reader asked to read, and the
   * feed's body stays one toggle away rather than being replaced.
   */
  const body = $derived(
    detail
      ? clean(
          showFull && detail.entry.full_content
            ? detail.entry.full_content
            : detail.entry.content_html,
          detail.entry.link,
        )
      : '',
  );

  /**
   * The list's copy of the open article, when the list has it.
   *
   * The fetched article and the list row are two copies of the same database row, and a mutation made
   * through the list (`s`, `m`, or clicking the star) updates the row. Without this the read bar's star
   * would keep showing the state the article was fetched with, disagreeing with the list beside it.
   */
  const row = $derived.by(() => {
    const current = detail;
    return current ? entries.items.find((e) => e.id === current.entry.id) : undefined;
  });
  const starred = $derived(row?.is_favorite ?? detail?.entry.is_favorite ?? false);
  const read = $derived(row?.is_read ?? detail?.entry.is_read ?? false);

  $effect(() => {
    void entryId;
    void load();
  });

  async function load(): Promise<void> {
    controller?.abort();
    controller = new AbortController();
    const mine = controller.signal;

    loading = true;
    error = null;
    // The mode belongs to the article being read, not to the pane: carrying it across would show
    // one article's scraped body request as though it applied to the next.
    showFull = false;
    fetchingFull = false;
    try {
      // The listing context travels with the request, so prev/next follow the order on screen rather
      // than the order in the database.
      detail = await api.entry(
        entryId,
        {
          scope: undefined,
          sort: view.sort,
          order: view.order,
          read: view.readFilter === 'unread' ? 'unread' : undefined,
          search: view.scope.kind === 'search' ? view.scope.query : undefined,
          id_max: undefined,
        },
        mine,
      );
      // Reading an article marks it read, the same as clicking it in the list. Doing it here too covers
      // arriving by a keyboard shortcut or a deep link.
      if (!detail.entry.is_read) void setRead([detail.entry.id], true);
      scrollEl?.scrollTo({ top: 0 });
    } catch (err) {
      if (mine.aborted) return;
      error = err instanceof Error ? err.message : 'Could not load the article';
      detail = null;
    } finally {
      if (!mine.aborted) loading = false;
    }
  }

  /**
   * Shows the article body scraped from its own page, fetching it the first time.
   *
   * The server stores what it scraped, so the second click on a later day costs nothing and another
   * device sees the same body; while it has none, this is the one button in the reader that waits on
   * a third-party page, hence its own busy state.
   */
  async function toggleFull(): Promise<void> {
    if (!detail) return;
    if (showFull) {
      showFull = false;
      return;
    }
    if (detail.entry.full_content) {
      showFull = true;
      return;
    }
    fetchingFull = true;
    try {
      const got = await api.fulltext(detail.entry.id);
      if (!detail) return;
      detail.entry.full_content = got.content_html;
      showFull = true;
    } catch (err) {
      notifyError(err instanceof Error ? err.message : 'Could not fetch the full article');
    } finally {
      fetchingFull = false;
    }
  }

  function toggleStar(): void {
    if (!detail) return;
    void setStarred([detail.entry.id], !starred);
  }

  function toggleUnread(): void {
    if (!detail) return;
    void setRead([detail.entry.id], !read);
  }

  async function share(): Promise<void> {
    if (!detail) return;
    const { title, link } = detail.entry;
    // The clipboard API needs a secure context; a plain `http://` deployment has to fall back.
    try {
      await navigator.clipboard.writeText(link ? `${title} — ${link}` : title);
      copied = true;
      setTimeout(() => (copied = false), 1500);
    } catch {
      notifyError('The browser would not let us copy the link');
    }
  }

  async function addLabel(): Promise<void> {
    if (!detail) return;
    const name = window.prompt('Label name');
    const trimmed = name?.trim();
    if (!trimmed || !detail) return;
    try {
      const labels = await api.addLabels(detail.entry.id, [trimmed]);
      detail.entry.labels = labels;
      setLabels(detail.entry.id, labels);
    } catch (err) {
      notifyError(err instanceof Error ? err.message : 'Could not add the label');
    }
  }

  async function removeLabel(name: string): Promise<void> {
    if (!detail) return;
    try {
      const labels = await api.removeLabel(detail.entry.id, name);
      detail.entry.labels = labels;
      setLabels(detail.entry.id, labels);
    } catch (err) {
      notifyError(err instanceof Error ? err.message : 'Could not remove the label');
    }
  }

  /**
   * The article before this one, preferring the list on screen.
   *
   * The server computes prev/next from the same filters as the listing — which means that under the
   * "unread" filter, opening an article marks it read and it drops out of the server's answer for its
   * neighbours. The reader still sees it in the list, so the list is the truth while it has the row;
   * the server's value is the fallback for arriving here by a deep link with no list at all.
   */
  function neighbour(direction: -1 | 1): string | null {
    const id = detail?.entry.id;
    if (!id) return null;
    const at = entries.items.findIndex((e) => e.id === id);
    if (at >= 0) {
      const next = entries.items[at + direction];
      if (next) return next.id;
      return null;
    }
    return direction < 0 ? detail?.prev_id ?? null : detail?.next_id ?? null;
  }

  const prevId = $derived(neighbour(-1));
  const nextId = $derived(neighbour(1));

  /** Exposed so the shortcuts can move between articles without the pane knowing about the list. */
  export function goPrev(): void {
    const id = prevId;
    if (id) onNavigate(id);
  }

  export function goNext(): void {
    const id = nextId;
    if (id) onNavigate(id);
  }

  export function openOriginal(): void {
    if (detail?.entry.link) window.open(detail.entry.link, '_blank', 'noopener,noreferrer');
  }

  /** The playable enclosure, if there is one, for `section.rd-player`. */
  const media = $derived(
    detail?.entry.enclosures.find((e) => {
      const type = (e.type ?? '').toLowerCase();
      return type.startsWith('audio/') || type.startsWith('video/');
    }) ?? null,
  );
  const mediaIsAudio = $derived((media?.type ?? '').toLowerCase().startsWith('audio/'));
</script>

<section class="rd-read" aria-label={detail?.entry.title ?? 'Article'}>
  <div class="rd-read-bar">
    <button class="rd-icon-btn rd-back" type="button" title="Back to list" onclick={onBack}>
      <i class="ri-arrow-left-line" aria-hidden="true"></i>
      <span class="rd-sr">Back to list</span>
    </button>
    <button
      class="rd-icon-btn"
      type="button"
      title="Previous (k)"
      disabled={!prevId}
      onclick={goPrev}
    >
      <i class="ri-arrow-up-s-line" aria-hidden="true"></i>
      <span class="rd-sr">Previous</span>
    </button>
    <button
      class="rd-icon-btn"
      type="button"
      title="Next (j)"
      disabled={!nextId}
      onclick={goNext}
    >
      <i class="ri-arrow-down-s-line" aria-hidden="true"></i>
      <span class="rd-sr">Next</span>
    </button>

    <span class="rd-read-bar-spacer"></span>

    <button
      class="rd-icon-btn"
      class:is-starred={starred}
      type="button"
      title="Star (s)"
      aria-pressed={starred}
      onclick={toggleStar}
    >
      <i class={starred ? 'ri-star-fill' : 'ri-star-line'} aria-hidden="true"></i>
      <span class="rd-sr">Star</span>
    </button>
    <button class="rd-icon-btn" type="button" title="Mark as unread (m)" onclick={toggleUnread}>
      <i class="ri-mail-unread-line" aria-hidden="true"></i>
      <span class="rd-sr">Mark as unread</span>
    </button>
    <button class="rd-icon-btn" type="button" title="Add a label" onclick={() => void addLabel()}>
      <i class="ri-price-tag-3-line" aria-hidden="true"></i>
      <span class="rd-sr">Add a label</span>
    </button>
    {#if detail?.entry.link}
      <a class="rd-icon-btn" href={detail.entry.link} target="_blank" rel="noopener noreferrer" title="Open original (v)">
        <i class="ri-external-link-line" aria-hidden="true"></i>
        <span class="rd-sr">Open original</span>
      </a>
    {/if}
    {#if detail?.entry.link}
      <button
        class="rd-icon-btn rd-fulltext-btn"
        type="button"
        title={showFull ? 'Show the feed’s version' : 'Show the full article'}
        aria-pressed={showFull}
        aria-busy={fetchingFull}
        disabled={fetchingFull}
        onclick={() => void toggleFull()}
      >
        <i class={fetchingFull ? 'ri-loader-4-line is-spinning' : showFull ? 'ri-file-text-line' : 'ri-article-line'}></i>
        <span class="rd-sr">{showFull ? 'Show the feed’s version' : 'Show the full article'}</span>
      </button>
    {/if}
    <button class="rd-icon-btn rd-share-btn" type="button" title="Share" onclick={() => void share()}>
      <i class={copied ? 'ri-check-line' : 'ri-share-line'} aria-hidden="true"></i>
      <span class="rd-sr rd-copy-label">{copied ? 'Copied' : 'Share'}</span>
    </button>
    <button class="rd-icon-btn rd-focus-btn" type="button" title="Focus mode (f)" onclick={toggleFocus}>
      <i class={prefs.focus ? 'ri-fullscreen-exit-line' : 'ri-fullscreen-line'} aria-hidden="true"></i>
      <span class="rd-sr">Focus mode</span>
    </button>
  </div>

  <div id="rd-read-scroll" class="rd-read-scroll" bind:this={scrollEl}>
    {#if error}
      <div class="rd-list-empty">
        <i class="ri-error-warning-line" aria-hidden="true"></i>
        <p>{error}</p>
        <button class="rd-btn" type="button" onclick={() => void load()}>Try again</button>
      </div>
    {:else if loading && !detail}
      <div class="rd-list-empty">
        <i class="ri-loader-4-line rd-spin" aria-hidden="true"></i>
        <p>Loading…</p>
      </div>
    {:else if detail}
      <article class="rd-article">
        <header class="rd-article-head">
          <a
            class="rd-article-feed"
            href={detail.feed.website || detail.entry.link || '#'}
            target="_blank"
            rel="noopener noreferrer"
          >
            {detail.feed.name}
          </a>
          <h1 class="rd-article-title">
            {#if detail.entry.link}
              <a href={detail.entry.link} target="_blank" rel="noopener noreferrer">{detail.entry.title}</a>
            {:else}
              {detail.entry.title}
            {/if}
          </h1>
          <p class="rd-article-byline">
            {#if detail.entry.author}
              <span class="rd-article-author">{detail.entry.author}</span>
            {/if}
            <time datetime={new Date(detail.entry.date * 1000).toISOString()}>
              {formatFullTime(detail.entry.date)}
            </time>
            {#if detail.entry.is_updated}
              <span class="rd-updated" title="The feed changed this article after we first saw it">updated</span>
            {/if}
          </p>
        </header>

        <!-- The one `{@html}` in the application. Sanitized above, and sanitized on ingest. -->
        <div class="rd-prose">
          {@html body}
        </div>

        {#if detail.entry.tags.length > 0 || detail.entry.labels.length > 0}
          <footer class="rd-article-foot">
            <div class="rd-article-tags">
              {#each detail.entry.tags as tag (tag)}
                <span class="rd-chip">#{tag}</span>
              {/each}
              {#each detail.entry.labels as label (label.id)}
                <button class="rd-chip rd-chip--label" type="button" title="Remove label" onclick={() => void removeLabel(label.name)}>
                  {label.name}
                  <i class="ri-close-line" aria-hidden="true"></i>
                </button>
              {/each}
            </div>
          </footer>
        {/if}
      </article>

      {#if media}
        <section class="rd-player" aria-label="Media">
          {#if mediaIsAudio}
            <audio controls preload="none" src={media.url}>
              <a href={media.url} rel="noopener noreferrer">Download the audio</a>
            </audio>
          {:else}
            <!-- The enclosure comes from the feed, which gives us a URL and a MIME type and nothing
                 else; there is no caption track to point at. -->
            <!-- svelte-ignore a11y_media_has_caption -->
            <video controls preload="none" src={media.url}>
              <a href={media.url} rel="noopener noreferrer">Download the video</a>
            </video>
          {/if}
        </section>
      {/if}
    {/if}
  </div>
</section>
