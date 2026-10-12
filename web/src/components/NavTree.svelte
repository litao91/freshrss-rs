<script lang="ts">
  import type { CategoryNode, FeedNode, LabelCount } from '../lib/api';
  import { formatCount } from '../lib/format';
  import { tree } from '../lib/session.svelte';
  import { isActive, selectScope, view, type Scope } from '../lib/view.svelte';

  interface Props {
    /** Opens `#rd-add-dialog`. */
    onAddSubscription: () => void;
    /** Opens `#rd-problems-dialog`. */
    onShowProblems: () => void;
    /** Opens `#rd-folder-dialog` for a folder, or creates one when `folder` is null. */
    onManageFolder: (folder: CategoryNode | null) => void;
    /** Opens `#rd-feed-dialog`. */
    onManageFeed: (feed: FeedNode) => void;
  }

  let { onAddSubscription, onShowProblems, onManageFolder, onManageFeed }: Props = $props();

  /** Collapsed folders, by id. Starting open matches the reference reader. */
  let collapsed = $state<Set<string>>(new Set());
  /** The eye toggle in `.rd-nav-head`: hides feeds whose unread count is zero. */
  let hideReadFeeds = $state(false);

  const counts = $derived(tree.data?.counts);
  const categories = $derived(tree.data?.categories ?? []);
  const orphans = $derived(tree.data?.orphans ?? []);
  const labels = $derived<LabelCount[]>(counts?.labels ?? []);
  const problems = $derived(tree.data?.problems.count ?? 0);

  // Scope builders rather than inline object literals: an untyped literal infers `kind: string`,
  // which does not satisfy the `Scope` discriminated union.
  const mainScope = (): Scope => ({ kind: 'main' });
  const starredScope = (): Scope => ({ kind: 'starred' });
  const categoryScope = (folder: CategoryNode): Scope => ({
    kind: 'category',
    id: folder.id,
    name: folder.name,
  });
  const feedScope = (feed: FeedNode): Scope => ({ kind: 'feed', id: feed.id, name: feed.name });
  const labelScope = (label: LabelCount): Scope => ({ kind: 'label', id: label.id, name: label.name });

  function scopeHref(scope: Scope): string {
    switch (scope.kind) {
      case 'main':
        return '/';
      case 'starred':
        return '/?view=starred';
      case 'category':
        return `/?folder=${scope.id}`;
      case 'feed':
        return `/?feed=${scope.id}`;
      case 'label':
        return `/?label=${scope.id}`;
      case 'search':
        return `/?search=${encodeURIComponent(scope.query)}`;
    }
  }

  /**
   * Navigates to a scope.
   *
   * Rendered as a real `<a href>` so middle-click, "open in new tab" and the status bar all behave
   * as a user expects of a link; the plain left-click is intercepted to keep it client-side.
   */
  function go(event: MouseEvent, scope: Scope): void {
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || event.button !== 0) return;
    event.preventDefault();
    selectScope(scope);
    window.history.pushState({}, '', scopeHref(scope));
  }

  function toggleFolder(id: string): void {
    const next = new Set(collapsed);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    collapsed = next;
  }

  const allCollapsed = $derived(categories.length > 0 && collapsed.size >= categories.length);

  function toggleAll(): void {
    collapsed = allCollapsed ? new Set() : new Set(categories.map((c) => c.id));
  }

  function visibleFeeds(folder: CategoryNode): FeedNode[] {
    return hideReadFeeds ? folder.feeds.filter((f) => f.unread > 0) : folder.feeds;
  }
</script>

<nav id="rd-nav" class="rd-nav" aria-label="Subscriptions">
  <div class="rd-nav-inner">
    <button class="rd-btn rd-btn--primary rd-add" type="button" onclick={onAddSubscription}>
      <i class="ri-add-line" aria-hidden="true"></i>
      Add subscription
    </button>

    <ul class="rd-tree">
      <li>
        <a
          class="rd-tree-link"
          class:is-active={isActive(view.scope, mainScope())}
          href={scopeHref(mainScope())}
          onclick={(e) => go(e, mainScope())}
        >
          <i class="ri-inbox-2-line rd-tree-icon" aria-hidden="true"></i>
          <span class="rd-tree-label">All articles</span>
          {#if counts && counts.main_stream.unread > 0}
            <span class="rd-count">
              <span>{formatCount(counts.main_stream.unread)}</span>
              <span class="rd-sr">{counts.main_stream.unread} unread</span>
            </span>
          {/if}
        </a>
      </li>
      <li>
        <a
          class="rd-tree-link"
          class:is-active={isActive(view.scope, starredScope())}
          href={scopeHref(starredScope())}
          onclick={(e) => go(e, starredScope())}
        >
          <i class="ri-star-line rd-tree-icon" aria-hidden="true"></i>
          <span class="rd-tree-label">Starred</span>
          {#if counts && counts.starred.total > 0}
            <span class="rd-count">
              <span>{formatCount(counts.starred.total)}</span>
              <span class="rd-sr">{counts.starred.total} starred</span>
            </span>
          {/if}
        </a>
      </li>
    </ul>

    {#if problems > 0}
      <button class="rd-btn rd-btn--quiet rd-btn--danger" type="button" onclick={onShowProblems}>
        <i class="ri-error-warning-line" aria-hidden="true"></i>
        {problems} {problems === 1 ? 'feed has' : 'feeds have'} problems
      </button>
    {/if}

    <div class="rd-nav-head">
      <h2 class="rd-nav-heading">Subscriptions</h2>
      <button
        class="rd-icon-btn"
        type="button"
        title={allCollapsed ? 'Expand all folders' : 'Collapse all folders'}
        onclick={toggleAll}
      >
        <i class="ri-contract-up-down-line" aria-hidden="true"></i>
        <span class="rd-sr">{allCollapsed ? 'Expand all folders' : 'Collapse all folders'}</span>
      </button>
      <button
        class="rd-icon-btn"
        type="button"
        aria-pressed={hideReadFeeds}
        title={hideReadFeeds ? 'Show feeds with no unread articles' : 'Hide feeds with no unread articles'}
        onclick={() => (hideReadFeeds = !hideReadFeeds)}
      >
        <i class={hideReadFeeds ? 'ri-eye-off-line' : 'ri-eye-line'} aria-hidden="true"></i>
        <span class="rd-sr">
          {hideReadFeeds ? 'Show feeds with no unread articles' : 'Hide feeds with no unread articles'}
        </span>
      </button>
      <button class="rd-icon-btn" type="button" title="New folder" onclick={() => onManageFolder(null)}>
        <i class="ri-folder-add-line" aria-hidden="true"></i>
        <span class="rd-sr">New folder</span>
      </button>
    </div>

    {#if categories.length === 0 && orphans.length === 0}
      <p class="rd-nav-empty">No subscriptions yet. Add one to get started.</p>
    {/if}

    <ul class="rd-tree">
      {#each categories as folder (folder.id)}
        {@const open = !collapsed.has(folder.id)}
        {@const feeds = visibleFeeds(folder)}
        {@const scope = categoryScope(folder)}
        <li class="rd-folder" class:is-open={open}>
          <div class="rd-folder-row">
            <button
              class="rd-folder-toggle"
              type="button"
              aria-expanded={open}
              onclick={() => toggleFolder(folder.id)}
            >
              <i class="ri-arrow-right-s-line" aria-hidden="true"></i>
              <span class="rd-sr">Show or hide the {folder.name} folder</span>
            </button>
            <a
              class="rd-tree-link"
              class:is-active={isActive(view.scope, scope)}
              class:is-error={folder.error > 0}
              href={scopeHref(scope)}
              onclick={(e) => go(e, scope)}
            >
              <i
                class={open ? 'ri-folder-open-line rd-tree-icon' : 'ri-folder-line rd-tree-icon'}
                aria-hidden="true"
              ></i>
              <span class="rd-tree-label">{folder.name}</span>
              {#if folder.unread > 0}
                <span class="rd-count">
                  <span>{formatCount(folder.unread)}</span>
                  <span class="rd-sr">{folder.unread} unread</span>
                </span>
              {/if}
            </a>
            <button
              class="rd-folder-toggle"
              type="button"
              title="Folder settings"
              onclick={() => onManageFolder(folder)}
            >
              <i class="ri-more-2-fill" aria-hidden="true"></i>
              <span class="rd-sr">Settings for the {folder.name} folder</span>
            </button>
          </div>

          {#if open && feeds.length > 0}
            <ul class="rd-tree rd-tree--nested">
              {#each feeds as feed (feed.id)}
                {@const fScope = feedScope(feed)}
                <li>
                  <a
                    class="rd-tree-link"
                    class:is-active={isActive(view.scope, fScope)}
                    class:is-unread={feed.unread > 0}
                    class:is-error={feed.error > 0 || feed.muted}
                    href={scopeHref(fScope)}
                    title={feed.muted ? `${feed.name} (muted)` : feed.name}
                    onclick={(e) => go(e, fScope)}
                    oncontextmenu={(e) => {
                      e.preventDefault();
                      onManageFeed(feed);
                    }}
                  >
                    {#if feed.favicon.url}
                      <img class="rd-favicon" src={feed.favicon.url} alt="" loading="lazy" />
                    {:else}
                      <span
                        class="rd-favicon rd-favicon--letter"
                        style:background={feed.favicon.color}
                        aria-hidden="true"
                      >
                        {feed.favicon.letter}
                      </span>
                    {/if}
                    <span class="rd-tree-label">{feed.name}</span>
                    {#if feed.unread > 0}
                      <span class="rd-count">
                        <span>{formatCount(feed.unread)}</span>
                        <span class="rd-sr">{feed.unread} unread</span>
                      </span>
                    {/if}
                  </a>
                </li>
              {/each}
            </ul>
          {/if}
        </li>
      {/each}

      {#each orphans as feed (feed.id)}
        {@const oScope = feedScope(feed)}
        <li>
          <a
            class="rd-tree-link"
            class:is-active={isActive(view.scope, oScope)}
            class:is-unread={feed.unread > 0}
            href={scopeHref(oScope)}
            title="Not in a folder"
            onclick={(e) => go(e, oScope)}
            oncontextmenu={(e) => {
              e.preventDefault();
              onManageFeed(feed);
            }}
          >
            {#if feed.favicon.url}
              <img class="rd-favicon" src={feed.favicon.url} alt="" loading="lazy" />
            {:else}
              <span
                class="rd-favicon rd-favicon--letter"
                style:background={feed.favicon.color}
                aria-hidden="true"
              >
                {feed.favicon.letter}
              </span>
            {/if}
            <span class="rd-tree-label">{feed.name}</span>
            {#if feed.unread > 0}
              <span class="rd-count">
                <span>{formatCount(feed.unread)}</span>
                <span class="rd-sr">{feed.unread} unread</span>
              </span>
            {/if}
          </a>
        </li>
      {/each}
    </ul>

    {#if labels.length > 0}
      <div class="rd-nav-head">
        <h2 class="rd-nav-heading">Labels</h2>
      </div>
      <ul class="rd-tree">
        {#each labels as label (label.id)}
          {@const scope = labelScope(label)}
          <li>
            <a
              class="rd-tree-link"
              class:is-active={isActive(view.scope, scope)}
              href={scopeHref(scope)}
              onclick={(e) => go(e, scope)}
            >
              <i class="ri-price-tag-3-line rd-tree-icon" aria-hidden="true"></i>
              <span class="rd-tree-label">{label.name}</span>
              {#if label.unread > 0}
                <span class="rd-count">
                  <span>{formatCount(label.unread)}</span>
                  <span class="rd-sr">{label.unread} unread</span>
                </span>
              {/if}
            </a>
          </li>
        {/each}
      </ul>
    {/if}
  </div>
</nav>
