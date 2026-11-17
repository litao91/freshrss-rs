<script lang="ts">
  import { ButtonGroup, Popover } from 'svelte-widgets';
  import ApiDialog from './dialogs/ApiDialog.svelte';
  import { LANGUAGES, t, tf } from '../lib/i18n';
  import {
    MAX_SIZE,
    MIN_SIZE,
    nudgeSize,
    prefs,
    setDensity,
    setLanguage,
    setTheme,
    isNarrow,
    toggleDrawer,
    toggleFocus,
    toggleNav,
    toggleNavForViewport,
    type Density,
    type ThemeName,
    type ThemePref,
  } from '../lib/prefs.svelte';
  import { session, signOut } from '../lib/session.svelte';
  import { refreshJob } from '../lib/refresh.svelte';

  interface Props {
    /** Current contents of `#rd-search`, bound so `/` can focus and fill it. */
    search: string;
    onSearch: (query: string) => void;
    onRefresh: () => void;
    onShowShortcuts: () => void;
    /** True while a refresh job is running; spins the icon. */
    refreshing: boolean;
  }

  let { search = $bindable(''), onSearch, onRefresh, onShowShortcuts, refreshing }: Props = $props();

  let searchEl: HTMLInputElement | undefined = $state();
  let searchFocused = $state(false);
  let displayOpen = $state(false);
  let accountOpen = $state(false);
  let apiDialogOpen = $state(false);
  let syntaxOpen = $state(false);
  let recent = $state<string[]>(readRecent());

  // Derived rather than plain constants: the labels go through `t()`, so they have to re-render
  // when the language changes.
  const themeOptions = $derived.by<{ value: ThemePref; label: string }[]>(() => [
    { value: 'kagit', label: t('theme.kagit') },
    { value: 'sepya', label: t('theme.sepya') },
    { value: 'gece', label: t('theme.gece') },
    { value: 'auto', label: t('theme.auto') },
  ]);

  const densities = $derived.by<{ value: Density; label: string }[]>(() => [
    { value: 'rahat', label: t('topbar.comfortable') },
    { value: 'sikisik', label: t('topbar.compact') },
  ]);

  /** Exposed so the `/` shortcut can focus the box. */
  export function focusSearch(): void {
    searchEl?.focus();
    searchEl?.select();
  }

  /**
   * Closes both panels.
   *
   * Everything the old `<details>` pair needed a document-level listener and an Escape handler for
   * — press-away dismissal, Escape, focus returning to the trigger — is `Popover`'s own now, and
   * its escape layer stops the key before App's global handler can also close the open article.
   * What remains here is the one thing no library can know: these two panels are one control
   * surface and are never both open.
   */
  function closeMenus(): void {
    displayOpen = false;
    accountOpen = false;
  }

  /**
   * Opening one panel closes the other.
   *
   * A press away already does this, because the press lands outside the open panel before the
   * other trigger's click arrives; the keyboard path presses nothing, so it needs saying.
   */
  function openOnly(which: 'display' | 'account'): void {
    if (which === 'display') accountOpen = false;
    else displayOpen = false;
  }

  function submitSearch(event: Event): void {
    event.preventDefault();
    const query = search.trim();
    rememberRecent(query);
    onSearch(query);
    closeMenus();
  }

  /** The clear button empties the box and returns to the main stream, then keeps focus in the field. */
  function clearSearch(): void {
    search = '';
    onSearch('');
    searchEl?.focus();
  }

  // Recent queries, kept locally so the box offers them back on focus.
  function readRecent(): string[] {
    try {
      const raw = localStorage.getItem('feeddi-reader-searches');
      const parsed: unknown = raw ? JSON.parse(raw) : [];
      return Array.isArray(parsed)
        ? parsed.filter((x): x is string => typeof x === 'string').slice(0, 6)
        : [];
    } catch {
      return [];
    }
  }

  function rememberRecent(query: string): void {
    if (!query) return;
    recent = [query, ...recent.filter((q) => q !== query)].slice(0, 6);
    try {
      localStorage.setItem('feeddi-reader-searches', JSON.stringify(recent));
    } catch {
      // History is a convenience, never a requirement.
    }
  }

  function pickRecent(query: string): void {
    search = query;
    onSearch(query);
    searchFocused = false;
    closeMenus();
  }

  function clearRecent(): void {
    recent = [];
    try {
      localStorage.removeItem('feeddi-reader-searches');
    } catch {
      // ignore
    }
  }

  async function handleSignOut(): Promise<void> {
    closeMenus();
    await signOut();
  }
</script>

<header class="rd-top">
  <button
    class="rd-icon-btn rd-nav-toggle"
    type="button"
    aria-expanded={prefs.drawerOpen}
    aria-controls="rd-nav"
    title={t('topbar.menu')}
    onclick={() => toggleDrawer()}
  >
    <i class="ri-menu-line" aria-hidden="true"></i>
    <span class="rd-sr">{t('topbar.menu')}</span>
  </button>

  <button
    class="rd-icon-btn rd-nav-dock"
    type="button"
    data-rd-nav-dock
    aria-expanded={prefs.navOpen}
    aria-controls="rd-nav"
    title="{t('topbar.subscriptionsPanel')} (b)"
    onclick={() => toggleNav()}
  >
    <i class="ri-sidebar-fold-line" aria-hidden="true"></i>
    <i class="ri-sidebar-unfold-line" aria-hidden="true"></i>
    <span class="rd-sr">{t('topbar.subscriptionsPanel')}</span>
  </button>

  <a class="rd-brand" href="/">
    <svg class="brand-mark" viewBox="8 4 48 55" aria-hidden="true" focusable="false">
      <path class="brand-mark-ink" d="M10.5 22h10v20h-10z"></path>
      <path
        class="brand-mark-accent"
        d="M12.5 12H28.5C38.5 12 46.5 11 53.5 6C49.5 18 39.5 23 26.5 23H10.5V14A2 2 0 0 1 12.5 12Z"
      ></path>
      <path class="brand-mark-ink" d="M10.5 27H24.5C32.5 27 37.5 26 42.5 22C39.5 32 32.5 37 22.5 37H10.5Z"></path>
      <path class="brand-mark-accent" d="M10.5 40H20.5V46C20.5 52.5 16 56.5 10.5 57Z"></path>
    </svg>
    <span class="rd-brand-name">freshrss-rs</span>
  </a>

  <form class="rd-search" role="search" onsubmit={submitSearch}>
    <label class="rd-sr" for="rd-search">{t('topbar.search')}</label>
    <i class="ri-search-line" aria-hidden="true"></i>
    <input
      id="rd-search"
      type="search"
      placeholder={t('topbar.search')}
      autocomplete="off"
      spellcheck="false"
      bind:this={searchEl}
      bind:value={search}
      onfocus={() => (searchFocused = true)}
      onblur={() => (searchFocused = false)}
    />
    {#if search}
      <button class="rd-search-clear" type="button" title={t('topbar.clearSearch')} onclick={clearSearch}>
        <i class="ri-close-line" aria-hidden="true"></i>
        <span class="rd-sr">{t('topbar.clearSearch')}</span>
      </button>
    {:else if !searchFocused}
      <kbd class="rd-search-hint" aria-hidden="true">/</kbd>
    {/if}
    {#if searchFocused && !search.trim() && recent.length > 0}
      <div class="rd-search-suggest">
        <p class="rd-menu-label">{t('search.recent')}</p>
        {#each recent as q (q)}
          <button
            class="rd-menu-item"
            type="button"
            onmousedown={(e) => e.preventDefault()}
            onclick={() => pickRecent(q)}
          >
            <i class="ri-history-line" aria-hidden="true"></i>
            {q}
          </button>
        {/each}
        <button
          class="rd-search-suggest-clear"
          type="button"
          onmousedown={(e) => e.preventDefault()}
          onclick={clearRecent}
        >
          {t('search.clearRecent')}
        </button>
      </div>
    {/if}
  </form>

  <Popover
    bind:open={syntaxOpen}
    placement="bottom"
    align="start"
    dismiss_on="press"
    class="rd-search-syntax"
  >
    {#snippet trigger(props)}
      <button class="rd-icon-btn rd-search-help" type="button" title={t('search.syntax')} {...props}>
        <i class="ri-question-line" aria-hidden="true"></i>
        <span class="rd-sr">{t('search.syntax')}</span>
      </button>
    {/snippet}
    <p class="rd-menu-label">{t('search.syntax')}</p>
    <dl class="rd-syntax">
      <dt><code>intitle:</code></dt>
      <dd>{t('search.op.intitle')}</dd>
      <dt><code>intext:</code></dt>
      <dd>{t('search.op.intext')}</dd>
      <dt><code>author:</code></dt>
      <dd>{t('search.op.author')}</dd>
      <dt><code>inurl:</code></dt>
      <dd>{t('search.op.inurl')}</dd>
      <dt><code>#label</code></dt>
      <dd>{t('search.op.label')}</dd>
      <dt><code>pubdate:</code></dt>
      <dd>{t('search.op.date')}</dd>
      <dt><code>f:id</code></dt>
      <dd>{t('search.op.feed')}</dd>
      <dt><code>OR - ( ) " "</code></dt>
      <dd>{t('search.op.bool')}</dd>
    </dl>
  </Popover>

  <div class="rd-top-actions">
    {#if refreshJob.running}
      <span class="rd-refresh-chip" role="status" aria-live="polite">
        <i class="ri-loader-4-line rd-spin" aria-hidden="true"></i>
        {tf('topbar.refreshProgress', { done: refreshJob.feedsDone, new: refreshJob.newEntries })}
      </span>
    {/if}
    <button
      class="rd-icon-btn"
      type="button"
      title="{t('topbar.refresh')} (r)"
      disabled={refreshing}
      onclick={() => {
        onRefresh();
        closeMenus();
      }}
    >
      <i class="ri-refresh-line" class:is-spinning={refreshing} aria-hidden="true"></i>
      <span class="rd-sr">{t('topbar.refresh')}</span>
    </button>

    <!-- `dismiss_on: 'press'` for the same reason as the list's menus: it moves dismissal off the
         browser's light-dismiss, whose Escape App's global handler would cancel on its way to
         closing the open article. The panel's own surface is the popover element, so its padding,
         border and shadow come from the `--popover-*` tokens rather than from `.rd-menu-panel`,
         whose absolute positioning would fight the `float` attachment's inline coordinates.

         `Popover` aligns `center` or `start` only, so a wide panel anchored near the right edge
         hangs off it and is shifted back inside the viewport rather than hanging right-aligned
         the way the old `.rd-menu--end` did. -->
    <Popover
      bind:open={displayOpen}
      placement="bottom"
      align="start"
      dismiss_on="press"
      class="rd-display"
    >
      {#snippet trigger(props)}
        <button
          class="rd-icon-btn"
          type="button"
          title={t('topbar.display')}
          {...props}
          onclick={(event) => {
            props.onclick?.(event);
            openOnly('display');
          }}
        >
          <i class="ri-font-size-2" aria-hidden="true"></i>
          <span class="rd-sr">{t('topbar.display')}</span>
        </button>
      {/snippet}

      <p class="rd-menu-label" id="rd-theme-label">{t('topbar.appearance')}</p>
      <div class="rd-themes" role="group" aria-labelledby="rd-theme-label">
        {#each themeOptions as option (option.value)}
          <button
            class="rd-theme-opt"
            type="button"
            data-feeddi-theme={option.value}
            aria-pressed={prefs.themePref === option.value}
            onclick={() => setTheme(option.value)}
          >
            <span class="rd-swatch" aria-hidden="true">
              {#if option.value === 'auto'}
                <span class="rd-swatch-face" data-theme-preview="kagit"></span>
                <span class="rd-swatch-face" data-theme-preview="gece"></span>
              {:else}
                <span class="rd-swatch-face" data-theme-preview={option.value as ThemeName}></span>
              {/if}
            </span>
            {option.label}
          </button>
        {/each}
      </div>

      <p class="rd-menu-label" id="rd-size-label">{t('topbar.textSize')}</p>
      <div class="rd-stepper" role="group" aria-labelledby="rd-size-label">
        <button
          class="rd-icon-btn"
          type="button"
          title="{t('topbar.smallerText')} (-)"
          disabled={prefs.size <= MIN_SIZE}
          onclick={() => nudgeSize(-1)}
        >
          <i class="ri-subtract-line" aria-hidden="true"></i>
          <span class="rd-sr">{t('topbar.smallerText')}</span>
        </button>
        <output class="rd-stepper-value" for="rd-search">{prefs.size}</output>
        <button
          class="rd-icon-btn"
          type="button"
          title="{t('topbar.largerText')} (+)"
          disabled={prefs.size >= MAX_SIZE}
          onclick={() => nudgeSize(1)}
        >
          <i class="ri-add-line" aria-hidden="true"></i>
          <span class="rd-sr">{t('topbar.largerText')}</span>
        </button>
      </div>

      <p class="rd-menu-label" id="rd-density-label">{t('topbar.density')}</p>
      <ButtonGroup
        class="rd-segments"
        label={t('topbar.density')}
        options={densities}
        value={prefs.density}
        on_change={(value: Density | null) => {
          if (value) setDensity(value);
        }}
      />

      <p class="rd-menu-label" id="rd-view-label">{t('topbar.focusMode')}</p>
      <div class="rd-view-toggles" role="group" aria-labelledby="rd-view-label">
        <!-- Visible controls for the two view states the `f` and `b` shortcuts drive; the top
             bar's dock button and the reading bar's focus button only cover part of the screen. -->
        <button
          class="rd-menu-item"
          type="button"
          aria-pressed={prefs.focus}
          title="{t('topbar.focusMode')} (f)"
          onclick={() => toggleFocus()}
        >
          <i class={prefs.focus ? 'ri-fullscreen-exit-line' : 'ri-fullscreen-line'} aria-hidden="true"></i>
          {t('topbar.focusMode')}
          <span class="rd-hint">{t('topbar.focusModeHint')}</span>
        </button>
        <button
          class="rd-menu-item"
          type="button"
          aria-pressed={isNarrow() ? prefs.drawerOpen : prefs.navOpen}
          title="{t('topbar.subscriptionsPanel')} (b)"
          onclick={() => toggleNavForViewport()}
        >
          <i class="ri-layout-left-2-line" aria-hidden="true"></i>
          {t('topbar.subscriptionsPanel')}
        </button>
      </div>

      <label class="rd-menu-label" for="rd-lang">{t('topbar.language')}</label>
      <select
        id="rd-lang"
        class="rd-input"
        data-feeddi-lang
        value={prefs.language}
        onchange={(e) => setLanguage((e.currentTarget.value === 'tr' ? 'tr' : 'en') as 'en' | 'tr')}
      >
        {#each LANGUAGES as language (language.value)}
          <option value={language.value} lang={language.value}>{language.label}</option>
        {/each}
      </select>
    </Popover>

    <button class="rd-icon-btn rd-hide-touch" type="button" title="{t('topbar.shortcuts')} (?)" onclick={onShowShortcuts}>
      <i class="ri-keyboard-box-line" aria-hidden="true"></i>
      <span class="rd-sr">{t('topbar.shortcuts')}</span>
    </button>

    {#if session.user}
      {@const user = session.user}
      <Popover
        bind:open={accountOpen}
        placement="bottom"
        align="start"
        dismiss_on="press"
        class="rd-account"
      >
        {#snippet trigger(props)}
          <button
            class="rd-avatar"
            type="button"
            title={t('topbar.accountMenu')}
            {...props}
            onclick={(event) => {
              props.onclick?.(event);
              openOnly('account');
            }}
          >
            <span aria-hidden="true">{user.initial}</span>
            <span class="rd-sr">{t('topbar.accountMenu')}</span>
          </button>
        {/snippet}
        <p class="rd-menu-user">{user.username}</p>
        <button class="rd-menu-item" type="button" onclick={() => (apiDialogOpen = true)}>
          <i class="ri-key-2-line" aria-hidden="true"></i>
          {t('topbar.apiPassword')}
        </button>
        <button class="rd-menu-item" type="button" onclick={handleSignOut}>
          <i class="ri-logout-box-r-line" aria-hidden="true"></i>
          {t('topbar.signOut')}
        </button>
      </Popover>
    {/if}

    <ApiDialog open={apiDialogOpen} onclose={() => (apiDialogOpen = false)} />
  </div>

  {#if refreshJob.running}
    <div class="rd-refresh-bar rd-bar" aria-hidden="true"></div>
  {/if}
</header>
