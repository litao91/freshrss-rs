<script lang="ts">
  /**
   * `#rd-feed-dialog`: per-feed settings, opened from the sidebar's feed rows (context menu).
   *
   * Two contracts shape this form:
   *
   * - `PUT /feeds/{id}` is field-wise: an absent field is left alone, and for the scraping paths
   *   and the HTTP credentials the *stored* values are never sent back (`FeedDto` omits them, and
   *   the URL arrives without embedded credentials). So those inputs start empty and are only
   *   sent when the reader types something — empty means "keep", not "clear". The hints say so.
   * - `PUT /feeds/{id}/filters` replaces the rule list wholesale, so the rules are loaded first
   *   and only sent back when that load succeeded and the reader actually changed a row; saving
   *   an empty list after a failed `GET` would silently delete every rule.
   */
  import { api, ApiError, type FeedNode, type FilterActionName } from '../../lib/api';
  import { t, tf } from '../../lib/i18n';
  import { tree } from '../../lib/session.svelte';
  import { notify } from '../../lib/toast.svelte';

  interface Props {
    /** `undefined` closes the dialog; a feed node opens it for that feed. */
    feed: FeedNode | undefined;
    onclose: () => void;
    /** Called after a save so the sidebar and the list can pick up the changes. */
    onChanged: () => Promise<void> | void;
  }

  let { feed, onclose, onChanged }: Props = $props();

  /** One editable filter row; the booleans become the `actions` array on save. */
  interface RuleRow {
    id: number;
    search: string;
    read: boolean;
    star: boolean;
    label: boolean;
  }

  let dialogEl: HTMLDialogElement | undefined = $state();
  let loading = $state(false);
  let loadError = $state<string | null>(null);
  let busy = $state(false);
  let formError = $state<string | null>(null);

  let name = $state('');
  let url = $state('');
  /** `''` means "no folder" and becomes `clear_category`; ids travel as JSON strings. */
  let folder = $state('');
  let priority = $state(10);
  let ttl = $state(0);
  let muted = $state(false);
  let kind = $state(0);
  let pathEntries = $state('');
  let pathEntriesFilter = $state('');
  let authUser = $state('');
  let authPass = $state('');
  /** Set by the explicit "clear credentials" button, because an empty form field means "keep". */
  let clearAuth = $state(false);
  let rules = $state<RuleRow[]>([]);
  /** False when the rules could not be loaded; saving must then not PUT them. */
  let filtersLoaded = $state(false);
  /** The loaded rules as JSON, so an untouched list is not sent back. */
  let filtersOriginal = $state('');

  let nextRuleId = 0;

  const categories = $derived(tree.data?.categories ?? []);

  /** The named priority levels; anything else renders as a "Custom (n)" option. */
  const PRIORITIES: { value: number; key: string }[] = [
    { value: 20, key: 'feed.priorityImportant' },
    { value: 10, key: 'feed.priorityMain' },
    { value: 0, key: 'feed.priorityCategory' },
    { value: -5, key: 'feed.priorityFeed' },
    { value: -10, key: 'feed.priorityHidden' },
  ];

  /** `FeedKind`'s on-disk values; the numbers are part of the storage format and OPML output. */
  const KINDS: { value: number; key: string }[] = [
    { value: 0, key: 'feed.kind0' },
    { value: 2, key: 'feed.kind2' },
    { value: 10, key: 'feed.kind10' },
    { value: 15, key: 'feed.kind15' },
    { value: 20, key: 'feed.kind20' },
    { value: 25, key: 'feed.kind25' },
    { value: 30, key: 'feed.kind30' },
    { value: 35, key: 'feed.kind35' },
  ];

  const priorityOptions = $derived.by(() =>
    PRIORITIES.some((p) => p.value === priority)
      ? PRIORITIES
      : [...PRIORITIES, { value: priority, key: '' }],
  );

  $effect(() => {
    if (!dialogEl) return;
    if (feed === undefined) {
      if (dialogEl.open) dialogEl.close();
    } else {
      // Seed from the tree node immediately so the form is never momentarily empty, then refresh
      // from `GET /feeds/{id}` in case the tree has gone stale.
      seed(feed);
      formError = null;
      if (!dialogEl.open) dialogEl.showModal();
      void load(feed.id);
    }
  });

  function seed(source: FeedNode): void {
    name = source.name;
    url = source.url;
    folder = source.category === null ? '' : String(source.category);
    priority = source.priority;
    ttl = source.ttl;
    muted = source.muted;
    kind = source.kind;
    // The API never sends these back; empty means "keep whatever is stored".
    pathEntries = '';
    pathEntriesFilter = '';
    authUser = '';
    authPass = '';
    clearAuth = false;
    rules = [];
    filtersLoaded = false;
    filtersOriginal = '';
    loadError = null;
  }

  async function load(id: string): Promise<void> {
    loading = true;
    try {
      const [fresh, filters] = await Promise.all([api.feed(id), api.feedFilters(id)]);
      // The dialog may have been closed or moved to another feed while these were in flight.
      if (feed?.id !== id) return;
      seed(fresh);
      rules = filters.filters.map((rule) => ({
        id: nextRuleId++,
        search: rule.search,
        read: rule.actions.includes('read'),
        star: rule.actions.includes('star'),
        label: rule.actions.includes('label'),
      }));
      filtersLoaded = true;
      filtersOriginal = rulesJson();
    } catch (err) {
      if (feed?.id !== id) return;
      // Editing the basic fields still works from the tree node; only the rules are unavailable.
      loadError = err instanceof ApiError ? err.message : t('feed.loadError');
    } finally {
      if (feed?.id === id) loading = false;
    }
  }

  function addRule(): void {
    rules = [...rules, { id: nextRuleId++, search: '', read: false, star: false, label: false }];
  }

  function removeRule(id: number): void {
    rules = rules.filter((rule) => rule.id !== id);
  }

  /** The rows as the wire format, dropping rows the reader left completely empty. */
  function buildRules(): { search: string; actions: FilterActionName[] }[] | null {
    const out: { search: string; actions: FilterActionName[] }[] = [];
    for (const row of rules) {
      const actions: FilterActionName[] = [];
      if (row.read) actions.push('read');
      if (row.star) actions.push('star');
      if (row.label) actions.push('label');
      const search = row.search.trim();
      if (!search && actions.length === 0) continue;
      if (!search || actions.length === 0) {
        formError = t('feed.ruleIncomplete');
        return null;
      }
      out.push({ search, actions });
    }
    return out;
  }

  function rulesJson(): string {
    return JSON.stringify(rules.map((r) => [r.search.trim(), r.read, r.star, r.label]));
  }

  async function submit(event: Event): Promise<void> {
    event.preventDefault();
    if (!feed || busy) return;
    formError = null;

    const body: Parameters<typeof api.updateFeed>[1] = {
      priority,
      ttl: Number.isFinite(ttl) ? Math.max(0, ttl) : 0,
      muted,
      kind,
    };
    const trimmedName = name.trim();
    if (trimmedName) body.name = trimmedName;
    const trimmedUrl = url.trim();
    if (trimmedUrl) body.url = trimmedUrl;
    if (folder === '') body.clear_category = true;
    else body.category_id = folder;
    // Empty scraping fields mean "keep the stored value", because the API never reveals it.
    if (pathEntries.trim()) body.path_entries = pathEntries.trim();
    if (pathEntriesFilter.trim()) body.path_entries_filter = pathEntriesFilter.trim();
    // Typed credentials win over the clear flag, so filling the fields back in undoes it.
    if (authUser.trim()) body.http_auth = { username: authUser.trim(), password: authPass };
    else if (clearAuth) body.http_auth = { username: '', password: '' };

    const sendFilters = filtersLoaded && rulesJson() !== filtersOriginal;
    let nextFilters: { search: string; actions: FilterActionName[] }[] | null = null;
    if (sendFilters) {
      nextFilters = buildRules();
      if (nextFilters === null) return; // buildRules set formError
    }

    busy = true;
    try {
      await api.updateFeed(feed.id, body);
      if (sendFilters && nextFilters) await api.setFeedFilters(feed.id, nextFilters);
      notify(t('common.saved'));
      await onChanged();
      onclose();
    } catch (err) {
      formError = err instanceof ApiError ? err.message : t('feed.saveError');
    } finally {
      busy = false;
    }
  }

  function handleCancel(): void {
    onclose();
  }
</script>

<dialog
  id="rd-feed-dialog"
  class="rd-dialog rd-dialog--wide"
  bind:this={dialogEl}
  aria-labelledby="rd-feed-title"
  oncancel={(e) => {
    e.preventDefault();
    handleCancel();
  }}
>
  <form class="rd-dialog-body" method="dialog" onsubmit={(e) => void submit(e)}>
    <h2 id="rd-feed-title">{t('feed.title')}{feed ? `: ${feed.name}` : ''}</h2>

    {#if loadError}
      <p class="rd-field-error" role="alert">{loadError}</p>
    {/if}

    <div class="rd-field-grid">
      <div class="rd-field-row">
        <label class="rd-label" for="rd-feed-name">{t('feed.name')}</label>
        <input id="rd-feed-name" class="rd-input" type="text" autocomplete="off" maxlength="191" bind:value={name} />
      </div>
      <div class="rd-field-row">
        <label class="rd-label" for="rd-feed-folder">{t('feed.folder')}</label>
        <select id="rd-feed-folder" class="rd-input" bind:value={folder}>
          <option value="">{t('feed.noFolder')}</option>
          {#each categories as category (category.id)}
            <option value={category.id}>{category.name}</option>
          {/each}
        </select>
      </div>
    </div>

    <label class="rd-label" for="rd-feed-url">{t('feed.url')}</label>
    <input id="rd-feed-url" class="rd-input" type="url" autocomplete="off" spellcheck="false" bind:value={url} />

    <div class="rd-field-grid">
      <div class="rd-field-row">
        <label class="rd-label" for="rd-feed-priority">{t('feed.priority')}</label>
        <select id="rd-feed-priority" class="rd-input" bind:value={priority}>
          {#each priorityOptions as option (option.value)}
            <option value={option.value}>
              {option.key ? t(option.key) : tf('feed.priorityCustom', { n: option.value })}
            </option>
          {/each}
        </select>
      </div>
      <div class="rd-field-row">
        <label class="rd-label" for="rd-feed-kind">{t('feed.kind')}</label>
        <select id="rd-feed-kind" class="rd-input" bind:value={kind}>
          {#each KINDS as option (option.value)}
            <option value={option.value}>{t(option.key)}</option>
          {/each}
        </select>
      </div>
      <div class="rd-field-row">
        <label class="rd-label" for="rd-feed-ttl">{t('feed.ttl')}</label>
        <input id="rd-feed-ttl" class="rd-input" type="number" min="0" step="60" bind:value={ttl} />
        <span class="rd-hint">{t('feed.ttlHint')}</span>
      </div>
      <div class="rd-field-row rd-field-row--bottom">
        <label class="rd-check">
          <input type="checkbox" bind:checked={muted} />
          {t('feed.muted')}
        </label>
      </div>
    </div>

    <p class="rd-menu-label">{t('feed.scraping')}</p>
    <div class="rd-field-grid">
      <div class="rd-field-row">
        <label class="rd-label" for="rd-feed-path-entries">{t('feed.pathEntries')}</label>
        <input
          id="rd-feed-path-entries"
          class="rd-input"
          type="text"
          autocomplete="off"
          spellcheck="false"
          bind:value={pathEntries}
        />
      </div>
      <div class="rd-field-row">
        <label class="rd-label" for="rd-feed-path-filter">{t('feed.pathEntriesFilter')}</label>
        <input
          id="rd-feed-path-filter"
          class="rd-input"
          type="text"
          autocomplete="off"
          spellcheck="false"
          bind:value={pathEntriesFilter}
        />
      </div>
    </div>
    <p class="rd-hint">{t('feed.scrapeHint')}</p>

    <p class="rd-menu-label">{t('feed.httpAuth')}</p>
    <div class="rd-field-grid">
      <div class="rd-field-row">
        <label class="rd-label" for="rd-feed-auth-user">{t('feed.username')}</label>
        <input id="rd-feed-auth-user" class="rd-input" type="text" autocomplete="off" bind:value={authUser} />
      </div>
      <div class="rd-field-row">
        <label class="rd-label" for="rd-feed-auth-pass">{t('feed.password')}</label>
        <input id="rd-feed-auth-pass" class="rd-input" type="password" autocomplete="new-password" bind:value={authPass} />
      </div>
    </div>
    <div class="rd-field-row">
      <p class="rd-hint">{t('feed.authHint')}</p>
      <button
        class="rd-btn rd-btn--quiet rd-btn--danger rd-self-start"
        type="button"
        aria-pressed={clearAuth}
        disabled={busy}
        onclick={() => {
          clearAuth = !clearAuth;
          if (clearAuth) {
            authUser = '';
            authPass = '';
          }
        }}
      >
        <i class="ri-delete-bin-line" aria-hidden="true"></i>
        {t('feed.clearAuth')}
      </button>
    </div>

    <p class="rd-menu-label">{t('feed.filters')}</p>
    <p class="rd-hint">{t('feed.filtersHint')}</p>
    {#each rules as rule (rule.id)}
      <div class="rd-filter-rule">
        <label class="rd-sr" for="rd-feed-rule-{rule.id}">{t('feed.filters')}</label>
        <input
          id="rd-feed-rule-{rule.id}"
          class="rd-input"
          type="text"
          autocomplete="off"
          spellcheck="false"
          placeholder={t('feed.ruleSearch')}
          bind:value={rule.search}
        />
        <div class="rd-filter-actions">
          <label class="rd-check">
            <input type="checkbox" bind:checked={rule.read} />
            {t('feed.actionRead')}
          </label>
          <label class="rd-check">
            <input type="checkbox" bind:checked={rule.star} />
            {t('feed.actionStar')}
          </label>
          <label class="rd-check">
            <input type="checkbox" bind:checked={rule.label} />
            {t('feed.actionLabel')}
          </label>
          <button
            class="rd-icon-btn"
            type="button"
            title={t('feed.removeRule')}
            onclick={() => removeRule(rule.id)}
          >
            <i class="ri-close-line" aria-hidden="true"></i>
            <span class="rd-sr">{t('feed.removeRule')}</span>
          </button>
        </div>
      </div>
    {/each}
    <button class="rd-btn rd-btn--quiet rd-self-start" type="button" disabled={loading} onclick={addRule}>
      <i class="ri-add-line" aria-hidden="true"></i>
      {t('feed.addRule')}
    </button>

    {#if formError}
      <p class="rd-field-error" role="alert">{formError}</p>
    {/if}

    <div class="rd-dialog-actions">
      <button class="rd-btn rd-btn--quiet" type="button" disabled={busy} onclick={handleCancel}>
        {t('common.cancel')}
      </button>
      <button class="rd-btn rd-btn--primary" type="submit" disabled={busy || loading}>
        {busy ? t('common.saving') : t('common.save')}
      </button>
    </div>
  </form>
</dialog>
